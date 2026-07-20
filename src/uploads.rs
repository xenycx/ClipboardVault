use std::{collections::HashMap, path::Path, time::Duration};

use axum::{
    Json,
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

use crate::{
    AppState,
    auth::authorize,
    error::{AppError, AppResult},
    models::ItemRow,
    storage::{
        TempUpload, file_kind, normalize_virtual_path, persist_blob, safe_filename, sniff_mime,
        validate_tags,
    },
};

const TUS_VERSION: &str = "1.0.0";
const DEFAULT_WORKSPACE_LIMIT: u64 = 104_857_600;

#[derive(Debug, Clone, FromRow)]
struct UploadSession {
    id: Uuid,
    organization_id: String,
    created_by_user_id: Option<String>,
    created_by_key_id: Option<String>,
    state: String,
    temp_storage_key: String,
    original_filename: String,
    virtual_path: String,
    source_url: Option<String>,
    tags: Value,
    declared_mime: Option<String>,
    expected_bytes: i64,
    acknowledged_bytes: i64,
    item_id: Option<Uuid>,
    error_code: Option<String>,
    expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadStatus {
    id: Uuid,
    state: String,
    upload_offset: i64,
    upload_length: i64,
    item_id: Option<Uuid>,
    error: Option<String>,
    expires_at: DateTime<Utc>,
}

impl From<UploadSession> for UploadStatus {
    fn from(row: UploadSession) -> Self {
        Self {
            id: row.id,
            state: row.state,
            upload_offset: row.acknowledged_bytes,
            upload_length: row.expected_bytes,
            item_id: row.item_id,
            error: row.error_code.filter(|value| value != "PROCESSING"),
            expires_at: row.expires_at,
        }
    }
}

pub async fn upload_options(State(state): State<AppState>) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    headers.insert("tus-version", HeaderValue::from_static(TUS_VERSION));
    headers.insert(
        "tus-extension",
        HeaderValue::from_static("creation,expiration,termination"),
    );
    headers.insert(
        "tus-max-size",
        HeaderValue::from_str(&state.server_max_upload_bytes.to_string()).unwrap(),
    );
    headers.insert(
        "x-tus-chunk-size",
        HeaderValue::from_str(&state.tus_chunk_size_bytes.to_string()).unwrap(),
    );
    response
}

pub async fn create_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Response> {
    require_tus(&headers)?;
    let auth = authorize(&state.auth, &headers, "items:write").await?;
    let length = required_u64_header(&headers, "upload-length")?;
    if length == 0 {
        return Err(AppError::Validation(
            "Upload-Length must be greater than zero".into(),
        ));
    }
    let workspace_limit = workspace_limit(&state, &auth.organization_id).await?;
    let limit = workspace_limit.min(state.server_max_upload_bytes);
    if length > limit {
        return Err(AppError::PayloadTooLarge {
            received: length,
            limit,
        });
    }
    let metadata_header = headers
        .get("upload-metadata")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if metadata_header.len() > 8192 {
        return Err(AppError::Validation("Upload-Metadata is too large".into()));
    }
    let metadata = parse_metadata(metadata_header)?;
    let filename = metadata
        .get("filename")
        .map(|value| safe_filename(value))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "upload.bin".into());
    let virtual_path = normalize_virtual_path(
        metadata
            .get("virtualPath")
            .or_else(|| metadata.get("path"))
            .map(String::as_str)
            .unwrap_or("/"),
    )?;
    let tags = metadata
        .get("tags")
        .map(|raw| {
            serde_json::from_str::<Vec<String>>(raw).unwrap_or_else(|_| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
        })
        .unwrap_or_default();
    let tags = validate_tags(tags)?;
    let source_url = validate_source_url(metadata.get("sourceUrl").cloned())?;
    let declared_mime = metadata
        .get("contentType")
        .filter(|value| value.len() <= 255 && !value.contains(['\r', '\n']))
        .cloned();

    let mut tx = state.pool.begin().await?;
    // Serialize capacity admission so simultaneous creates cannot over-reserve disk.
    sqlx::query("SELECT pg_advisory_xact_lock(192837465)")
        .execute(&mut *tx)
        .await?;
    let outstanding = sqlx::query_scalar::<_, i64>(
        "SELECT least(coalesce(sum(expected_bytes - acknowledged_bytes), 0), 9223372036854775807)::bigint FROM vault_upload_sessions WHERE state = 'uploading' AND expires_at > now()",
    )
    .fetch_one(&mut *tx)
    .await?
    .max(0) as u64;
    let available = available_space(&state.upload_root)?;
    let required = state
        .upload_disk_reserve_bytes
        .saturating_add(outstanding)
        .saturating_add(length);
    if available < required {
        return Err(AppError::InsufficientStorage {
            available,
            required,
        });
    }

    let id = Uuid::new_v4();
    let temp_storage_key = format!("tmp/{id}.part");
    let expires_at = Utc::now()
        + chrono::Duration::seconds(state.tus_session_ttl_seconds.min(i64::MAX as u64) as i64);
    sqlx::query(
        r#"INSERT INTO vault_upload_sessions
           (id, organization_id, created_by_user_id, created_by_key_id, temp_storage_key,
            original_filename, virtual_path, source_url, tags, declared_mime, expected_bytes,
            expires_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)"#,
    )
    .bind(id)
    .bind(&auth.organization_id)
    .bind(&auth.user_id)
    .bind(&auth.api_key_id)
    .bind(&temp_storage_key)
    .bind(&filename)
    .bind(&virtual_path)
    .bind(&source_url)
    .bind(json!(tags))
    .bind(&declared_mime)
    .bind(length as i64)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    let temp_path = state.upload_root.join(&temp_storage_key);
    if let Err(error) = tokio::fs::File::create(&temp_path).await {
        tx.rollback().await?;
        return Err(error.into());
    }
    tx.commit().await?;

    let location = format!("{}/api/v1/uploads/{id}", state.public_base_url);
    let mut response = StatusCode::CREATED.into_response();
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(&location)
            .map_err(|_| AppError::Validation("invalid public base URL".into()))?,
    );
    add_tus_headers(&mut response, 0, length, Some(expires_at));
    Ok(response)
}

pub async fn head_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
) -> AppResult<Response> {
    require_tus(&headers)?;
    let auth = authorize(&state.auth, &headers, "items:write").await?;
    let row = fetch_session(&state, id, &auth.organization_id).await?;
    if matches!(row.state.as_str(), "failed" | "canceled" | "expired") {
        let mut response = StatusCode::GONE.into_response();
        response
            .headers_mut()
            .insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
        return Ok(response);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    add_tus_headers(
        &mut response,
        row.acknowledged_bytes.max(0) as u64,
        row.expected_bytes.max(0) as u64,
        Some(row.expires_at),
    );
    response.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

pub async fn patch_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
    body: Bytes,
) -> AppResult<Response> {
    require_tus(&headers)?;
    let auth = authorize(&state.auth, &headers, "items:write").await?;
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some("application/offset+octet-stream")
    {
        return Err(AppError::Validation(
            "PATCH Content-Type must be application/offset+octet-stream".into(),
        ));
    }
    let offset = required_u64_header(&headers, "upload-offset")?;
    let declared_length = required_u64_header(&headers, header::CONTENT_LENGTH.as_str())?;
    if declared_length != body.len() as u64 {
        return Err(AppError::Validation(
            "Content-Length does not match the chunk".into(),
        ));
    }
    if body.is_empty() || body.len() as u64 > state.tus_chunk_size_bytes {
        return Err(AppError::PayloadTooLarge {
            received: body.len() as u64,
            limit: state.tus_chunk_size_bytes,
        });
    }

    let mut tx = state.pool.begin().await?;
    let row = sqlx::query_as::<_, UploadSession>(
        "SELECT * FROM vault_upload_sessions WHERE id = $1 AND organization_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(&auth.organization_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if row.state != "uploading" || row.expires_at <= Utc::now() {
        return Err(AppError::Validation("upload is not active".into()));
    }
    let server_offset = row.acknowledged_bytes.max(0) as u64;
    if offset != server_offset {
        return Err(AppError::OffsetConflict {
            expected: server_offset,
        });
    }
    let next_offset = offset.saturating_add(body.len() as u64);
    if next_offset > row.expected_bytes as u64 {
        return Err(AppError::PayloadTooLarge {
            received: next_offset,
            limit: row.expected_bytes as u64,
        });
    }
    let outstanding = sqlx::query_scalar::<_, i64>(
        "SELECT least(coalesce(sum(expected_bytes - acknowledged_bytes), 0), 9223372036854775807)::bigint FROM vault_upload_sessions WHERE state = 'uploading' AND expires_at > now()",
    )
    .fetch_one(&mut *tx)
    .await?
    .max(0) as u64;
    let available = available_space(&state.upload_root)?;
    let required = state.upload_disk_reserve_bytes.saturating_add(outstanding);
    if available < required {
        return Err(AppError::InsufficientStorage {
            available,
            required,
        });
    }

    let path = safe_storage_path(&state, &row.temp_storage_key)?;
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .await?;
    let actual_length = file.metadata().await?.len();
    if actual_length < server_offset {
        sqlx::query("UPDATE vault_upload_sessions SET state='failed', error_code='PARTIAL_FILE_SHORT', updated_at=now() WHERE id=$1")
            .bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Err(AppError::Validation(
            "partial upload is shorter than its confirmed offset".into(),
        ));
    }
    if actual_length > server_offset {
        file.set_len(server_offset).await?;
    }
    file.seek(std::io::SeekFrom::Start(server_offset)).await?;
    file.write_all(&body).await?;
    file.sync_data().await?;
    let state_value = if next_offset == row.expected_bytes as u64 {
        "finalizing"
    } else {
        "uploading"
    };
    let expires_at = Utc::now()
        + chrono::Duration::seconds(state.tus_session_ttl_seconds.min(i64::MAX as u64) as i64);
    sqlx::query(
        "UPDATE vault_upload_sessions SET acknowledged_bytes=$2, state=$3, expires_at=$4, updated_at=now() WHERE id=$1",
    )
    .bind(id)
    .bind(next_offset as i64)
    .bind(state_value)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut response = StatusCode::NO_CONTENT.into_response();
    add_tus_headers(
        &mut response,
        next_offset,
        row.expected_bytes as u64,
        Some(expires_at),
    );
    Ok(response)
}

pub async fn cancel_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
) -> AppResult<Response> {
    require_tus(&headers)?;
    let auth = authorize(&state.auth, &headers, "items:write").await?;
    let key = sqlx::query_scalar::<_, String>(
        r#"UPDATE vault_upload_sessions SET state='canceled', updated_at=now()
           WHERE id=$1 AND organization_id=$2 AND state IN ('uploading','failed')
           RETURNING temp_storage_key"#,
    )
    .bind(id)
    .bind(&auth.organization_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::NotFound)?;
    if let Ok(path) = safe_storage_path(&state, &key) {
        let _ = tokio::fs::remove_file(path).await;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
    Ok(response)
}

pub async fn upload_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
) -> AppResult<Json<Value>> {
    let auth = authorize(&state.auth, &headers, "items:write").await?;
    let row = fetch_session(&state, id, &auth.organization_id).await?;
    Ok(Json(serde_json::to_value(UploadStatus::from(row)).unwrap()))
}

pub async fn maintenance_worker(state: AppState) {
    loop {
        if let Err(error) = maintenance_tick(&state).await {
            tracing::error!(%error, "large-upload maintenance failed");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn maintenance_tick(state: &AppState) -> AppResult<()> {
    let expired = sqlx::query_as::<_, (Uuid, String)>(
        r#"UPDATE vault_upload_sessions SET state='expired', updated_at=now()
           WHERE state='uploading' AND expires_at <= now()
           RETURNING id, temp_storage_key"#,
    )
    .fetch_all(&state.pool)
    .await?;
    for (_, key) in expired {
        if let Ok(path) = safe_storage_path(state, &key) {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    let stale = sqlx::query_as::<_, (Uuid, String)>(
        r#"DELETE FROM vault_upload_sessions
           WHERE state IN ('failed','canceled','expired','completed') AND expires_at <= now()
           RETURNING id, temp_storage_key"#,
    )
    .fetch_all(&state.pool)
    .await?;
    for (_, key) in stale {
        if let Ok(path) = safe_storage_path(state, &key) {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    let candidate = sqlx::query_as::<_, UploadSession>(
        r#"UPDATE vault_upload_sessions SET error_code='PROCESSING', updated_at=now()
           WHERE id = (
             SELECT id FROM vault_upload_sessions
             WHERE state='finalizing' AND (error_code IS NULL OR updated_at < now() - interval '5 minutes')
             ORDER BY updated_at FOR UPDATE SKIP LOCKED LIMIT 1
           ) RETURNING *"#,
    )
    .fetch_optional(&state.pool)
    .await?;
    if let Some(session) = candidate
        && let Err(error) = finalize_upload(state, &session).await
    {
        tracing::error!(upload_id=%session.id, %error, "upload finalization failed");
        sqlx::query("UPDATE vault_upload_sessions SET state='failed', error_code='FINALIZATION_FAILED', updated_at=now() WHERE id=$1")
            .bind(session.id).execute(&state.pool).await?;
    }
    Ok(())
}

async fn finalize_upload(state: &AppState, session: &UploadSession) -> AppResult<()> {
    // The durable advisory lease prevents another instance from finalizing this
    // upload even if hashing a very large file exceeds the claim timeout.
    let mut lease = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(session.id.to_string())
        .execute(&mut *lease)
        .await?;
    let current_state =
        sqlx::query_scalar::<_, String>("SELECT state FROM vault_upload_sessions WHERE id=$1")
            .bind(session.id)
            .fetch_optional(&mut *lease)
            .await?;
    if current_state.as_deref() != Some("finalizing") {
        lease.commit().await?;
        return Ok(());
    }
    let path = safe_storage_path(state, &session.temp_storage_key)?;
    let mut file = tokio::fs::File::open(&path).await?;
    let actual = file.metadata().await?.len();
    if actual != session.expected_bytes as u64 {
        return Err(AppError::Validation(
            "completed upload length is invalid".into(),
        ));
    }
    let mut hasher = Sha256::new();
    let mut signature = Vec::with_capacity(512);
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if signature.len() < 512 {
            let take = (512 - signature.len()).min(read);
            signature.extend_from_slice(&buffer[..take]);
        }
        hasher.update(&buffer[..read]);
    }
    let sha256 = hex::encode(hasher.finalize());
    let mime_type = sniff_mime(
        &signature,
        &session.original_filename,
        session.declared_mime.as_deref(),
    );
    let upload = TempUpload {
        path,
        original_filename: session.original_filename.clone(),
        mime_type,
        size_bytes: actual,
        sha256,
    };
    let mut tx = state.pool.begin().await?;
    let (blob, blob_created) = persist_blob(
        &mut tx,
        &state.pool,
        &state.upload_root,
        &session.organization_id,
        &upload,
    )
    .await?;
    let kind = file_kind(&blob.mime_type);
    let inserted = sqlx::query_as::<_, ItemRow>(
        r#"INSERT INTO vault_items
           (organization_id, created_by_user_id, created_by_key_id, kind, blob_id,
            original_filename, virtual_path, source_url, tags, content_hash, size_bytes)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING *"#,
    )
    .bind(&session.organization_id)
    .bind(&session.created_by_user_id)
    .bind(&session.created_by_key_id)
    .bind(kind)
    .bind(blob.id)
    .bind(&session.original_filename)
    .bind(&session.virtual_path)
    .bind(&session.source_url)
    .bind(&session.tags)
    .bind(&upload.sha256)
    .bind(actual as i64)
    .fetch_one(&mut *tx)
    .await;
    let item = match inserted {
        Ok(item) => item,
        Err(error) => {
            tx.rollback().await?;
            if blob_created {
                let _ = tokio::fs::remove_file(state.upload_root.join(&blob.storage_key)).await;
            }
            return Err(error.into());
        }
    };
    sqlx::query("UPDATE vault_upload_sessions SET state='completed', item_id=$2, error_code=NULL, updated_at=now() WHERE id=$1")
        .bind(session.id).bind(item.id).execute(&mut *tx).await?;
    tx.commit().await?;
    if let Err(error) = lease.commit().await {
        tracing::warn!(upload_id=%session.id, %error, "finalization lease release failed after commit");
    }
    tracing::info!(upload_id=%session.id, item_id=%item.id, bytes=actual, "large upload finalized");
    Ok(())
}

async fn fetch_session(
    state: &AppState,
    id: Uuid,
    organization_id: &str,
) -> AppResult<UploadSession> {
    sqlx::query_as::<_, UploadSession>(
        "SELECT * FROM vault_upload_sessions WHERE id=$1 AND organization_id=$2",
    )
    .bind(id)
    .bind(organization_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::NotFound)
}

async fn workspace_limit(state: &AppState, organization_id: &str) -> AppResult<u64> {
    let value = sqlx::query_scalar::<_, i64>(
        "SELECT max_upload_bytes FROM vault_workspace_settings WHERE organization_id=$1",
    )
    .bind(organization_id)
    .fetch_optional(&state.pool)
    .await?
    .unwrap_or(DEFAULT_WORKSPACE_LIMIT as i64);
    Ok(value.max(1) as u64)
}

fn require_tus(headers: &HeaderMap) -> AppResult<()> {
    if headers
        .get("tus-resumable")
        .and_then(|value| value.to_str().ok())
        != Some(TUS_VERSION)
    {
        return Err(AppError::Validation("Tus-Resumable must be 1.0.0".into()));
    }
    Ok(())
}

fn required_u64_header(headers: &HeaderMap, name: &str) -> AppResult<u64> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| AppError::Validation(format!("missing or invalid {name} header")))
}

fn add_tus_headers(
    response: &mut Response,
    offset: u64,
    length: u64,
    expires: Option<DateTime<Utc>>,
) {
    let headers = response.headers_mut();
    headers.insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
    headers.insert(
        "upload-offset",
        HeaderValue::from_str(&offset.to_string()).unwrap(),
    );
    headers.insert(
        "upload-length",
        HeaderValue::from_str(&length.to_string()).unwrap(),
    );
    if let Some(expires) = expires {
        let value = expires.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert("upload-expires", value);
        }
    }
}

fn parse_metadata(value: &str) -> AppResult<HashMap<String, String>> {
    let mut result = HashMap::new();
    for part in value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let (name, encoded) = part
            .split_once(' ')
            .ok_or_else(|| AppError::Validation("invalid Upload-Metadata".into()))?;
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(AppError::Validation("invalid Upload-Metadata key".into()));
        }
        let decoded = decode_base64(encoded)?;
        let decoded = String::from_utf8(decoded).map_err(|_| {
            AppError::Validation("Upload-Metadata must contain UTF-8 values".into())
        })?;
        result.insert(name.to_owned(), decoded);
    }
    Ok(result)
}

fn decode_base64(value: &str) -> AppResult<Vec<u8>> {
    let mut output = Vec::with_capacity(value.len() * 3 / 4);
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    for byte in value.bytes() {
        if byte == b'=' {
            break;
        }
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(AppError::Validation("invalid base64 metadata".into())),
        };
        accumulator = (accumulator << 6) | digit as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    Ok(output)
}

fn validate_source_url(value: Option<String>) -> AppResult<Option<String>> {
    let Some(value) = value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if value.len() > 2048 {
        return Err(AppError::Validation("sourceUrl is too long".into()));
    }
    let parsed =
        url::Url::parse(&value).map_err(|_| AppError::Validation("sourceUrl is invalid".into()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::Validation(
            "sourceUrl must use http or https".into(),
        ));
    }
    Ok(Some(value))
}

fn safe_storage_path(state: &AppState, key: &str) -> AppResult<std::path::PathBuf> {
    let relative = Path::new(key);
    if !relative
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Err(AppError::Validation("unsafe storage key".into()));
    }
    Ok(state.upload_root.join(relative))
}

#[cfg(unix)]
pub fn available_space(path: &Path) -> AppResult<u64> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| AppError::Validation("upload path is invalid".into()))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL terminated and `stats` points to writable memory.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: statvfs initialized the structure after returning success.
    let stats = unsafe { stats.assume_init() };
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

#[cfg(not(unix))]
pub fn available_space(_path: &Path) -> AppResult<u64> {
    // Disk admission is enforced on the Linux production host. Keep local Windows
    // development functional without introducing platform-specific system bindings.
    Ok(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_parser_decodes_standard_tus_values() {
        let parsed = parse_metadata("filename aGVsbG8udHh0,virtualPath L2RvY3M=").unwrap();
        assert_eq!(parsed["filename"], "hello.txt");
        assert_eq!(parsed["virtualPath"], "/docs");
    }
}
