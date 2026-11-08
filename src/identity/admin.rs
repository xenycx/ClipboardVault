//! Global administration. Every function checks that the requester is an approved
//! administrator, and keeps the 2.x safety rules: nobody disables, demotes or
//! deletes themselves; administrators must be demoted before they are disabled,
//! rejected or deleted; and deleting an account needs its exact email address.

use chrono::{Duration, Utc};
use serde::Serialize;

use crate::{
    db::{self, Db},
    error::{AppError, AppResult},
    identity::{
        ADMIN_ROLE_SQL, approve_user, approve_user_in, is_admin_role, new_id, password,
        random_token, sha256_hex, workspaces::storage_keys,
    },
};

/// Password reset links stay valid for 15 minutes, as Better Auth's did.
pub const RESET_LINK_MINUTES: i64 = 15;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AdminUser {
    pub id: String,
    pub name: String,
    pub email: String,
    pub role: String,
    pub approval_status: String,
    pub banned: bool,
    pub created_at: String,
}

impl AdminUser {
    pub fn is_admin(&self) -> bool {
        is_admin_role(&self.role)
    }

    pub fn joined(&self) -> String {
        self.created_at
            .get(..10)
            .unwrap_or(&self.created_at)
            .to_string()
    }
}

fn refuse(message: &str) -> AppError {
    AppError::Validation(message.into())
}

pub async fn require_admin(db: &Db, requester_id: &str) -> AppResult<()> {
    let row = sqlx::query_as::<_, (String, String, bool)>(
        "SELECT role, approval_status, banned FROM users WHERE id = ?",
    )
    .bind(requester_id)
    .fetch_optional(&db.read)
    .await?;
    match row {
        Some((role, status, false)) if status == "approved" && is_admin_role(&role) => Ok(()),
        _ => Err(AppError::Forbidden),
    }
}

async fn target_role(db: &Db, user_id: &str) -> AppResult<(String, bool)> {
    sqlx::query_as::<_, (String, bool)>("SELECT role, banned FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&db.read)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn list_users(db: &Db, requester_id: &str) -> AppResult<Vec<AdminUser>> {
    require_admin(db, requester_id).await?;
    Ok(sqlx::query_as::<_, AdminUser>(
        "SELECT id, name, email, role, approval_status, banned, created_at FROM users ORDER BY created_at DESC LIMIT 1000",
    )
    .fetch_all(&db.read)
    .await?)
}

pub async fn approve(db: &Db, requester_id: &str, user_id: &str) -> AppResult<()> {
    require_admin(db, requester_id).await?;
    if !approve_user(db, user_id, Some(requester_id)).await? {
        return Err(AppError::NotFound);
    }
    Ok(())
}

pub async fn reject(db: &Db, requester_id: &str, user_id: &str) -> AppResult<()> {
    require_admin(db, requester_id).await?;
    let (role, _) = target_role(db, user_id).await?;
    if is_admin_role(&role) {
        return Err(refuse("Global administrators cannot be rejected"));
    }
    sqlx::query(
        "UPDATE users SET approval_status = 'rejected', approved_at = NULL, approved_by = ?, updated_at = ? WHERE id = ?",
    )
    .bind(requester_id)
    .bind(db::now())
    .bind(user_id)
    .execute(&db.write)
    .await?;
    Ok(())
}

pub fn email_is_valid(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.chars().any(char::is_whitespace)
        && email.len() <= 254
}

/// Creates an approved account with its own workspace. Returns the new user ID.
pub async fn create_user(
    db: &Db,
    requester_id: &str,
    name: &str,
    email: &str,
    password_value: &str,
    make_admin: bool,
) -> AppResult<String> {
    require_admin(db, requester_id).await?;
    let name: String = name.trim().chars().take(80).collect();
    let email = email.trim().to_lowercase();
    if name.is_empty() {
        return Err(refuse("Enter a name for the account."));
    }
    if !email_is_valid(&email) {
        return Err(refuse("Enter a valid email address."));
    }
    if !password::length_is_valid(password_value) {
        return Err(refuse("Passwords must be 8 to 128 characters."));
    }
    let hash = password::hash_blocking(password_value.to_owned()).await?;
    let mut tx = db.begin().await?;
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE email = ?)")
            .bind(&email)
            .fetch_one(&mut *tx)
            .await?;
    if exists {
        return Err(refuse("An account with that email already exists."));
    }
    let id = new_id();
    let now = db::now();
    sqlx::query(
        r#"INSERT INTO users (id, name, email, password_hash, role, approval_status, created_at, updated_at)
           VALUES (?,?,?,?,?,'pending',?,?)"#,
    )
    .bind(&id)
    .bind(&name)
    .bind(&email)
    .bind(&hash)
    .bind(if make_admin { "admin" } else { "user" })
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    approve_user_in(&mut tx, &id, Some(requester_id)).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn set_role(
    db: &Db,
    requester_id: &str,
    user_id: &str,
    make_admin: bool,
) -> AppResult<()> {
    require_admin(db, requester_id).await?;
    let (_, banned) = target_role(db, user_id).await?;
    if !make_admin && user_id == requester_id {
        return Err(refuse("You cannot remove your own administrator role."));
    }
    if make_admin && banned {
        return Err(refuse(
            "Enable the account before making it an administrator.",
        ));
    }
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE users SET role = ?, updated_at = ? WHERE id = ?")
        .bind(if make_admin { "admin" } else { "user" })
        .bind(db::now())
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if make_admin {
        approve_user_in(&mut tx, user_id, Some(requester_id)).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn set_banned(db: &Db, requester_id: &str, user_id: &str, banned: bool) -> AppResult<()> {
    require_admin(db, requester_id).await?;
    let (role, _) = target_role(db, user_id).await?;
    if banned && user_id == requester_id {
        return Err(refuse("You cannot disable your own account."));
    }
    if banned && is_admin_role(&role) {
        return Err(refuse(
            "Remove the administrator role before disabling this account.",
        ));
    }
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE users SET banned = ?, ban_reason = ?, updated_at = ? WHERE id = ?")
        .bind(banned)
        .bind(banned.then_some("Disabled by an administrator"))
        .bind(db::now())
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if banned {
        sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn revoke_sessions(db: &Db, requester_id: &str, user_id: &str) -> AppResult<u64> {
    require_admin(db, requester_id).await?;
    Ok(sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(user_id)
        .execute(&db.write)
        .await?
        .rows_affected())
}

/// Deletes an account and the workspaces only it belongs to. Returns the storage
/// keys of the files those workspaces held, to unlink after the commit.
pub async fn delete_user(
    db: &Db,
    requester_id: &str,
    user_id: &str,
    confirmation: &str,
) -> AppResult<Vec<String>> {
    require_admin(db, requester_id).await?;
    if user_id == requester_id {
        return Err(refuse("You cannot delete your own account."));
    }
    let mut tx = db.begin().await?;
    let (email, role) =
        sqlx::query_as::<_, (String, String)>("SELECT email, role FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
    if is_admin_role(&role) {
        return Err(refuse(
            "Remove the administrator role before deleting this account.",
        ));
    }
    if confirmation.trim().to_lowercase() != email.to_lowercase() {
        return Err(refuse(
            "Type the account's email address exactly to confirm deletion.",
        ));
    }
    let owned = sqlx::query_as::<_, (String, String, i64)>(
        r#"SELECT w.id, w.name,
                  (SELECT count(*) FROM memberships other
                   WHERE other.workspace_id = w.id AND other.user_id <> ?1) AS others
           FROM memberships m JOIN workspaces w ON w.id = m.workspace_id
           WHERE m.user_id = ?1 AND m.role = 'owner'"#,
    )
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?;
    if let Some((_, name, _)) = owned.iter().find(|(_, _, others)| *others > 0) {
        return Err(AppError::Validation(format!(
            "This person owns \"{name}\", which has other members. Transfer that workspace first."
        )));
    }
    let workspace_ids: Vec<String> = owned.into_iter().map(|(id, _, _)| id).collect();
    let keys = storage_keys(&mut tx, &workspace_ids).await?;
    sqlx::query("DELETE FROM workspaces WHERE id IN (SELECT value FROM json_each(?))")
        .bind(serde_json::to_string(&workspace_ids).expect("strings serialize"))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(keys)
}

/// Issues a single-use reset link for an account and returns its URL.
pub async fn reset_link(
    db: &Db,
    public_base_url: &str,
    requester_id: &str,
    user_id: &str,
) -> AppResult<String> {
    require_admin(db, requester_id).await?;
    target_role(db, user_id).await?;
    let token = random_token(32);
    let now = Utc::now();
    sqlx::query("INSERT INTO password_resets (token_hash, user_id, expires_at, created_at) VALUES (?,?,?,?)")
        .bind(sha256_hex(&token))
        .bind(user_id)
        .bind(db::ts(now + Duration::minutes(RESET_LINK_MINUTES)))
        .bind(db::ts(now))
        .execute(&db.write)
        .await?;
    Ok(format!("{public_base_url}/reset-password?token={token}"))
}

/// Number of global administrators, used by the setup page and the bootstrap claim.
pub async fn admin_count(db: &Db) -> AppResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(&format!(
        "SELECT count(*) FROM users WHERE {ADMIN_ROLE_SQL}"
    ))
    .fetch_one(&db.read)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::email_is_valid;

    #[test]
    fn email_validation_matches_the_admin_form() {
        assert!(email_is_valid("ana@example.com"));
        assert!(!email_is_valid("ana@example"));
        assert!(!email_is_valid("ana example@x.com"));
        assert!(!email_is_valid("@example.com"));
        assert!(!email_is_valid("a@b@c.com"));
    }
}
