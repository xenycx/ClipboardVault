use std::collections::HashMap;

use askama::Template;
use axum::{
    Form,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    AppState, api, auth,
    db::{self, Id},
    error::{AppError, AppResult},
    filters,
    identity::{
        self, ServerSettings,
        admin::{self as accounts, AdminUser},
        keys::{self, ApiKeySummary},
        workspaces::{self, WorkspaceMember},
    },
    models::{AuthContext, ItemCreate, ItemRow, UserSession},
};
use sqlx::{QueryBuilder, Sqlite};

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    registration_enabled: bool,
}

#[derive(Template)]
#[template(path = "pending.html")]
struct PendingTemplate {
    name: String,
    rejected: bool,
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate;

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    session: UserSession,
    items: Vec<ItemRow>,
    max_upload_mb: u64,
    max_upload_bytes: u64,
    nav: &'static str,
    query: String,
    trash: bool,
    chips: Vec<FilterChip>,
    total: i64,
}

/// One type filter in the vault toolbar. Hrefs are built server side so the
/// browser never has to guess how to escape a search term.
struct FilterChip {
    label: String,
    href: String,
    count: i64,
    active: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct VaultQuery {
    q: Option<String>,
    kind: Option<String>,
    trash: Option<String>,
}

#[derive(Template)]
#[template(path = "keys.html")]
struct KeysTemplate {
    session: UserSession,
    nav: &'static str,
    workspace_name: String,
    keys: Vec<ApiKeySummary>,
    members: Vec<WorkspaceMember>,
    max_upload_mb: u64,
    server_max_upload_mb: u64,
    can_manage: bool,
    is_owner: bool,
    server_url: String,
}

struct AdminStats {
    workspaces: i64,
    items: i64,
    trashed: i64,
    stored_bytes: i64,
}

struct Notice {
    error: bool,
    text: String,
}

#[derive(Template)]
#[template(path = "admin.html")]
struct AdminTemplate {
    session: UserSession,
    nav: &'static str,
    users: Vec<AdminUser>,
    pending: usize,
    admins: usize,
    settings: ServerSettings,
    default_upload_mb: u64,
    server_max_upload_mb: u64,
    stats: AdminStats,
    notice: Option<Notice>,
}

#[derive(Template)]
#[template(path = "storage.html")]
pub struct StorageTemplate {
    pub session: UserSession,
    pub nav: &'static str,
    pub workspace_name: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub other_bytes: u64,
    pub reserve_bytes: u64,
    pub workspace_bytes: i64,
    pub reserved_upload_bytes: i64,
    pub reclaimable_bytes: i64,
    pub low_storage: bool,
    pub cleanup_items: Vec<api::StorageCleanupItem>,
}

#[derive(Template)]
#[template(path = "one_time_secret.html")]
struct OneTimeSecretTemplate {
    title: String,
    description: String,
    secret: String,
    back_url: String,
}

#[derive(Template)]
#[template(path = "api_key_created.html")]
struct ApiKeyCreatedTemplate {
    name: String,
    secret: String,
    server_url: String,
    can_read: bool,
    can_delete: bool,
}

#[derive(Template)]
#[template(path = "join.html")]
struct JoinTemplate {
    token: String,
    signed_in: bool,
}

#[derive(Template)]
#[template(path = "reset_password.html")]
struct ResetPasswordTemplate {
    token: String,
}

pub async fn login(State(state): State<AppState>) -> AppResult<Html<String>> {
    let settings = identity::server_settings(&state.db).await;
    render(LoginTemplate {
        registration_enabled: settings.registration_enabled,
    })
}

pub async fn setup(State(state): State<AppState>) -> AppResult<Html<String>> {
    if identity::any_admin_exists(&state.db).await? {
        return Err(AppError::NotFound);
    }
    render(SetupTemplate)
}

pub async fn pending(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let session = match auth::session(&state, &headers).await {
        Ok(session) => session,
        Err(AppError::Unauthorized) => return Ok(Redirect::to("/login").into_response()),
        Err(error) => return Err(error),
    };
    if session.user.approval_status == "approved" {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(render(PendingTemplate {
        name: session.user.name,
        rejected: session.user.approval_status == "rejected",
    })?
    .into_response())
}

pub async fn dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<VaultQuery>,
) -> AppResult<Response> {
    let session = match auth::session(&state, &headers).await {
        Ok(session) => session,
        Err(AppError::Unauthorized) => return Ok(Redirect::to("/login").into_response()),
        Err(error) => return Err(error),
    };
    if session.user.approval_status != "approved" {
        return Ok(Redirect::to("/pending").into_response());
    }
    let organization_id = session
        .active_organization_id
        .clone()
        .ok_or(AppError::Forbidden)?;
    let trash = query
        .trash
        .as_deref()
        .is_some_and(|value| matches!(value, "1" | "true" | "yes"));
    let search = query.q.unwrap_or_default().trim().to_string();
    let kind = query
        .kind
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(16)
        .collect::<String>();
    let kind_filter = (!kind.is_empty() && kind != "all").then(|| kind.clone());

    let mut builder =
        QueryBuilder::<Sqlite>::new("SELECT * FROM vault_items WHERE organization_id = ");
    builder.push_bind(&organization_id);
    builder.push(if trash {
        " AND deleted_at IS NOT NULL"
    } else {
        " AND deleted_at IS NULL"
    });
    if let Some(kind) = kind_filter.as_ref() {
        builder.push(" AND kind = ").push_bind(kind.clone());
    }
    api::push_search(&mut builder, &search);
    builder.push(" ORDER BY pinned DESC, created_at DESC LIMIT 100");
    let items = builder
        .build_query_as::<ItemRow>()
        .fetch_all(&state.db.read)
        .await?;

    let counts = sqlx::query_as::<_, (String, i64)>(if trash {
        "SELECT kind, count(*) FROM vault_items WHERE organization_id = ? AND deleted_at IS NOT NULL GROUP BY kind"
    } else {
        "SELECT kind, count(*) FROM vault_items WHERE organization_id = ? AND deleted_at IS NULL GROUP BY kind"
    })
    .bind(&organization_id)
    .fetch_all(&state.db.read)
    .await?;
    let chips = build_chips(&counts, kind_filter.as_deref(), &search, trash);
    let total = counts.iter().map(|(_, count)| count).sum();

    let limit = api::workspace_limit(&state, &organization_id).await?;
    Ok(render(DashboardTemplate {
        session,
        items,
        max_upload_mb: limit / 1024 / 1024,
        max_upload_bytes: limit,
        nav: if trash { "trash" } else { "vault" },
        query: search,
        trash,
        chips,
        total,
    })?
    .into_response())
}

/// Vault filter link, e.g. `/?trash=1&kind=code&q=deploy`.
fn vault_href(kind: Option<&str>, search: &str, trash: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if trash {
        parts.push("trash=1".to_string());
    }
    if let Some(kind) = kind {
        parts.push(format!("kind={}", urlencoding(kind)));
    }
    if !search.is_empty() {
        parts.push(format!("q={}", urlencoding(search)));
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/?{}", parts.join("&"))
    }
}

/// `All` plus one chip per kind present in the current view, in a stable order.
fn build_chips(
    counts: &[(String, i64)],
    active_kind: Option<&str>,
    search: &str,
    trash: bool,
) -> Vec<FilterChip> {
    const ORDER: [&str; 6] = ["text", "code", "url", "html", "image", "file"];
    let total: i64 = counts.iter().map(|(_, count)| count).sum();
    let mut chips = vec![FilterChip {
        label: "All".to_string(),
        href: vault_href(None, search, trash),
        count: total,
        active: active_kind.is_none(),
    }];
    let mut present: Vec<&(String, i64)> = counts.iter().filter(|(_, count)| *count > 0).collect();
    present.sort_by_key(|(kind, _)| {
        ORDER
            .iter()
            .position(|known| known == kind)
            .unwrap_or(ORDER.len())
    });
    for (kind, count) in present {
        chips.push(FilterChip {
            label: kind.clone(),
            href: vault_href(Some(kind), search, trash),
            count: *count,
            active: active_kind == Some(kind.as_str()),
        });
    }
    chips
}

pub async fn keys(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let session = approved_session(&state, &headers).await?;
    let organization_id = session
        .active_organization_id
        .clone()
        .ok_or(AppError::Forbidden)?;
    let user_id = session.user.id.clone();
    let keys = keys::list(&state.db, &organization_id, &user_id).await?;
    let members = workspaces::members(&state.db, &organization_id, &user_id).await?;
    let effective_limit = api::workspace_limit(&state, &organization_id).await?;
    let workspace_name = session
        .memberships
        .iter()
        .find(|m| m.organization_id == organization_id)
        .map(|m| m.organization_name.clone())
        .unwrap_or_else(|| "Workspace".into());
    let active_role = session.active_role.as_deref().unwrap_or("member");
    let can_manage = matches!(active_role, "owner" | "admin");
    let is_owner = active_role == "owner";
    Ok(render(KeysTemplate {
        session,
        nav: "keys",
        workspace_name,
        keys,
        members,
        max_upload_mb: effective_limit / 1024 / 1024,
        server_max_upload_mb: state.server_max_upload_bytes / 1024 / 1024,
        can_manage,
        is_owner,
        server_url: state.public_base_url.to_string(),
    })?
    .into_response())
}

pub async fn storage_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let session = approved_session(&state, &headers).await?;
    let auth = page_auth_context(&session)?;
    api::require_workspace_manager(&auth)?;
    let details = api::storage_details(&state, &auth).await?;
    let used_bytes = details.total_bytes.saturating_sub(details.free_bytes);
    let other_bytes = used_bytes.saturating_sub(details.workspace_bytes.max(0) as u64);
    Ok(render(StorageTemplate {
        session,
        nav: "storage",
        workspace_name: details.workspace_name,
        free_bytes: details.free_bytes,
        total_bytes: details.total_bytes,
        used_bytes,
        other_bytes,
        reserve_bytes: details.reserve_bytes,
        workspace_bytes: details.workspace_bytes,
        reserved_upload_bytes: details.reserved_upload_bytes,
        reclaimable_bytes: details.reclaimable_bytes,
        low_storage: details.low_storage,
        cleanup_items: details.items,
    })?
    .into_response())
}

pub async fn storage_purge(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Redirect> {
    let session = approved_session(&state, &headers).await?;
    api::require_same_origin(&state, &headers)?;
    let auth = page_auth_context(&session)?;
    let mut item_ids = Vec::new();
    let mut confirmation = String::new();
    for (name, value) in url::form_urlencoded::parse(&body) {
        match name.as_ref() {
            "item_ids" => item_ids.push(
                value
                    .parse::<Uuid>()
                    .map_err(|_| AppError::Validation("invalid cleanup item".into()))?,
            ),
            "confirmation" => confirmation = value.into_owned(),
            _ => {}
        }
    }
    api::purge_items(&state, &auth, item_ids, &confirmation).await?;
    Ok(Redirect::to("/storage"))
}

fn page_auth_context(session: &UserSession) -> AppResult<AuthContext> {
    Ok(AuthContext {
        user_id: Some(session.user.id.clone()),
        api_key_id: None,
        organization_id: session
            .active_organization_id
            .clone()
            .ok_or(AppError::Forbidden)?,
        role: session
            .active_role
            .clone()
            .unwrap_or_else(|| "member".into()),
        is_global_admin: identity::is_admin_role(&session.user.role),
    })
}

pub async fn admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let session = approved_session(&state, &headers).await?;
    require_global_admin(&session)?;
    let notice = query
        .get("notice")
        .and_then(|code| notice_text(code))
        .map(|text| Notice {
            error: false,
            text: text.to_string(),
        });
    render_admin(&state, session, notice).await
}

fn notice_text(code: &str) -> Option<&'static str> {
    Some(match code {
        "settings" => "Server settings saved.",
        "created" => "Account created. It is approved and has its own workspace.",
        "approved" => "Account approved.",
        "rejected" => "Account rejected.",
        "role" => "Global role updated.",
        "banned" => "Account disabled and signed out everywhere.",
        "unbanned" => "Account enabled. The person can sign in again.",
        "sessions" => "Every session for that account was signed out.",
        "deleted" => "Account deleted.",
        _ => return None,
    })
}

async fn render_admin(
    state: &AppState,
    session: UserSession,
    notice: Option<Notice>,
) -> AppResult<Response> {
    let mut users = accounts::list_users(&state.db, &session.user.id).await?;
    // Anything waiting on a decision belongs at the top of the queue.
    users.sort_by_key(|user| match user.approval_status.as_str() {
        "pending" => 0,
        "approved" => 1,
        _ => 2,
    });
    let pending = users
        .iter()
        .filter(|user| user.approval_status == "pending")
        .count();
    let admins = users.iter().filter(|user| user.is_admin()).count();
    let settings = identity::server_settings(&state.db).await;
    let (items, trashed) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT count(*) FILTER (WHERE deleted_at IS NULL), count(*) FILTER (WHERE deleted_at IS NOT NULL) FROM vault_items",
    )
    .fetch_one(&state.db.read)
    .await?;
    let stored_bytes =
        sqlx::query_scalar::<_, i64>("SELECT coalesce(sum(size_bytes), 0) FROM vault_blobs")
            .fetch_one(&state.db.read)
            .await?;
    let workspaces = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM workspaces")
        .fetch_one(&state.db.read)
        .await?;
    let default_upload_mb = (settings.default_max_upload_bytes.max(1) as u64)
        .min(state.server_max_upload_bytes)
        / 1024
        / 1024;
    Ok(render(AdminTemplate {
        session,
        nav: "admin",
        users,
        pending,
        admins,
        settings,
        default_upload_mb,
        server_max_upload_mb: state.server_max_upload_bytes / 1024 / 1024,
        stats: AdminStats {
            workspaces,
            items,
            trashed,
            stored_bytes,
        },
        notice,
    })?
    .into_response())
}

#[derive(Deserialize)]
pub struct ServerSettingsForm {
    #[serde(default)]
    registration_enabled: Option<String>,
    #[serde(default)]
    require_approval: Option<String>,
    default_max_upload_mb: u64,
}

pub async fn update_server_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ServerSettingsForm>,
) -> AppResult<Redirect> {
    let session = approved_session(&state, &headers).await?;
    require_global_admin(&session)?;
    let bytes = form
        .default_max_upload_mb
        .max(1)
        .saturating_mul(1024 * 1024)
        .min(state.server_max_upload_bytes) as i64;
    sqlx::query(
        r#"UPDATE server_settings SET registration_enabled = ?, require_approval = ?,
             default_max_upload_bytes = ?, updated_by_user_id = ?, updated_at = ?
           WHERE id = 1"#,
    )
    .bind(form.registration_enabled.is_some())
    .bind(form.require_approval.is_some())
    .bind(bytes)
    .bind(&session.user.id)
    .bind(db::now())
    .execute(&state.db.write)
    .await?;
    Ok(Redirect::to("/admin?notice=settings"))
}

/// Downloads a consistent snapshot of the database (`VACUUM INTO`). Uploaded
/// files are not included; back up the `uploads` folder alongside it.
pub async fn download_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let session = approved_session(&state, &headers).await?;
    require_global_admin(&session)?;
    crate::backup::download(&state).await
}

/// Shows the outcome of a global-admin command: a notice on success, or the
/// business-rule refusal on the admin page instead of a JSON body.
async fn admin_outcome(
    state: &AppState,
    session: UserSession,
    result: AppResult<()>,
    notice: &str,
) -> AppResult<Response> {
    match result {
        Ok(()) => Ok(Redirect::to(&format!("/admin?notice={notice}")).into_response()),
        Err(AppError::Validation(message)) => {
            render_admin(
                state,
                session,
                Some(Notice {
                    error: true,
                    text: message,
                }),
            )
            .await
        }
        Err(error) => Err(error),
    }
}

async fn admin_session(state: &AppState, headers: &HeaderMap) -> AppResult<UserSession> {
    let session = approved_session(state, headers).await?;
    require_global_admin(&session)?;
    Ok(session)
}

#[derive(Deserialize)]
pub struct CreateUserForm {
    name: String,
    email: String,
    password: String,
    #[serde(default)]
    role: String,
}

pub async fn admin_create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateUserForm>,
) -> AppResult<Response> {
    let session = admin_session(&state, &headers).await?;
    let result = accounts::create_user(
        &state.db,
        &session.user.id,
        &form.name,
        &form.email,
        &form.password,
        form.role == "admin",
    )
    .await
    .map(|_| ());
    admin_outcome(&state, session, result, "created").await
}

#[derive(Deserialize)]
pub struct RoleForm {
    role: String,
}

pub async fn admin_set_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RoleForm>,
) -> AppResult<Response> {
    let session = admin_session(&state, &headers).await?;
    let result = accounts::set_role(&state.db, &session.user.id, &id, form.role == "admin").await;
    admin_outcome(&state, session, result, "role").await
}

pub async fn admin_ban_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let session = admin_session(&state, &headers).await?;
    let result = accounts::set_banned(&state.db, &session.user.id, &id, true).await;
    admin_outcome(&state, session, result, "banned").await
}

pub async fn admin_unban_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let session = admin_session(&state, &headers).await?;
    let result = accounts::set_banned(&state.db, &session.user.id, &id, false).await;
    admin_outcome(&state, session, result, "unbanned").await
}

pub async fn admin_revoke_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let session = admin_session(&state, &headers).await?;
    let result = accounts::revoke_sessions(&state.db, &session.user.id, &id)
        .await
        .map(|_| ());
    admin_outcome(&state, session, result, "sessions").await
}

#[derive(Deserialize)]
pub struct DeleteUserForm {
    confirmation: String,
}

pub async fn admin_delete_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DeleteUserForm>,
) -> AppResult<Response> {
    let session = admin_session(&state, &headers).await?;
    let result = accounts::delete_user(&state.db, &session.user.id, &id, &form.confirmation).await;
    let result = match result {
        Ok(storage_keys) => {
            api::remove_storage_keys(&state, storage_keys).await;
            Ok(())
        }
        Err(error) => Err(error),
    };
    admin_outcome(&state, session, result, "deleted").await
}

pub async fn reset_password(
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Html<String>> {
    render(ResetPasswordTemplate {
        token: query.get("token").cloned().unwrap_or_default(),
    })
}

pub async fn join(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> AppResult<Html<String>> {
    let valid = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM vault_invitations WHERE token_hash = ? AND redeemed_at IS NULL AND expires_at > ?)",
    )
    .bind(token_hash(&token))
    .bind(db::now())
    .fetch_one(&state.db.read)
    .await?;
    if !valid {
        return Err(AppError::NotFound);
    }
    let signed_in = auth::session(&state, &headers)
        .await
        .map(|session| session.user.approval_status == "approved")
        .unwrap_or(false);
    render(JoinTemplate { token, signed_in })
}

#[derive(Deserialize)]
pub struct SwitchForm {
    organization_id: String,
}
pub async fn switch_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SwitchForm>,
) -> AppResult<Response> {
    let session =
        identity::session_from_headers(&state.db, &headers, Some(&form.organization_id)).await?;
    if session.user.approval_status != "approved"
        || session.active_organization_id.as_deref() != Some(&form.organization_id)
    {
        return Err(AppError::Forbidden);
    }
    let cookie = identity::set_cookie(
        identity::WORKSPACE_COOKIE,
        &form.organization_id,
        31_536_000,
        state.secure_cookies,
        "/",
    );
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

#[derive(Deserialize)]
pub struct WorkspaceSettingsForm {
    max_upload_mb: u64,
}
pub async fn update_workspace_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<WorkspaceSettingsForm>,
) -> AppResult<Redirect> {
    let session = approved_session(&state, &headers).await?;
    require_workspace_manager(&session)?;
    let organization_id = session.active_organization_id.ok_or(AppError::Forbidden)?;
    let requested = form.max_upload_mb.max(1).saturating_mul(1024 * 1024);
    let limit = requested.min(state.server_max_upload_bytes) as i64;
    sqlx::query(
        r#"INSERT INTO vault_workspace_settings (organization_id, max_upload_bytes, updated_by_user_id, updated_at)
           VALUES (?, ?, ?, ?)
           ON CONFLICT (organization_id) DO UPDATE SET max_upload_bytes = excluded.max_upload_bytes,
           updated_by_user_id = excluded.updated_by_user_id, updated_at = excluded.updated_at"#,
    )
    .bind(organization_id)
    .bind(limit)
    .bind(session.user.id)
    .bind(db::now())
    .execute(&state.db.write)
    .await?;
    Ok(Redirect::to("/keys"))
}

fn active_workspace(session: &UserSession) -> AppResult<&str> {
    session
        .active_organization_id
        .as_deref()
        .ok_or(AppError::Forbidden)
}

#[derive(Deserialize)]
pub struct MemberRoleForm {
    role: String,
}
pub async fn update_member_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<MemberRoleForm>,
) -> AppResult<Redirect> {
    let session = approved_session(&state, &headers).await?;
    require_workspace_manager(&session)?;
    workspaces::set_member_role(
        &state.db,
        active_workspace(&session)?,
        &session.user.id,
        &id,
        &form.role,
    )
    .await?;
    Ok(Redirect::to("/keys"))
}

pub async fn remove_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Redirect> {
    let session = approved_session(&state, &headers).await?;
    require_workspace_manager(&session)?;
    workspaces::remove_member(
        &state.db,
        active_workspace(&session)?,
        &session.user.id,
        &id,
    )
    .await?;
    Ok(Redirect::to("/keys"))
}

pub async fn transfer_ownership(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Redirect> {
    let session = approved_session(&state, &headers).await?;
    if session.active_role.as_deref() != Some("owner") {
        return Err(AppError::Forbidden);
    }
    workspaces::transfer(
        &state.db,
        active_workspace(&session)?,
        &session.user.id,
        &id,
    )
    .await?;
    Ok(Redirect::to("/keys"))
}

#[derive(Deserialize)]
pub struct DeleteWorkspaceForm {
    confirmation: String,
}
pub async fn delete_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DeleteWorkspaceForm>,
) -> AppResult<Response> {
    let session = approved_session(&state, &headers).await?;
    if session.active_role.as_deref() != Some("owner") {
        return Err(AppError::Forbidden);
    }
    let storage_keys = workspaces::delete(
        &state.db,
        active_workspace(&session)?,
        &session.user.id,
        &form.confirmation,
    )
    .await?;
    api::remove_storage_keys(&state, storage_keys).await;
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        identity::set_cookie(identity::WORKSPACE_COOKIE, "", 0, state.secure_cookies, "/"),
    );
    Ok(response)
}

#[derive(Deserialize)]
pub struct CaptureForm {
    payload: String,
    kind: String,
    virtual_path: String,
    tags: String,
    source_url: Option<String>,
}
pub async fn capture_text(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CaptureForm>,
) -> AppResult<Response> {
    let auth = auth::authorize(&state, &headers, "items:write").await?;
    let input = ItemCreate {
        payload: form.payload,
        kind: form.kind,
        virtual_path: form.virtual_path,
        tags: form
            .tags
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        source_url: form.source_url,
    };
    api::create_text_item(&state, auth, input).await?;
    Ok(Redirect::to("/").into_response())
}

#[derive(Deserialize)]
pub struct KeyForm {
    name: String,
    expires_in_days: i64,
    #[serde(default)]
    read: Option<String>,
    #[serde(default)]
    delete: Option<String>,
}
pub async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<KeyForm>,
) -> AppResult<Html<String>> {
    let session = approved_session(&state, &headers).await?;
    let organization_id = active_workspace(&session)?;
    let mut permissions = vec!["items:write"];
    if form.read.is_some() {
        permissions.push("items:read");
    }
    if form.delete.is_some() {
        permissions.push("items:delete");
    }
    let secret = keys::create(
        &state.db,
        organization_id,
        &session.user.id,
        &form.name,
        form.expires_in_days,
        &permissions,
    )
    .await?;
    render(ApiKeyCreatedTemplate {
        name: form.name.trim().chars().take(80).collect(),
        secret,
        server_url: state.public_base_url.to_string(),
        can_read: form.read.is_some(),
        can_delete: form.delete.is_some(),
    })
}

pub async fn revoke_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Redirect> {
    let session = approved_session(&state, &headers).await?;
    keys::revoke(
        &state.db,
        active_workspace(&session)?,
        &session.user.id,
        &id,
    )
    .await?;
    Ok(Redirect::to("/keys"))
}

#[derive(Deserialize)]
pub struct InviteForm {
    role: String,
}
pub async fn create_invitation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<InviteForm>,
) -> AppResult<Html<String>> {
    let session = approved_session(&state, &headers).await?;
    let organization_id = active_workspace(&session)?.to_owned();
    let role = match form.role.as_str() {
        "admin" => "admin",
        _ => "member",
    };
    let active_role = session.active_role.as_deref().unwrap_or("member");
    if !matches!(active_role, "owner" | "admin") {
        return Err(AppError::Forbidden);
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO vault_invitations (id, organization_id, token_hash, role, created_by_user_id, expires_at, created_at) VALUES (?,?,?,?,?,?,?)",
    )
    .bind(Uuid::new_v4().hyphenated())
    .bind(&organization_id)
    .bind(token_hash(&token))
    .bind(role)
    .bind(&session.user.id)
    .bind(db::ts(now + Duration::hours(48)))
    .bind(db::ts(now))
    .execute(&state.db.write)
    .await?;
    let url = format!("{}/join/{}", state.public_base_url, token);
    render(OneTimeSecretTemplate {
        title: "Invitation created".into(),
        description: "Share this private link. It works once and expires in 48 hours.".into(),
        secret: url,
        back_url: "/keys".into(),
    })
}

pub async fn redeem_invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> AppResult<Redirect> {
    let session = approved_session(&state, &headers).await?;
    let mut tx = state.db.begin().await?;
    let now = db::now();
    let (id, organization_id, role) = sqlx::query_as::<_, (Id, String, String)>(
        "SELECT id, organization_id, role FROM vault_invitations WHERE token_hash = ? AND redeemed_at IS NULL AND expires_at > ?",
    )
    .bind(token_hash(&token))
    .bind(&now)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    workspaces::join(&mut tx, &organization_id, &session.user.id, &role).await?;
    sqlx::query(
        "UPDATE vault_invitations SET redeemed_at = ?, redeemed_by_user_id = ? WHERE id = ?",
    )
    .bind(&now)
    .bind(&session.user.id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Redirect::to("/"))
}

pub async fn approve_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Redirect> {
    let session = admin_session(&state, &headers).await?;
    accounts::approve(&state.db, &session.user.id, &id).await?;
    Ok(Redirect::to("/admin?notice=approved"))
}

pub async fn reject_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let session = admin_session(&state, &headers).await?;
    let result = accounts::reject(&state.db, &session.user.id, &id).await;
    admin_outcome(&state, session, result, "rejected").await
}

pub async fn manual_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let session = admin_session(&state, &headers).await?;
    let url =
        accounts::reset_link(&state.db, &state.public_base_url, &session.user.id, &id).await?;
    render(OneTimeSecretTemplate {
        title: "Password reset link".into(),
        description: format!(
            "Send this private link to the user. It expires in {} minutes and works once.",
            accounts::RESET_LINK_MINUTES
        ),
        secret: url,
        back_url: "/admin".into(),
    })
}

pub async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Html("<!doctype html><title>Not found</title><h1>Not found</h1>"),
    )
}

async fn approved_session(state: &AppState, headers: &HeaderMap) -> AppResult<UserSession> {
    let session = auth::session(state, headers).await?;
    if session.user.approval_status != "approved" {
        return Err(AppError::PendingApproval);
    }
    Ok(session)
}
fn require_global_admin(session: &UserSession) -> AppResult<()> {
    if identity::is_admin_role(&session.user.role) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}
fn require_workspace_manager(session: &UserSession) -> AppResult<()> {
    if matches!(session.active_role.as_deref(), Some("owner" | "admin")) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}
fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}
fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
fn render(template: impl Template) -> AppResult<Html<String>> {
    Ok(Html(
        template
            .render()
            .map_err(|error| AppError::Other(error.into()))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_pages_request_versioned_assets() {
        let html = LoginTemplate {
            registration_enabled: true,
        }
        .render()
        .unwrap();
        for asset in [
            "/static/app.css?v=",
            "/static/app.js?v=",
            "/static/theme.js?v=",
        ] {
            assert!(html.contains(asset), "{asset} is missing from the head");
        }
    }

    #[test]
    fn rendered_pages_carry_no_inline_script() {
        // script-src 'self' means an inline script or handler silently does nothing.
        let html = LoginTemplate {
            registration_enabled: true,
        }
        .render()
        .unwrap();
        assert!(!html.contains("<script>"));
        assert!(!html.contains("onclick="));
        assert!(!html.contains("onchange="));
    }

    #[test]
    fn vault_links_escape_the_search_term() {
        let href = vault_href(Some("code"), "a&b c", true);
        assert_eq!(href, "/?trash=1&kind=code&q=a%26b+c");
        assert_eq!(vault_href(None, "", false), "/");
    }

    #[test]
    fn chips_lead_with_an_all_entry_that_totals_the_view() {
        let counts = vec![("code".to_string(), 3), ("text".to_string(), 5)];
        let chips = build_chips(&counts, Some("code"), "", false);
        assert_eq!(chips[0].label, "All");
        assert_eq!(chips[0].count, 8);
        assert!(!chips[0].active);
        // Fixed display order regardless of how Postgres grouped them.
        assert_eq!(chips[1].label, "text");
        assert_eq!(chips[2].label, "code");
        assert!(chips[2].active);
    }
}
