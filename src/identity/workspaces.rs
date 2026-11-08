//! Workspace membership management, with the rules the 2.x auth service enforced:
//! only owners and admins manage members, nobody changes or removes the owner except
//! by an owner-initiated transfer, and deleting a workspace needs its exact name.

use serde::Serialize;
use sqlx::SqliteConnection;

use crate::{
    db::{self, Db},
    error::{AppError, AppResult},
    identity::{keys::membership_role, new_id},
};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMember {
    pub id: String,
    pub name: String,
    pub email: String,
    pub role: String,
    pub created_at: String,
}

fn is_manager(role: &str) -> bool {
    matches!(role, "owner" | "admin")
}

async fn require_manager(db: &Db, requester_id: &str, workspace_id: &str) -> AppResult<String> {
    let role = membership_role(db, requester_id, workspace_id)
        .await?
        .ok_or(AppError::Forbidden)?;
    if !is_manager(&role) {
        return Err(AppError::Forbidden);
    }
    Ok(role)
}

/// Members of a workspace. Owners and admins see everyone; members see themselves.
pub async fn members(
    db: &Db,
    workspace_id: &str,
    requester_id: &str,
) -> AppResult<Vec<WorkspaceMember>> {
    let role = membership_role(db, requester_id, workspace_id)
        .await?
        .ok_or(AppError::Forbidden)?;
    let mut members = sqlx::query_as::<_, WorkspaceMember>(
        r#"SELECT u.id, u.name, u.email, m.role, m.created_at
           FROM memberships m JOIN users u ON u.id = m.user_id
           WHERE m.workspace_id = ?
           ORDER BY CASE m.role WHEN 'owner' THEN 0 WHEN 'admin' THEN 1 ELSE 2 END, m.created_at"#,
    )
    .bind(workspace_id)
    .fetch_all(&db.read)
    .await?;
    if !is_manager(&role) {
        members.retain(|member| member.id == requester_id);
    }
    Ok(members)
}

/// Adds an approved user to a workspace (invitation redemption). Idempotent.
pub async fn join(
    connection: &mut SqliteConnection,
    workspace_id: &str,
    user_id: &str,
    role: &str,
) -> AppResult<()> {
    let approved =
        sqlx::query_scalar::<_, String>("SELECT approval_status FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *connection)
            .await?;
    if approved.as_deref() != Some("approved") {
        return Err(AppError::Forbidden);
    }
    let role = if role == "admin" { "admin" } else { "member" };
    sqlx::query(
        "INSERT INTO memberships (id, workspace_id, user_id, role, created_at) VALUES (?,?,?,?,?) ON CONFLICT (workspace_id, user_id) DO NOTHING",
    )
    .bind(new_id())
    .bind(workspace_id)
    .bind(user_id)
    .bind(role)
    .bind(db::now())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

pub async fn set_member_role(
    db: &Db,
    workspace_id: &str,
    requester_id: &str,
    user_id: &str,
    role: &str,
) -> AppResult<()> {
    require_manager(db, requester_id, workspace_id).await?;
    if !matches!(role, "member" | "admin") {
        return Err(AppError::Validation("Invalid role".into()));
    }
    let target = membership_role(db, user_id, workspace_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if target == "owner" {
        return Err(AppError::Forbidden);
    }
    sqlx::query("UPDATE memberships SET role = ? WHERE workspace_id = ? AND user_id = ? AND role <> 'owner'")
        .bind(role)
        .bind(workspace_id)
        .bind(user_id)
        .execute(&db.write)
        .await?;
    Ok(())
}

pub async fn remove_member(
    db: &Db,
    workspace_id: &str,
    requester_id: &str,
    user_id: &str,
) -> AppResult<()> {
    require_manager(db, requester_id, workspace_id).await?;
    let target = membership_role(db, user_id, workspace_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if target == "owner" {
        return Err(AppError::Forbidden);
    }
    sqlx::query(
        "DELETE FROM memberships WHERE workspace_id = ? AND user_id = ? AND role <> 'owner'",
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(&db.write)
    .await?;
    Ok(())
}

/// The owner hands the workspace to another member and becomes an admin.
pub async fn transfer(
    db: &Db,
    workspace_id: &str,
    requester_id: &str,
    user_id: &str,
) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let requester = sqlx::query_scalar::<_, String>(
        "SELECT role FROM memberships WHERE workspace_id = ? AND user_id = ?",
    )
    .bind(workspace_id)
    .bind(requester_id)
    .fetch_optional(&mut *tx)
    .await?;
    if requester.as_deref() != Some("owner") {
        return Err(AppError::Forbidden);
    }
    let target_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE workspace_id = ? AND user_id = ?)",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if requester_id == user_id || !target_exists {
        return Err(AppError::Validation("Invalid new owner".into()));
    }
    sqlx::query("UPDATE memberships SET role = 'admin' WHERE workspace_id = ? AND user_id = ?")
        .bind(workspace_id)
        .bind(requester_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE memberships SET role = 'owner' WHERE workspace_id = ? AND user_id = ?")
        .bind(workspace_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Deletes a workspace and everything in it. Returns the storage keys (blobs and
/// partial uploads) to unlink once the deletion is committed.
pub async fn delete(
    db: &Db,
    workspace_id: &str,
    requester_id: &str,
    confirmation: &str,
) -> AppResult<Vec<String>> {
    let mut tx = db.begin().await?;
    let requester = sqlx::query_scalar::<_, String>(
        "SELECT role FROM memberships WHERE workspace_id = ? AND user_id = ?",
    )
    .bind(workspace_id)
    .bind(requester_id)
    .fetch_optional(&mut *tx)
    .await?;
    if requester.as_deref() != Some("owner") {
        return Err(AppError::Forbidden);
    }
    let name = sqlx::query_scalar::<_, String>("SELECT name FROM workspaces WHERE id = ?")
        .bind(workspace_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    if confirmation != name {
        return Err(AppError::Validation("Workspace name does not match".into()));
    }
    let keys = storage_keys(&mut tx, &[workspace_id.to_owned()]).await?;
    sqlx::query("DELETE FROM workspaces WHERE id = ?")
        .bind(workspace_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(keys)
}

/// Blob and partial-upload files that belong to the given workspaces.
pub async fn storage_keys(
    connection: &mut SqliteConnection,
    workspace_ids: &[String],
) -> AppResult<Vec<String>> {
    let ids = serde_json::to_string(workspace_ids).expect("strings serialize");
    Ok(sqlx::query_scalar::<_, String>(
        r#"SELECT storage_key FROM vault_blobs WHERE organization_id IN (SELECT value FROM json_each(?1))
           UNION ALL
           SELECT temp_storage_key FROM vault_upload_sessions WHERE organization_id IN (SELECT value FROM json_each(?1))"#,
    )
    .bind(ids)
    .fetch_all(&mut *connection)
    .await?)
}
