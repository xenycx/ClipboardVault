//! Native identity: users, sessions, workspaces, memberships and API keys.
//!
//! This replaces the 2.x Node.js auth service (Hono + Better Auth) and its private
//! `/internal/*` bridge. Every former bridge call is now a function in this module
//! tree, so there is no shared secret and no second process.
//!
//! Security properties carried over from Better Auth:
//! * Session cookies hold 32 random bytes; the database stores only their SHA-256.
//! * Sessions last 7 days and slide forward at most once a day ([`refresh_session`]).
//! * Passwords are Argon2id; legacy scrypt hashes verify and are replaced on sign-in.
//! * API keys are stored as SHA-256 (base64url), so 2.x keys keep working.
//! * State-changing `/api/auth/*` requests must carry this server's `Origin`.

pub mod admin;
pub mod keys;
pub mod oauth;
pub mod password;
pub mod rate_limit;
pub mod routes;
pub mod workspaces;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, header},
    middleware::Next,
    response::Response,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use rand::{Rng, RngCore, distributions::Alphanumeric};
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;
use unicode_normalization::UnicodeNormalization;

use crate::{
    AppState,
    db::{self, Db},
    error::{AppError, AppResult},
    models::{Membership, SessionUser, UserSession},
};

pub const SESSION_COOKIE: &str = "vault_session";
pub const WORKSPACE_COOKIE: &str = "vault_workspace";
pub const SESSION_DAYS: i64 = 7;
/// A session is extended when it was last extended more than this long ago.
pub const SESSION_UPDATE_AGE_HOURS: i64 = 24;

/// URL-safe random token with `bytes` bytes of entropy.
pub fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

/// Random 32-character alphanumeric identifier, the same shape Better Auth used.
pub fn new_id() -> String {
    rand::rngs::OsRng
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

pub fn sha256_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

pub fn constant_time_eq(left: &str, right: &str) -> bool {
    use subtle::ConstantTimeEq;
    left.len() == right.len() && bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

pub fn is_admin_role(role: &str) -> bool {
    role.split(',').any(|part| part.trim() == "admin")
}

/// SQL predicate matching users whose comma-separated role list contains `admin`.
pub const ADMIN_ROLE_SQL: &str =
    "(',' || replace(coalesce(role, ''), ' ', '') || ',') LIKE '%,admin,%'";

pub fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|cookies| cookies.split(';'))
        .find_map(|cookie| {
            let (key, value) = cookie.trim().split_once('=')?;
            (key == name && !value.is_empty()).then_some(value)
        })
}

pub fn set_cookie(
    name: &str,
    value: &str,
    max_age_seconds: i64,
    secure: bool,
    path: &str,
) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path={path}; HttpOnly; SameSite=Lax; Max-Age={max_age_seconds}{secure}"
    ))
    .expect("cookie values are URL-safe")
}

pub fn session_cookie(token: &str, secure: bool) -> HeaderValue {
    set_cookie(SESSION_COOKIE, token, SESSION_DAYS * 86_400, secure, "/")
}

pub fn clear_session_cookie(secure: bool) -> HeaderValue {
    set_cookie(SESSION_COOKIE, "", 0, secure, "/")
}

/// Server-wide switches owned by global administrators.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ServerSettings {
    pub registration_enabled: bool,
    pub require_approval: bool,
    pub default_max_upload_bytes: i64,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            registration_enabled: true,
            require_approval: true,
            default_max_upload_bytes: 10_737_418_240,
        }
    }
}

pub async fn server_settings(db: &Db) -> ServerSettings {
    sqlx::query_as::<_, ServerSettings>(
        "SELECT registration_enabled, require_approval, default_max_upload_bytes FROM server_settings WHERE id = 1",
    )
    .fetch_optional(&db.read)
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

pub async fn any_admin_exists(db: &Db) -> AppResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(&format!(
        "SELECT EXISTS(SELECT 1 FROM users WHERE {ADMIN_ROLE_SQL})"
    ))
    .fetch_one(&db.read)
    .await?)
}

/// Creates a session for `user_id` and returns the cookie token.
pub async fn create_session(
    connection: &mut SqliteConnection,
    user_id: &str,
    ip_address: Option<&str>,
    user_agent: Option<&str>,
) -> AppResult<String> {
    let token = random_token(32);
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO sessions (id, token_hash, user_id, expires_at, created_at, updated_at, ip_address, user_agent) VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(new_id())
    .bind(sha256_hex(&token))
    .bind(user_id)
    .bind(db::ts(now + Duration::days(SESSION_DAYS)))
    .bind(db::ts(now))
    .bind(db::ts(now))
    .bind(ip_address)
    .bind(user_agent.map(|agent| agent.chars().take(512).collect::<String>()))
    .execute(&mut *connection)
    .await?;
    // Expired rows are only clutter; clear them while the write lock is held anyway.
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(db::ts(now))
        .execute(&mut *connection)
        .await?;
    Ok(token)
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: String,
    name: String,
    email: String,
    image: Option<String>,
    role: String,
    approval_status: String,
    banned: bool,
}

/// Resolves the browser session from the `vault_session` cookie.
///
/// `requested_workspace` (normally the `vault_workspace` cookie) selects the active
/// workspace only if the user is a member of it; otherwise the oldest membership is
/// active. A caller can never name a workspace it does not belong to.
pub async fn session_from_headers(
    db: &Db,
    headers: &HeaderMap,
    requested_workspace: Option<&str>,
) -> AppResult<UserSession> {
    let token = cookie_value(headers, SESSION_COOKIE).ok_or(AppError::Unauthorized)?;
    let user_id = sqlx::query_scalar::<_, String>(
        "SELECT user_id FROM sessions WHERE token_hash = ? AND expires_at > ?",
    )
    .bind(sha256_hex(token))
    .bind(db::now())
    .fetch_optional(&db.read)
    .await?
    .ok_or(AppError::Unauthorized)?;
    let mut user = sqlx::query_as::<_, UserRow>(
        "SELECT id, name, email, image, role, approval_status, banned FROM users WHERE id = ?",
    )
    .bind(&user_id)
    .fetch_optional(&db.read)
    .await?
    .ok_or(AppError::Unauthorized)?;
    if user.banned {
        return Err(AppError::Unauthorized);
    }
    if user.approval_status == "pending" && !server_settings(db).await.require_approval {
        // Approval was switched off by an administrator: let waiting accounts in.
        approve_user(db, &user.id, None).await?;
        user.approval_status = "approved".into();
    }
    let memberships = sqlx::query_as::<_, Membership>(
        r#"SELECT m.workspace_id AS organization_id, w.name AS organization_name,
                  w.slug AS organization_slug, m.role
           FROM memberships m JOIN workspaces w ON w.id = m.workspace_id
           WHERE m.user_id = ? ORDER BY m.created_at, m.rowid"#,
    )
    .bind(&user.id)
    .fetch_all(&db.read)
    .await?;
    let active = requested_workspace
        .and_then(|requested| {
            memberships
                .iter()
                .find(|membership| membership.organization_id == requested)
        })
        .or_else(|| memberships.first());
    Ok(UserSession {
        active_organization_id: active.map(|membership| membership.organization_id.clone()),
        active_role: active.map(|membership| membership.role.clone()),
        user: SessionUser {
            id: user.id,
            name: user.name,
            email: user.email,
            image: user.image,
            role: user.role,
            approval_status: user.approval_status,
        },
        memberships,
    })
}

/// Marks a user approved and gives them a personal workspace if they have none.
pub async fn approve_user(db: &Db, user_id: &str, approved_by: Option<&str>) -> AppResult<bool> {
    let mut tx = db.begin().await?;
    let approved = approve_user_in(&mut tx, user_id, approved_by).await?;
    tx.commit().await?;
    Ok(approved)
}

pub async fn approve_user_in(
    connection: &mut SqliteConnection,
    user_id: &str,
    approved_by: Option<&str>,
) -> AppResult<bool> {
    let now = db::now();
    let name = sqlx::query_scalar::<_, String>(
        "UPDATE users SET approval_status = 'approved', approved_at = ?, approved_by = ?, updated_at = ? WHERE id = ? RETURNING name",
    )
    .bind(&now)
    .bind(approved_by)
    .bind(&now)
    .bind(user_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(name) = name else {
        return Ok(false);
    };
    ensure_personal_workspace(connection, user_id, &name).await?;
    Ok(true)
}

/// Every approved account starts with a workspace it owns.
pub async fn ensure_personal_workspace(
    connection: &mut SqliteConnection,
    user_id: &str,
    name: &str,
) -> AppResult<()> {
    let has_membership =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM memberships WHERE user_id = ?)")
            .bind(user_id)
            .fetch_one(&mut *connection)
            .await?;
    if has_membership {
        return Ok(());
    }
    let workspace_id = new_id();
    let mut slug = format!("{}-{}", slug_stem(name), user_prefix(user_id));
    let taken =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM workspaces WHERE slug = ?)")
            .bind(&slug)
            .fetch_one(&mut *connection)
            .await?;
    if taken {
        slug = format!("{slug}-{}", new_id()[..6].to_ascii_lowercase());
    }
    let now = db::now();
    sqlx::query("INSERT INTO workspaces (id, name, slug, created_at) VALUES (?,?,?,?)")
        .bind(&workspace_id)
        .bind(format!("{name}'s Vault"))
        .bind(&slug)
        .bind(&now)
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "INSERT INTO memberships (id, workspace_id, user_id, role, created_at) VALUES (?,?,?,'owner',?)",
    )
    .bind(new_id())
    .bind(&workspace_id)
    .bind(user_id)
    .bind(&now)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

fn user_prefix(user_id: &str) -> String {
    user_id.chars().take(8).collect::<String>().to_lowercase()
}

/// `Ana María` → `ana-maria`, as the 2.x auth service built workspace slugs.
pub fn slug_stem(name: &str) -> String {
    let mut slug = String::new();
    for character in name.to_lowercase().nfkd() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !character.is_ascii() && unicode_normalization::char::is_combining_mark(character)
        {
            continue;
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "workspace".into()
    } else {
        slug.into()
    }
}

/// Slides a session's expiry forward once a day and re-issues its cookie.
///
/// Runs for every non-asset request that carries a session cookie. Requests under
/// `/api/auth/` set or clear the cookie themselves and are left alone.
pub async fn refresh_session(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let candidate = (!path.starts_with("/static/") && !path.starts_with("/api/auth/"))
        .then(|| cookie_value(request.headers(), SESSION_COOKIE).map(str::to_owned))
        .flatten();
    let mut response = next.run(request).await;
    let Some(token) = candidate else {
        return response;
    };
    let hash = sha256_hex(&token);
    let updated = sqlx::query_scalar::<_, String>(
        "SELECT updated_at FROM sessions WHERE token_hash = ? AND expires_at > ?",
    )
    .bind(&hash)
    .bind(db::now())
    .fetch_optional(&state.db.read)
    .await;
    let Ok(Some(updated_at)) = updated else {
        return response;
    };
    let due = db::parse_ts(&updated_at)
        .is_none_or(|updated| Utc::now() - updated > Duration::hours(SESSION_UPDATE_AGE_HOURS));
    if !due {
        return response;
    }
    let now = Utc::now();
    let result =
        sqlx::query("UPDATE sessions SET expires_at = ?, updated_at = ? WHERE token_hash = ?")
            .bind(db::ts(now + Duration::days(SESSION_DAYS)))
            .bind(db::ts(now))
            .bind(&hash)
            .execute(&state.db.write)
            .await;
    if result.is_ok() {
        response.headers_mut().append(
            header::SET_COOKIE,
            session_cookie(&token, state.secure_cookies),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_match_the_previous_auth_service() {
        assert_eq!(slug_stem("Ana María"), "ana-maria");
        assert_eq!(slug_stem("  Big   Team!! "), "big-team");
        assert_eq!(slug_stem("ქართული"), "workspace");
    }

    #[test]
    fn identifiers_and_tokens_have_the_expected_shape() {
        let id = new_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
        let token = random_token(32);
        assert_eq!(token.len(), 43);
        assert_ne!(token, random_token(32));
    }

    #[test]
    fn cookies_are_parsed_by_exact_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "xvault_session=bad; vault_session=good; vault_workspace=ws"
                .parse()
                .unwrap(),
        );
        assert_eq!(cookie_value(&headers, SESSION_COOKIE), Some("good"));
        assert_eq!(cookie_value(&headers, WORKSPACE_COOKIE), Some("ws"));
        assert_eq!(cookie_value(&headers, "missing"), None);
    }

    #[test]
    fn session_cookies_are_http_only_and_lax() {
        let cookie = session_cookie("abc", true);
        let cookie = cookie.to_str().unwrap();
        assert!(cookie.starts_with("vault_session=abc;"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Secure"));
        assert!(
            !session_cookie("abc", false)
                .to_str()
                .unwrap()
                .contains("Secure")
        );
    }

    #[test]
    fn admin_role_lists_are_understood() {
        assert!(is_admin_role("admin"));
        assert!(is_admin_role("user, admin"));
        assert!(!is_admin_role("user"));
        assert!(!is_admin_role("administrator"));
    }
}
