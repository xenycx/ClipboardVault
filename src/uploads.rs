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
    db::{self, Id},
    error::{AppError, AppResult},
    models::ItemRow,
    storage::{
        TempUpload, file_kind, normalize_virtual_path, persist_blob, safe_filename, sniff_mime,
        validate_tags,
    },
};

const TUS_VERSION: &str = "1.0.0";

#[derive(Debug, Clone, FromRow)]
struct UploadSession {
    id: Id,
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
    item_id: Option<Id>,
    error_code: Option<String>,
    expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadStatus {
    id: Id,
    state: String,
    upload_offset: i64,
    upload_length: i64,
    item_id: Option<Id>,
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
    let auth = authorize(&state, &headers, "items:write").await?;
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

    // Serialize capacity admission so simultaneous creates cannot over-reserve disk.
    let admission = state.upload_admission.lock().await;
    let outstanding = outstanding_reservations(&state).await?;
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
    let temp_path = state.upload_root.join(&temp_storage_key);
    tokio::fs::File::create(&temp_path).await?;
    let now = db::now();
    let inserted = sqlx::query(
        r#"INSERT INTO vault_upload_sessions
           (id, organization_id, created_by_user_id, created_by_key_id, temp_storage_key,
            original_filename, virtual_path, source_url, tags, declared_mime, expected_bytes,
            expires_at, created_at, updated_at)
           VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)"#,
    )
    .bind(id.hyphenated())
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
    .bind(db::ts(expires_at))
    .bind(&now)
    .bind(&now)
    .execute(&state.db.write)
    .await;
    drop(admission);
    if let Err(error) = inserted {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(error.into());
    }

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
    let auth = authorize(&state, &headers, "items:write").await?;
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
    let auth = authorize(&state, &headers, "items:write").await?;
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

    // One PATCH at a time per upload. The lock is in-process, so the chunk is
    // written to disk without holding the database write connection.
    let lock = upload_lock(&state, id);
    let guard = lock.lock().await;
    let result = write_chunk(&state, &auth.organization_id, id, offset, &body).await;
    drop(guard);
    release_upload_lock(&state, id, lock);
    let (next_offset, expected, expires_at) = result?;

    let mut response = StatusCode::NO_CONTENT.into_response();
    add_tus_headers(&mut response, next_offset, expected, Some(expires_at));
    Ok(response)
}

/// Appends one chunk at the server's confirmed offset and records the new offset.
/// Returns `(new offset, expected length, new expiry)`.
async fn write_chunk(
    state: &AppState,
    organization_id: &str,
    id: Uuid,
    offset: u64,
    body: &[u8],
) -> AppResult<(u64, u64, DateTime<Utc>)> {
    let row = fetch_session(state, id, organization_id).await?;
    if row.state != "uploading" || row.expires_at <= Utc::now() {
        return Err(AppError::Validation("upload is not active".into()));
    }
    let server_offset = row.acknowledged_bytes.max(0) as u64;
    if offset != server_offset {
        return Err(AppError::OffsetConflict {
            expected: server_offset,
        });
    }
    let expected = row.expected_bytes.max(0) as u64;
    let next_offset = offset.saturating_add(body.len() as u64);
    if next_offset > expected {
        return Err(AppError::PayloadTooLarge {
            received: next_offset,
            limit: expected,
        });
    }
    let outstanding = outstanding_reservations(state).await?;
    let available = available_space(&state.upload_root)?;
    let required = state.upload_disk_reserve_bytes.saturating_add(outstanding);
    if available < required {
        return Err(AppError::InsufficientStorage {
            available,
            required,
        });
    }

    let path = safe_storage_path(state, &row.temp_storage_key)?;
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .await?;
    let actual_length = file.metadata().await?.len();
    if actual_length < server_offset {
        sqlx::query("UPDATE vault_upload_sessions SET state = 'failed', error_code = 'PARTIAL_FILE_SHORT', updated_at = ? WHERE id = ?")
            .bind(db::now())
            .bind(id.hyphenated())
            .execute(&state.db.write)
            .await?;
        return Err(AppError::Validation(
            "partial upload is shorter than its confirmed offset".into(),
        ));
    }
    if actual_length > server_offset {
        file.set_len(server_offset).await?;
    }
    file.seek(std::io::SeekFrom::Start(server_offset)).await?;
    file.write_all(body).await?;
    file.sync_data().await?;
    drop(file);
    let state_value = if next_offset == expected {
        "finalizing"
    } else {
        "uploading"
    };
    let expires_at = Utc::now()
        + chrono::Duration::seconds(state.tus_session_ttl_seconds.min(i64::MAX as u64) as i64);
    // Conditional on the offset this request started from: if anything moved the
    // session meanwhile (a cancel, an expiry), the bytes are not acknowledged.
    let updated = sqlx::query(
        r#"UPDATE vault_upload_sessions
           SET acknowledged_bytes = ?, state = ?, expires_at = ?, updated_at = ?
           WHERE id = ? AND state = 'uploading' AND acknowledged_bytes = ?"#,
    )
    .bind(next_offset as i64)
    .bind(state_value)
    .bind(db::ts(expires_at))
    .bind(db::now())
    .bind(id.hyphenated())
    .bind(server_offset as i64)
    .execute(&state.db.write)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::Validation("upload is not active".into()));
    }
    Ok((next_offset, expected, expires_at))
}

fn upload_lock(state: &AppState, id: Uuid) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let mut locks = state
        .upload_locks
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    locks.entry(id).or_default().clone()
}

/// Drops the per-upload lock entry once no other request is waiting on it.
fn release_upload_lock(state: &AppState, id: Uuid, lock: std::sync::Arc<tokio::sync::Mutex<()>>) {
    let mut locks = state
        .upload_locks
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    drop(lock);
    if locks
        .get(&id)
        .is_some_and(|entry| std::sync::Arc::strong_count(entry) == 1)
    {
        locks.remove(&id);
    }
}

/// Bytes promised to active Tus uploads but not yet received.
pub(crate) async fn outstanding_reservations(state: &AppState) -> AppResult<u64> {
    let outstanding = sqlx::query_scalar::<_, i64>(
        "SELECT coalesce(sum(expected_bytes - acknowledged_bytes), 0) FROM vault_upload_sessions WHERE state = 'uploading' AND expires_at > ?",
    )
    .bind(db::now())
    .fetch_one(&state.db.read)
    .await?;
    Ok(outstanding.max(0) as u64)
}

pub async fn cancel_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
) -> AppResult<Response> {
    require_tus(&headers)?;
    let auth = authorize(&state, &headers, "items:write").await?;
    let key = sqlx::query_scalar::<_, String>(
        r#"UPDATE vault_upload_sessions SET state = 'canceled', updated_at = ?
           WHERE id = ? AND organization_id = ? AND state IN ('uploading', 'failed')
           RETURNING temp_storage_key"#,
    )
    .bind(db::now())
    .bind(id.hyphenated())
    .bind(&auth.organization_id)
    .fetch_optional(&state.db.write)
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
    let auth = authorize(&state, &headers, "items:write").await?;
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

pub async fn maintenance_tick(state: &AppState) -> AppResult<()> {
    let now = Utc::now();
    let expired = sqlx::query_as::<_, (Id, String)>(
        r#"UPDATE vault_upload_sessions SET state = 'expired', updated_at = ?1
           WHERE state = 'uploading' AND expires_at <= ?1
           RETURNING id, temp_storage_key"#,
    )
    .bind(db::ts(now))
    .fetch_all(&state.db.write)
    .await?;
    for (_, key) in expired {
        if let Ok(path) = safe_storage_path(state, &key) {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    let stale = sqlx::query_as::<_, (Id, String)>(
        r#"DELETE FROM vault_upload_sessions
           WHERE state IN ('failed', 'canceled', 'expired', 'completed') AND expires_at <= ?
           RETURNING id, temp_storage_key"#,
    )
    .bind(db::ts(now))
    .fetch_all(&state.db.write)
    .await?;
    for (_, key) in stale {
        if let Ok(path) = safe_storage_path(state, &key) {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    // Claim one finished upload. This worker is the only finalizer and runs one
    // upload at a time, so the claim needs no lease; the five-minute clause only
    // retries a claim abandoned by a crash or restart.
    let candidate = sqlx::query_as::<_, UploadSession>(
        r#"UPDATE vault_upload_sessions SET error_code = 'PROCESSING', updated_at = ?1
           WHERE id = (
             SELECT id FROM vault_upload_sessions
             WHERE state = 'finalizing' AND (error_code IS NULL OR updated_at < ?2)
             ORDER BY updated_at LIMIT 1
           ) RETURNING *"#,
    )
    .bind(db::ts(now))
    .bind(db::ts(now - chrono::Duration::minutes(5)))
    .fetch_optional(&state.db.write)
    .await?;
    if let Some(session) = candidate
        && let Err(error) = finalize_upload(state, &session).await
    {
        tracing::error!(upload_id=%session.id, %error, "upload finalization failed");
        sqlx::query("UPDATE vault_upload_sessions SET state = 'failed', error_code = 'FINALIZATION_FAILED', updated_at = ? WHERE id = ?")
            .bind(db::now())
            .bind(session.id)
            .execute(&state.db.write)
            .await?;
    }
    Ok(())
}

async fn finalize_upload(state: &AppState, session: &UploadSession) -> AppResult<()> {
    // Hash and sniff with no transaction open: this can take a while for large files.
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
    drop(file);
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
    let mut tx = state.db.begin().await?;
    let current_state =
        sqlx::query_scalar::<_, String>("SELECT state FROM vault_upload_sessions WHERE id = ?")
            .bind(session.id)
            .fetch_optional(&mut *tx)
            .await?;
    if current_state.as_deref() != Some("finalizing") {
        // Canceled or expired meanwhile: dropping `upload` removes the partial file.
        return Ok(());
    }
    let (blob, blob_created) = persist_blob(
        &mut tx,
        &state.upload_root,
        &session.organization_id,
        &upload,
    )
    .await?;
    let kind = file_kind(&blob.mime_type);
    let now = db::now();
    let inserted = sqlx::query_as::<_, ItemRow>(
        r#"INSERT INTO vault_items
           (id, organization_id, created_by_user_id, created_by_key_id, kind, blob_id,
            original_filename, virtual_path, source_url, tags, content_hash, size_bytes,
            created_at, updated_at)
           VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?) RETURNING *"#,
    )
    .bind(Uuid::new_v4().hyphenated())
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
    .bind(&now)
    .bind(&now)
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
    sqlx::query("UPDATE vault_upload_sessions SET state = 'completed', item_id = ?, error_code = NULL, updated_at = ? WHERE id = ?")
        .bind(item.id)
        .bind(&now)
        .bind(session.id)
        .execute(&mut *tx)
        .await?;
    if let Err(error) = tx.commit().await {
        if blob_created {
            let _ = tokio::fs::remove_file(state.upload_root.join(&blob.storage_key)).await;
        }
        return Err(error.into());
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
        "SELECT * FROM vault_upload_sessions WHERE id = ? AND organization_id = ?",
    )
    .bind(id.hyphenated())
    .bind(organization_id)
    .fetch_optional(&state.db.read)
    .await?
    .ok_or(AppError::NotFound)
}

async fn workspace_limit(state: &AppState, organization_id: &str) -> AppResult<u64> {
    crate::api::workspace_limit(state, organization_id).await
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

/// Total size of the filesystem holding `path`, or `0` when it cannot be measured.
#[cfg(unix)]
pub fn total_space(path: &Path) -> AppResult<u64> {
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
    Ok(stats.f_blocks.saturating_mul(stats.f_frsize))
}

/// Zero means "unknown"; the storage page hides the capacity gauge in that case.
#[cfg(not(unix))]
pub fn total_space(_path: &Path) -> AppResult<u64> {
    Ok(0)
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
