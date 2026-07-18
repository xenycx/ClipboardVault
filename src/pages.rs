use std::collections::HashMap;

use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    AppState, api,
    auth::workspace_cookie,
    error::{AppError, AppResult},
    models::{BridgeSession, ItemCreate, ItemRow},
};

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate;

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
    session: BridgeSession,
    active_workspace_name: String,
    items: Vec<ItemRow>,
    max_upload_mb: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiKeySummary {
    id: String,
    name: String,
    start: String,
    #[allow(dead_code)]
    created_by_user_id: Option<String>,
    permissions: Vec<String>,
    expires_at: Option<String>,
    #[allow(dead_code)]
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct KeyList {
    keys: Vec<ApiKeySummary>,
}

#[derive(Template)]
#[template(path = "keys.html")]
struct KeysTemplate {
    workspace_name: String,
    keys: Vec<ApiKeySummary>,
    members: Vec<WorkspaceMember>,
    max_upload_mb: u64,
    can_manage: bool,
    is_owner: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceMember {
    id: String,
    name: String,
    email: String,
    role: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDetails {
    members: Vec<WorkspaceMember>,
    max_upload_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdminUser {
    id: String,
    name: String,
    email: String,
    approval_status: String,
    role: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct AdminUsers {
    users: Vec<AdminUser>,
}

#[derive(Template)]
#[template(path = "admin.html")]
struct AdminTemplate {
    users: Vec<AdminUser>,
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

#[derive(Debug, Deserialize)]
struct SecretResult {
    secret: String,
}

#[derive(Debug, Deserialize)]
struct UrlResult {
    url: String,
}

#[derive(Debug, Deserialize)]
struct OkResult {
    #[allow(dead_code)]
    ok: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteWorkspaceResult {
    #[allow(dead_code)]
    ok: bool,
    storage_keys: Vec<String>,
}

pub async fn login() -> AppResult<Html<String>> {
    render(LoginTemplate)
}
pub async fn setup(State(state): State<AppState>) -> AppResult<Html<String>> {
    let configured = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM \"user\" WHERE string_to_array(coalesce(role,''), ',') @> ARRAY['admin'])",
    )
    .fetch_one(&state.pool)
    .await?;
    if configured {
        return Err(AppError::NotFound);
    }
    render(SetupTemplate)
}

pub async fn pending(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let session = match state
        .auth
        .session(&headers, workspace_cookie(&headers))
        .await
    {
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

pub async fn dashboard(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let session = match state
        .auth
        .session(&headers, workspace_cookie(&headers))
        .await
    {
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
    let workspace_name = session
        .memberships
        .iter()
        .find(|m| m.organization_id == organization_id)
        .map(|m| m.organization_name.clone())
        .unwrap_or_else(|| "Workspace".into());
    let items = sqlx::query_as::<_, ItemRow>(
        "SELECT * FROM vault_items WHERE organization_id = $1 AND deleted_at IS NULL ORDER BY pinned DESC, created_at DESC LIMIT 100",
    ).bind(&organization_id).fetch_all(&state.pool).await?;
    let limit = sqlx::query_scalar::<_, i64>(
        "SELECT max_upload_bytes FROM vault_workspace_settings WHERE organization_id = $1",
    )
    .bind(&organization_id)
    .fetch_optional(&state.pool)
    .await?
    .unwrap_or(26_214_400) as u64;
    Ok(render(DashboardTemplate {
        session,
        active_workspace_name: workspace_name,
        items,
        max_upload_mb: limit.min(state.server_max_upload_bytes) / 1024 / 1024,
    })?
    .into_response())
}

pub async fn keys(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let session = approved_session(&state, &headers).await?;
    let organization_id = session
        .active_organization_id
        .clone()
        .ok_or(AppError::Forbidden)?;
    let user_id = session.user.id.clone();
    let path = format!(
        "/internal/api-keys?organizationId={}&requesterId={}",
        urlencoding(&organization_id),
        urlencoding(&user_id)
    );
    let list: KeyList = state.auth.get_json(&path, &[]).await?;
    let details_path = format!(
        "/internal/workspaces/details?organizationId={}&requesterId={}",
        urlencoding(&organization_id),
        urlencoding(&user_id)
    );
    let details: WorkspaceDetails = state.auth.get_json(&details_path, &[]).await?;
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
        workspace_name,
        keys: list.keys,
        members: details.members,
        max_upload_mb: details.max_upload_bytes.min(state.server_max_upload_bytes) / 1024 / 1024,
        can_manage,
        is_owner,
    })?
    .into_response())
}

pub async fn admin(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let session = approved_session(&state, &headers).await?;
    require_global_admin(&session)?;
    let path = format!(
        "/internal/admin/users?requesterId={}",
        urlencoding(&session.user.id)
    );
    let users: AdminUsers = state.auth.get_json(&path, &[]).await?;
    Ok(render(AdminTemplate { users: users.users })?.into_response())
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
    let hash = token_hash(&token);
    let valid = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM vault_invitations WHERE token_hash = $1 AND redeemed_at IS NULL AND expires_at > now())",
    ).bind(hash).fetch_one(&state.pool).await?;
    if !valid {
        return Err(AppError::NotFound);
    }
    let signed_in = state
        .auth
        .session(&headers, workspace_cookie(&headers))
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
    let session = state
        .auth
        .session(&headers, Some(&form.organization_id))
        .await?;
    if session.user.approval_status != "approved"
        || session.active_organization_id.as_deref() != Some(&form.organization_id)
    {
        return Err(AppError::Forbidden);
    }
    let secure = if state.public_base_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "vault_workspace={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000{}",
        form.organization_id, secure
    );
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie.parse().map_err(|_| AppError::Forbidden)?,
    );
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
        r#"INSERT INTO vault_workspace_settings (organization_id, max_upload_bytes, updated_by_user_id)
           VALUES ($1, $2, $3)
           ON CONFLICT (organization_id) DO UPDATE SET max_upload_bytes = EXCLUDED.max_upload_bytes,
           updated_by_user_id = EXCLUDED.updated_by_user_id, updated_at = now()"#,
    )
    .bind(organization_id)
    .bind(limit)
    .bind(session.user.id)
    .execute(&state.pool)
    .await?;
    Ok(Redirect::to("/keys"))
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
    let body = workspace_member_body(&session, &id, Some(&form.role))?;
    let _: OkResult = state
        .auth
        .post_json("/internal/workspaces/member-role", &body, &[])
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
    let body = workspace_member_body(&session, &id, None)?;
    let _: OkResult = state
        .auth
        .post_json("/internal/workspaces/remove-member", &body, &[])
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
    let body = workspace_member_body(&session, &id, None)?;
    let _: OkResult = state
        .auth
        .post_json("/internal/workspaces/transfer", &body, &[])
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
    let organization_id = session.active_organization_id.ok_or(AppError::Forbidden)?;
    let body = json!({
        "organizationId": organization_id,
        "requesterId": &session.user.id,
        "confirmation": form.confirmation,
    });
    let result: DeleteWorkspaceResult = state
        .auth
        .post_json("/internal/workspaces/delete", &body, &[])
        .await?;
    for storage_key in result.storage_keys {
        let path = std::path::Path::new(&storage_key);
        if path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            if let Err(error) = tokio::fs::remove_file(state.upload_root.join(path)).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(%error, %storage_key, "failed to remove deleted workspace file");
                }
            }
        }
    }
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        "vault_workspace=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"
            .parse()
            .map_err(|_| AppError::Forbidden)?,
    );
    Ok(response)
}

fn workspace_member_body(
    session: &BridgeSession,
    user_id: &str,
    role: Option<&str>,
) -> AppResult<serde_json::Value> {
    let organization_id = session
        .active_organization_id
        .as_deref()
        .ok_or(AppError::Forbidden)?;
    let mut body = json!({
        "organizationId": organization_id,
        "requesterId": &session.user.id,
        "userId": user_id,
    });
    if let Some(role) = role {
        body["role"] = json!(role);
    }
    Ok(body)
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
    let auth = crate::auth::authorize(&state.auth, &headers, "items:write").await?;
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
    let organization_id = session
        .active_organization_id
        .clone()
        .ok_or(AppError::Forbidden)?;
    let mut permissions = vec!["items:write"];
    if form.read.is_some() {
        permissions.push("items:read");
    }
    if form.delete.is_some() {
        permissions.push("items:delete");
    }
    let body = json!({
        "organizationId": organization_id, "requesterId": session.user.id,
        "name": form.name, "expiresInDays": form.expires_in_days.clamp(1, 3650), "permissions": permissions,
    });
    let result: SecretResult = state
        .auth
        .post_json("/internal/api-keys", &body, &[])
        .await?;
    render(OneTimeSecretTemplate {
        title: "API key created".into(),
        description: "Copy this key now. It will never be shown again.".into(),
        secret: result.secret,
        back_url: "/keys".into(),
    })
}

pub async fn revoke_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Redirect> {
    let session = approved_session(&state, &headers).await?;
    let organization_id = session.active_organization_id.ok_or(AppError::Forbidden)?;
    let path = format!(
        "/internal/api-keys/{}?organizationId={}&requesterId={}",
        urlencoding(&id),
        urlencoding(&organization_id),
        urlencoding(&session.user.id)
    );
    let _: OkResult = state.auth.delete_json(&path, &[]).await?;
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
    let organization_id = session.active_organization_id.ok_or(AppError::Forbidden)?;
    let role = match form.role.as_str() {
        "admin" => "admin",
        _ => "member",
    };
    let active_role = session.active_role.as_deref().unwrap_or("member");
    if !matches!(active_role, "owner" | "admin") {
        return Err(AppError::Forbidden);
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO vault_invitations (organization_id, token_hash, role, created_by_user_id, expires_at) VALUES ($1,$2,$3,$4,$5)",
    ).bind(&organization_id).bind(token_hash(&token)).bind(role).bind(&session.user.id)
        .bind(Utc::now() + Duration::hours(48)).execute(&state.pool).await?;
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
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT id, organization_id, role FROM vault_invitations WHERE token_hash = $1 AND redeemed_at IS NULL AND expires_at > now() FOR UPDATE",
    ).bind(token_hash(&token)).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
    let body = json!({"organizationId": row.1, "userId": session.user.id, "role": row.2});
    let _: OkResult = state
        .auth
        .post_json("/internal/workspaces/join", &body, &[])
        .await?;
    sqlx::query(
        "UPDATE vault_invitations SET redeemed_at = now(), redeemed_by_user_id = $2 WHERE id = $1",
    )
    .bind(row.0)
    .bind(&session.user.id)
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
    admin_mutation(&state, &headers, &id, "approve").await?;
    Ok(Redirect::to("/admin"))
}
pub async fn reject_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Redirect> {
    admin_mutation(&state, &headers, &id, "reject").await?;
    Ok(Redirect::to("/admin"))
}
async fn admin_mutation(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    action: &str,
) -> AppResult<()> {
    let session = approved_session(state, headers).await?;
    require_global_admin(&session)?;
    let body = json!({"requesterId": session.user.id, "userId": id});
    let _: OkResult = state
        .auth
        .post_json(&format!("/internal/admin/{action}"), &body, &[])
        .await?;
    Ok(())
}

pub async fn manual_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let session = approved_session(&state, &headers).await?;
    require_global_admin(&session)?;
    let body = json!({"requesterId": session.user.id, "userId": id});
    let result: UrlResult = state
        .auth
        .post_json("/internal/admin/reset-link", &body, &[])
        .await?;
    render(OneTimeSecretTemplate {
        title: "Password reset link".into(),
        description: "Send this private link to the user. It expires shortly and works once."
            .into(),
        secret: result.url,
        back_url: "/admin".into(),
    })
}

pub async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Html("<!doctype html><title>Not found</title><h1>Not found</h1>"),
    )
}

async fn approved_session(state: &AppState, headers: &HeaderMap) -> AppResult<BridgeSession> {
    let session = state
        .auth
        .session(headers, workspace_cookie(headers))
        .await?;
    if session.user.approval_status != "approved" {
        return Err(AppError::PendingApproval);
    }
    Ok(session)
}
fn require_global_admin(session: &BridgeSession) -> AppResult<()> {
    if session.user.role.split(',').any(|role| role == "admin") {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}
fn require_workspace_manager(session: &BridgeSession) -> AppResult<()> {
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
