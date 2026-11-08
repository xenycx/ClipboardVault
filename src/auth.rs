//! Request authentication for vault routes.
//!
//! An API key (`X-API-Key` or `Authorization: Bearer`) wins when present; its row
//! decides the workspace and permissions. Otherwise the browser session decides,
//! with the active workspace taken from the `vault_workspace` cookie only when the
//! user belongs to it.

use axum::http::HeaderMap;

use crate::{
    AppState,
    error::{AppError, AppResult},
    identity::{self, WORKSPACE_COOKIE, cookie_value, keys},
    models::{AuthContext, UserSession},
};

pub fn api_key_from_headers(headers: &HeaderMap) -> Option<String> {
    keys::from_headers(headers)
}

pub fn workspace_cookie(headers: &HeaderMap) -> Option<&str> {
    cookie_value(headers, WORKSPACE_COOKIE)
}

/// The signed-in session, with the active workspace from the workspace cookie.
pub async fn session(state: &AppState, headers: &HeaderMap) -> AppResult<UserSession> {
    identity::session_from_headers(&state.db, headers, workspace_cookie(headers)).await
}

pub async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    permission: &str,
) -> AppResult<AuthContext> {
    if let Some(key) = api_key_from_headers(headers) {
        let verified = keys::verify(state, &key, permission).await?;
        return Ok(AuthContext {
            user_id: verified.created_by_user_id,
            api_key_id: Some(verified.id),
            organization_id: verified.organization_id,
            role: "api_key".into(),
            is_global_admin: false,
        });
    }

    let session = session(state, headers).await?;
    if session.user.approval_status != "approved" {
        return Err(AppError::PendingApproval);
    }
    let organization_id = session.active_organization_id.ok_or(AppError::Forbidden)?;
    Ok(AuthContext {
        user_id: Some(session.user.id),
        api_key_id: None,
        organization_id,
        role: session.active_role.unwrap_or_else(|| "member".into()),
        is_global_admin: identity::is_admin_role(&session.user.role),
    })
}
