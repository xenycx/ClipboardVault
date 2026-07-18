use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeUser {
    pub id: String,
    pub name: String,
    pub email: String,
    pub image: Option<String>,
    pub role: String,
    pub approval_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Membership {
    pub organization_id: String,
    pub organization_name: String,
    pub organization_slug: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeSession {
    pub user: BridgeUser,
    pub active_organization_id: Option<String>,
    pub active_role: Option<String>,
    #[serde(default)]
    pub memberships: Vec<Membership>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedApiKey {
    pub id: String,
    pub organization_id: String,
    pub created_by_user_id: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub organization_id: String,
    pub role: String,
    pub is_global_admin: bool,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct ItemRow {
    pub id: Uuid,
    pub organization_id: String,
    pub created_by_user_id: Option<String>,
    pub created_by_key_id: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_payload: Option<String>,
    pub blob_id: Option<Uuid>,
    pub original_filename: Option<String>,
    pub virtual_path: String,
    pub source_url: Option<String>,
    pub tags: serde_json::Value,
    pub content_hash: String,
    pub size_bytes: i64,
    pub pinned: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow)]
pub struct BlobRow {
    pub id: Uuid,
    pub storage_key: String,
    pub size_bytes: i64,
    pub mime_type: String,
}

#[derive(Debug, Deserialize)]
pub struct ItemCreate {
    pub payload: String,
    #[serde(rename = "type", default = "default_auto")]
    pub kind: String,
    #[serde(default = "default_path")]
    pub virtual_path: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source_url: Option<String>,
}

fn default_auto() -> String {
    "auto".into()
}
fn default_path() -> String {
    "/".into()
}

#[derive(Debug, Deserialize)]
pub struct ItemPatch {
    pub virtual_path: Option<String>,
    pub tags: Option<Vec<String>>,
    pub source_url: Option<Option<String>>,
    pub pinned: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub q: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub before: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
    pub trash: Option<bool>,
    pub tag: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ItemListResponse {
    pub items: Vec<ItemRow>,
    pub next_cursor: Option<DateTime<Utc>>,
}
