//! Workspace API keys.
//!
//! A key is `cv_live_` followed by 64 random letters. Only SHA-256 of the whole key
//! (base64url, no padding) is stored, exactly as Better Auth stored it, so every key
//! issued by 2.x still verifies. The organization a request acts on comes from the
//! key row, never from the request.

use axum::http::HeaderMap;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use rand::{Rng, distributions::Uniform};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{
    AppState,
    db::{self, Db},
    error::{AppError, AppResult},
    identity::{new_id, rate_limit},
    models::VerifiedApiKey,
};

pub const PREFIX: &str = "cv_live_";
const RANDOM_LENGTH: usize = 64;
const VISIBLE_PREFIX: usize = 12;
const ALLOWED_ACTIONS: [&str; 3] = ["read", "write", "delete"];

pub fn hash_key(key: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(key.as_bytes()))
}

fn generate() -> String {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut rng = rand::rngs::OsRng;
    let index = Uniform::from(0..LETTERS.len());
    let random: String = (0..RANDOM_LENGTH)
        .map(|_| LETTERS[rng.sample(index)] as char)
        .collect();
    format!("{PREFIX}{random}")
}

/// `{"items":["write","read"]}` → `["items:write", "items:read"]`.
pub fn permission_list(stored: &str) -> Vec<String> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(stored) else {
        return Vec::new();
    };
    map.iter()
        .flat_map(|(resource, actions)| {
            actions
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(move |action| format!("{resource}:{action}"))
        })
        .collect()
}

fn grants(stored: &str, permission: &str) -> bool {
    permission_list(stored)
        .iter()
        .any(|granted| granted == permission)
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeySummary {
    pub id: String,
    pub name: String,
    pub start: String,
    pub created_by_user_id: Option<String>,
    #[serde(skip)]
    pub permissions_json: String,
    #[sqlx(skip)]
    pub permissions: Vec<String>,
    pub expires_at: Option<String>,
    pub created_at: String,
}

pub async fn membership_role(
    db: &Db,
    user_id: &str,
    workspace_id: &str,
) -> AppResult<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT role FROM memberships WHERE user_id = ? AND workspace_id = ?",
    )
    .bind(user_id)
    .bind(workspace_id)
    .fetch_optional(&db.read)
    .await?)
}

fn is_manager(role: &str) -> bool {
    matches!(role, "owner" | "admin")
}

/// Keys in a workspace. Owners and admins see all of them; members see their own.
pub async fn list(
    db: &Db,
    workspace_id: &str,
    requester_id: &str,
) -> AppResult<Vec<ApiKeySummary>> {
    let role = membership_role(db, requester_id, workspace_id)
        .await?
        .ok_or(AppError::Forbidden)?;
    let mut keys = sqlx::query_as::<_, ApiKeySummary>(
        r#"SELECT id, coalesce(name, 'API key') AS name, coalesce(start, 'cv_live_') AS start,
                  created_by_user_id, permissions AS permissions_json, expires_at, created_at
           FROM api_keys WHERE workspace_id = ? ORDER BY created_at DESC"#,
    )
    .bind(workspace_id)
    .fetch_all(&db.read)
    .await?;
    keys.retain(|key| is_manager(&role) || key.created_by_user_id.as_deref() == Some(requester_id));
    for key in &mut keys {
        key.permissions = permission_list(&key.permissions_json);
    }
    Ok(keys)
}

/// Creates a key and returns the full secret, which is shown once and never stored.
pub async fn create(
    db: &Db,
    workspace_id: &str,
    requester_id: &str,
    name: &str,
    expires_in_days: i64,
    permissions: &[&str],
) -> AppResult<String> {
    membership_role(db, requester_id, workspace_id)
        .await?
        .ok_or(AppError::Forbidden)?;
    let mut actions: Vec<&str> = permissions
        .iter()
        .filter_map(|permission| permission.strip_prefix("items:"))
        .filter(|action| ALLOWED_ACTIONS.contains(action))
        .collect();
    if !actions.contains(&"write") {
        actions.push("write");
    }
    actions.dedup();
    let mut grants = Map::new();
    grants.insert("items".into(), json!(actions));
    let name = match name.trim() {
        "" => "API key".to_string(),
        trimmed => trimmed.chars().take(80).collect(),
    };
    let secret = generate();
    let now = Utc::now();
    let expires_at = now + Duration::days(expires_in_days.clamp(1, 3650));
    sqlx::query(
        r#"INSERT INTO api_keys
           (id, workspace_id, created_by_user_id, name, start, key_hash, permissions, enabled,
            expires_at, created_at, updated_at)
           VALUES (?,?,?,?,?,?,?,1,?,?,?)"#,
    )
    .bind(new_id())
    .bind(workspace_id)
    .bind(requester_id)
    .bind(&name)
    .bind(&secret[..VISIBLE_PREFIX])
    .bind(hash_key(&secret))
    .bind(Value::Object(grants).to_string())
    .bind(db::ts(expires_at))
    .bind(db::ts(now))
    .bind(db::ts(now))
    .execute(&db.write)
    .await?;
    Ok(secret)
}

/// Owners and admins may revoke any key in the workspace; members only their own.
pub async fn revoke(
    db: &Db,
    workspace_id: &str,
    requester_id: &str,
    key_id: &str,
) -> AppResult<()> {
    let role = membership_role(db, requester_id, workspace_id)
        .await?
        .ok_or(AppError::Forbidden)?;
    let creator = sqlx::query_scalar::<_, Option<String>>(
        "SELECT created_by_user_id FROM api_keys WHERE id = ? AND workspace_id = ?",
    )
    .bind(key_id)
    .bind(workspace_id)
    .fetch_optional(&db.read)
    .await?
    .ok_or(AppError::NotFound)?;
    if !is_manager(&role) && creator.as_deref() != Some(requester_id) {
        return Err(AppError::Forbidden);
    }
    sqlx::query("DELETE FROM api_keys WHERE id = ? AND workspace_id = ?")
        .bind(key_id)
        .bind(workspace_id)
        .execute(&db.write)
        .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct KeyRow {
    id: String,
    workspace_id: String,
    created_by_user_id: Option<String>,
    permissions: String,
    enabled: bool,
    expires_at: Option<String>,
}

/// Verifies a presented key for one permission such as `items:write`.
pub async fn verify(state: &AppState, key: &str, permission: &str) -> AppResult<VerifiedApiKey> {
    if key.len() > 512 {
        return Err(AppError::Unauthorized);
    }
    let row = sqlx::query_as::<_, KeyRow>(
        "SELECT id, workspace_id, created_by_user_id, permissions, enabled, expires_at FROM api_keys WHERE key_hash = ?",
    )
    .bind(hash_key(key))
    .fetch_optional(&state.db.read)
    .await?
    .ok_or(AppError::Unauthorized)?;
    let now = db::now();
    if !row.enabled
        || row
            .expires_at
            .as_deref()
            .is_some_and(|expires| expires <= now.as_str())
    {
        return Err(AppError::Unauthorized);
    }
    if !grants(&row.permissions, permission) {
        return Err(AppError::Unauthorized);
    }
    if !state.limiter.allow("api-key", &row.id, rate_limit::API_KEY) {
        return Err(AppError::RateLimited);
    }
    let db = state.db.clone();
    let id = row.id.clone();
    tokio::spawn(async move {
        let _ = sqlx::query(
            "UPDATE api_keys SET request_count = request_count + 1, last_request_at = ? WHERE id = ?",
        )
        .bind(db::now())
        .bind(id)
        .execute(&db.write)
        .await;
    });
    Ok(VerifiedApiKey {
        permissions: permission_list(&row.permissions),
        id: row.id,
        organization_id: row.workspace_id,
        created_by_user_id: row.created_by_user_id,
    })
}

/// The key a request presents in `X-API-Key` or `Authorization: Bearer`.
pub fn from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        && !value.trim().is_empty()
    {
        return Some(value.trim().to_owned());
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_better_auth() {
        // Computed with node:crypto exactly as @better-auth/api-key's defaultKeyHasher.
        let key = "cv_live_AbCdEfGhIjKlMnOpQrStUvWxYzAbCdEfGhIjKlMnOpQrStUvWxYzAbCdEfGhIjKl";
        assert_eq!(hash_key(key), "hrxplUbqtwmqu4OzxpwbjjtfCauGcy60-AcJVJzpz78");
    }

    #[test]
    fn generated_keys_keep_the_published_format() {
        let key = generate();
        assert!(key.starts_with(PREFIX));
        assert_eq!(key.len(), PREFIX.len() + RANDOM_LENGTH);
        assert!(key[PREFIX.len()..].chars().all(|c| c.is_ascii_alphabetic()));
        assert_ne!(key, generate());
    }

    #[test]
    fn permissions_follow_the_stored_resource_map() {
        let stored = r#"{"items":["write","read"]}"#;
        assert_eq!(permission_list(stored), ["items:write", "items:read"]);
        assert!(grants(stored, "items:read"));
        assert!(!grants(stored, "items:delete"));
        assert!(permission_list("not json").is_empty());
        assert!(!grants("null", "items:read"));
    }
}
