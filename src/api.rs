use std::{
    collections::{HashMap, HashSet},
    path::Path as FsPath,
};

use axum::{
    Json,
    body::Body,
    extract::{FromRequest, Multipart, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, QueryBuilder};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{
    AppState,
    auth::authorize,
    error::{AppError, AppResult},
    models::{ItemCreate, ItemListResponse, ItemPatch, ItemRow, ListQuery},
    storage::{
        TempUpload, detect_text_kind, file_kind, normalize_virtual_path, persist_blob,
        stream_field_to_temp, validate_tags,
    },
};

pub async fn live() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

pub async fn ready(State(state): State<AppState>) -> AppResult<Json<Value>> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await?;
    let _: Value = state.auth.get_json("/health", &[]).await?;
    Ok(Json(json!({"status": "ready"})))
}

pub async fn create_item(State(state): State<AppState>, request: Request) -> AppResult<Response> {
    let headers = request.headers().clone();
    let auth = authorize(&state.auth, &headers, "items:write").await?;
    let declared_request_bytes = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("multipart/form-data") {
        let multipart = Multipart::from_request(request, &state)
            .await
            .map_err(|error| AppError::Validation(error.to_string()))?;
        create_file_item(&state, auth, multipart, declared_request_bytes).await
    } else {
        let Json(input) = Json::<ItemCreate>::from_request(request, &state)
            .await
            .map_err(|error| AppError::Validation(error.to_string()))?;
        create_text_item(&state, auth, input).await
    }
}

pub async fn create_text_item(
    state: &AppState,
    auth: crate::models::AuthContext,
    input: ItemCreate,
) -> AppResult<Response> {
    let payload = input.payload.trim_end().to_owned();
    if payload.trim().is_empty() {
        return Err(AppError::Validation("payload cannot be blank".into()));
    }
    let size = payload.len() as u64;
    let limit = workspace_limit(state, &auth.organization_id)
        .await?
        .min(state.legacy_max_upload_bytes);
    if size > limit {
        return Err(AppError::PayloadTooLarge {
            received: size,
            limit,
        });
    }
    let kind = detect_text_kind(&payload, &input.kind)?;
    let virtual_path = normalize_virtual_path(&input.virtual_path)?;
    let tags = validate_tags(input.tags)?;
    let source_url = validate_source_url(input.source_url)?;
    let content_hash = hex::encode(Sha256::digest(payload.as_bytes()));

    let item = sqlx::query_as::<_, ItemRow>(
        r#"INSERT INTO vault_items
           (organization_id, created_by_user_id, created_by_key_id, kind, text_payload,
            virtual_path, source_url, tags, content_hash, size_bytes)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
           RETURNING *"#,
    )
    .bind(&auth.organization_id)
    .bind(&auth.user_id)
    .bind(&auth.api_key_id)
    .bind(&kind)
    .bind(&payload)
    .bind(&virtual_path)
    .bind(&source_url)
    .bind(json!(tags))
    .bind(&content_hash)
    .bind(size as i64)
    .fetch_one(&state.pool)
    .await?;
    audit(state, &auth, "create", Some(item.id), json!({"kind": kind})).await;
    Ok((StatusCode::CREATED, Json(item)).into_response())
}

async fn create_file_item(
    state: &AppState,
    auth: crate::models::AuthContext,
    mut multipart: Multipart,
    declared_request_bytes: Option<u64>,
) -> AppResult<Response> {
    let limit = workspace_limit(state, &auth.organization_id)
        .await?
        .min(state.legacy_max_upload_bytes);
    let mut tx = state.pool.begin().await?;
    // Serialize legacy admission with Tus session creation so concurrent
    // uploads cannot collectively consume the protected disk reserve.
    sqlx::query("SELECT pg_advisory_xact_lock(192837465)")
        .execute(&mut *tx)
        .await?;
    let outstanding = sqlx::query_scalar::<_, i64>(
        "SELECT least(coalesce(sum(expected_bytes - acknowledged_bytes), 0), 9223372036854775807)::bigint FROM vault_upload_sessions WHERE state = 'uploading' AND expires_at > now()",
    )
    .fetch_one(&mut *tx)
    .await?
    .max(0) as u64;
    let available = crate::uploads::available_space(&state.upload_root)?;
    let incoming = declared_request_bytes.unwrap_or_else(|| limit.saturating_add(1024 * 1024));
    let required = state
        .upload_disk_reserve_bytes
        .saturating_add(outstanding)
        .saturating_add(incoming);
    if available < required {
        return Err(AppError::InsufficientStorage {
            available,
            required,
        });
    }
    let mut upload: Option<TempUpload> = None;
    let mut virtual_path = "/".to_owned();
    let mut tags = Vec::new();
    let mut source_url = None;

    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "file" => {
                if upload.is_some() {
                    return Err(AppError::Validation("only one file is allowed".into()));
                }
                upload = Some(stream_field_to_temp(field, &state.upload_root, limit).await?);
            }
            "virtual_path" | "path" => virtual_path = field.text().await?,
            "tags" => {
                let raw = field.text().await?;
                tags = serde_json::from_str::<Vec<String>>(&raw).unwrap_or_else(|_| {
                    raw.split(',')
                        .map(str::trim)
                        .map(str::to_owned)
                        .filter(|s| !s.is_empty())
                        .collect()
                });
            }
            "source_url" => source_url = Some(field.text().await?),
            _ => {}
        }
    }
    let upload =
        upload.ok_or_else(|| AppError::Validation("multipart request must include file".into()))?;
    let virtual_path = normalize_virtual_path(&virtual_path)?;
    let tags = validate_tags(tags)?;
    let source_url = validate_source_url(source_url)?;
    let available = crate::uploads::available_space(&state.upload_root)?;
    let required = state.upload_disk_reserve_bytes.saturating_add(outstanding);
    if available < required {
        return Err(AppError::InsufficientStorage {
            available,
            required,
        });
    }
    let (blob, blob_created) = persist_blob(
        &mut tx,
        &state.pool,
        &state.upload_root,
        &auth.organization_id,
        &upload,
    )
    .await?;
    let kind = file_kind(&blob.mime_type);
    let item_result = sqlx::query_as::<_, ItemRow>(
        r#"INSERT INTO vault_items
           (organization_id, created_by_user_id, created_by_key_id, kind, blob_id,
            original_filename, virtual_path, source_url, tags, content_hash, size_bytes)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
           RETURNING *"#,
    )
    .bind(&auth.organization_id)
    .bind(&auth.user_id)
    .bind(&auth.api_key_id)
    .bind(kind)
    .bind(blob.id)
    .bind(&upload.original_filename)
    .bind(&virtual_path)
    .bind(&source_url)
    .bind(json!(tags))
    .bind(&upload.sha256)
    .bind(upload.size_bytes as i64)
    .fetch_one(&mut *tx)
    .await;
    let item = match item_result {
        Ok(item) => item,
        Err(error) => {
            tx.rollback().await?;
            if blob_created {
                let _ = tokio::fs::remove_file(state.upload_root.join(&blob.storage_key)).await;
            }
            return Err(error.into());
        }
    };
    tx.commit().await?;
    audit(
        state,
        &auth,
        "upload",
        Some(item.id),
        json!({"mime": blob.mime_type}),
    )
    .await;
    Ok((StatusCode::CREATED, Json(item)).into_response())
}

pub async fn list_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<ItemListResponse>> {
    let auth = authorize(&state.auth, &headers, "items:read").await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let mut builder =
        QueryBuilder::<Postgres>::new("SELECT * FROM vault_items WHERE organization_id = ");
    builder.push_bind(&auth.organization_id);
    if query.trash.unwrap_or(false) {
        builder.push(" AND deleted_at IS NOT NULL");
    } else {
        builder.push(" AND deleted_at IS NULL");
    }
    if let Some(kind) = query.kind {
        builder.push(" AND kind = ").push_bind(kind);
    }
    if let Some(before) = query.before {
        builder.push(" AND created_at < ").push_bind(before);
    }
    if let Some(tag) = query.tag {
        builder.push(" AND tags ? ").push_bind(tag);
    }
    if let Some(q) = query.q.filter(|q| !q.trim().is_empty()) {
        builder
            .push(" AND to_tsvector('simple', coalesce(text_payload,'') || ' ' || coalesce(original_filename,'') || ' ' || virtual_path) @@ plainto_tsquery('simple', ")
            .push_bind(q)
            .push(")");
    }
    builder
        .push(" ORDER BY pinned DESC, created_at DESC LIMIT ")
        .push_bind(limit + 1);
    let mut items = builder
        .build_query_as::<ItemRow>()
        .fetch_all(&state.pool)
        .await?;
    let has_more = items.len() as i64 > limit;
    if has_more {
        items.truncate(limit as usize);
    }
    let next_cursor = if has_more {
        items.last().map(|item| item.created_at)
    } else {
        None
    };
    Ok(Json(ItemListResponse { items, next_cursor }))
}

pub async fn get_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ItemRow>> {
    let auth = authorize(&state.auth, &headers, "items:read").await?;
    Ok(Json(fetch_item(&state, &auth.organization_id, id).await?))
}

pub async fn update_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<ItemPatch>,
) -> AppResult<Json<ItemRow>> {
    let auth = authorize(&state.auth, &headers, "items:write").await?;
    let virtual_path = input
        .virtual_path
        .as_deref()
        .map(normalize_virtual_path)
        .transpose()?;
    let tags = input
        .tags
        .map(validate_tags)
        .transpose()?
        .map(|tags| json!(tags));
    let has_source_url = input.source_url.is_some();
    let source_url = input.source_url.flatten();
    let source_url = validate_source_url(source_url)?;
    let item = sqlx::query_as::<_, ItemRow>(
        r#"UPDATE vault_items SET
           virtual_path = COALESCE($3, virtual_path), tags = COALESCE($4, tags),
           source_url = CASE WHEN $5 THEN $6 ELSE source_url END,
           pinned = COALESCE($7, pinned), updated_at = now()
           WHERE id = $1 AND organization_id = $2 AND deleted_at IS NULL RETURNING *"#,
    )
    .bind(id)
    .bind(&auth.organization_id)
    .bind(virtual_path)
    .bind(tags)
    .bind(has_source_url)
    .bind(source_url)
    .bind(input.pinned)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::NotFound)?;
    audit(&state, &auth, "update", Some(id), json!({})).await;
    Ok(Json(item))
}

pub async fn delete_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let auth = authorize(&state.auth, &headers, "items:delete").await?;
    let changed = sqlx::query("UPDATE vault_items SET deleted_at = now(), updated_at = now() WHERE id = $1 AND organization_id = $2 AND deleted_at IS NULL")
        .bind(id).bind(&auth.organization_id).execute(&state.pool).await?.rows_affected();
    if changed == 0 {
        return Err(AppError::NotFound);
    }
    audit(&state, &auth, "delete", Some(id), json!({})).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn restore_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ItemRow>> {
    let auth = authorize(&state.auth, &headers, "items:delete").await?;
    let item = sqlx::query_as::<_, ItemRow>(
        "UPDATE vault_items SET deleted_at = NULL, updated_at = now() WHERE id = $1 AND organization_id = $2 AND deleted_at IS NOT NULL RETURNING *",
    ).bind(id).bind(&auth.organization_id).fetch_optional(&state.pool).await?.ok_or(AppError::NotFound)?;
    audit(&state, &auth, "restore", Some(id), json!({})).await;
    Ok(Json(item))
}

pub async fn purge_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let auth = authorize(&state.auth, &headers, "items:delete").await?;
    let mut tx = state.pool.begin().await?;
    let blob = sqlx::query_as::<_, (Option<Uuid>, Option<String>)>(
        r#"SELECT i.blob_id, b.storage_key FROM vault_items i
           LEFT JOIN vault_blobs b ON b.id = i.blob_id
           WHERE i.id = $1 AND i.organization_id = $2 AND i.deleted_at IS NOT NULL
           FOR UPDATE OF i"#,
    )
    .bind(id)
    .bind(&auth.organization_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    sqlx::query("DELETE FROM vault_items WHERE id = $1 AND organization_id = $2")
        .bind(id)
        .bind(&auth.organization_id)
        .execute(&mut *tx)
        .await?;
    let mut remove_storage = None;
    if let Some(blob_id) = blob.0 {
        let references = sqlx::query_scalar::<_, i64>(
            "SELECT count(*)::bigint FROM vault_items WHERE blob_id = $1",
        )
        .bind(blob_id)
        .fetch_one(&mut *tx)
        .await?;
        if references == 0 {
            sqlx::query("DELETE FROM vault_blobs WHERE id = $1")
                .bind(blob_id)
                .execute(&mut *tx)
                .await?;
            remove_storage = blob.1;
        }
    }
    tx.commit().await?;
    if let Some(storage_key) = remove_storage {
        let path = std::path::Path::new(&storage_key);
        if path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
            && let Err(error) = tokio::fs::remove_file(state.upload_root.join(path)).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, %storage_key, "failed to remove purged blob");
        }
    }
    audit(&state, &auth, "purge", None, json!({"itemId": id})).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn download_content(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    Path(id): Path<Uuid>,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let auth = authorize(&state.auth, &headers, "items:read").await?;
    let row = sqlx::query_as::<_, (String, String, Option<String>)>(
        r#"SELECT b.storage_key, b.mime_type, i.original_filename
           FROM vault_items i JOIN vault_blobs b ON b.id = i.blob_id
           WHERE i.id = $1 AND i.organization_id = $2 AND i.deleted_at IS NULL"#,
    )
    .bind(id)
    .bind(&auth.organization_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::NotFound)?;
    let path = checked_blob_path(&state, &row.0)?;
    let disposition = if query.get("download").map(String::as_str) == Some("1") {
        "attachment"
    } else if row.1.starts_with("image/") && row.1 != "image/svg+xml" {
        "inline"
    } else {
        "attachment"
    };
    let filename = row
        .2
        .unwrap_or_else(|| "download.bin".into())
        .replace(['\r', '\n', '"'], "_");
    ranged_file_response(&path, &headers, &method, &row.1, disposition, &filename).await
}

pub async fn preview_content(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    let auth = authorize(&state.auth, &headers, "items:read").await?;
    let row = sqlx::query_as::<_, (String, String, Option<String>, i64)>(
        r#"SELECT b.storage_key, b.mime_type, i.original_filename, b.size_bytes
           FROM vault_items i JOIN vault_blobs b ON b.id = i.blob_id
           WHERE i.id=$1 AND i.organization_id=$2 AND i.deleted_at IS NULL"#,
    )
    .bind(id)
    .bind(&auth.organization_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::NotFound)?;
    let path = checked_blob_path(&state, &row.0)?;
    let filename = row.2.unwrap_or_else(|| "preview.bin".into());
    let mut sample_file = File::open(&path).await?;
    let mut sample = vec![0_u8; (row.3.max(0) as usize).min(8192)];
    let sample_len = sample_file.read(&mut sample).await?;
    sample.truncate(sample_len);
    let preview = classify_preview(&sample, &row.1, &filename);
    match preview.kind {
        "pdf" => {
            let mut response = ranged_file_response(
                &path,
                &headers,
                &method,
                "application/pdf",
                "inline",
                &filename,
            )
            .await?;
            set_preview_headers(&mut response, &preview, false);
            Ok(response)
        }
        "image" => {
            if row.3 > 50 * 1024 * 1024 {
                return Err(AppError::PreviewUnavailable);
            }
            let mut response =
                ranged_file_response(&path, &headers, &method, preview.mime, "inline", &filename)
                    .await?;
            set_preview_headers(&mut response, &preview, false);
            Ok(response)
        }
        "text" | "code" | "markdown" | "html" => {
            const TEXT_PREVIEW_LIMIT: u64 = 10 * 1024 * 1024;
            let take = (row.3.max(0) as u64).min(TEXT_PREVIEW_LIMIT);
            let file = File::open(&path).await?;
            let mut bytes = Vec::with_capacity(take.min(usize::MAX as u64) as usize);
            file.take(take).read_to_end(&mut bytes).await?;
            let text = decode_text(&bytes)?;
            let truncated = row.3.max(0) as u64 > take;
            let mut response = if method == Method::HEAD {
                Response::new(Body::empty())
            } else {
                Response::new(Body::from(text.clone()))
            };
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&text.len().to_string()).unwrap(),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-store"),
            );
            set_preview_headers(&mut response, &preview, truncated);
            Ok(response)
        }
        _ => Err(AppError::PreviewUnavailable),
    }
}

async fn ranged_file_response(
    path: &FsPath,
    request_headers: &HeaderMap,
    method: &Method,
    mime: &str,
    disposition: &str,
    filename: &str,
) -> AppResult<Response> {
    let mut file = File::open(path).await?;
    let size = file.metadata().await?.len();
    let range = request_headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|value| parse_range(value, size))
        .transpose()?;
    let (start, end, status) = match range {
        Some((start, end)) => (start, end, StatusCode::PARTIAL_CONTENT),
        None => (0, size.saturating_sub(1), StatusCode::OK),
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(file.take(length)))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "{disposition}; filename=\"{}\"",
            sanitize_filename(filename)
        ))
        .unwrap_or(HeaderValue::from_static("attachment")),
    );
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).unwrap(),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")).unwrap(),
        );
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

fn parse_range(value: &str, size: u64) -> AppResult<(u64, u64)> {
    let value = value
        .strip_prefix("bytes=")
        .ok_or(AppError::RangeNotSatisfiable { size })?;
    if value.contains(',') || size == 0 {
        return Err(AppError::RangeNotSatisfiable { size });
    }
    let (start, end) = value
        .split_once('-')
        .ok_or(AppError::RangeNotSatisfiable { size })?;
    let (start, end) = if start.is_empty() {
        let suffix = end
            .parse::<u64>()
            .map_err(|_| AppError::RangeNotSatisfiable { size })?;
        if suffix == 0 {
            return Err(AppError::RangeNotSatisfiable { size });
        }
        (size.saturating_sub(suffix), size - 1)
    } else {
        let start = start
            .parse::<u64>()
            .map_err(|_| AppError::RangeNotSatisfiable { size })?;
        let end = if end.is_empty() {
            size - 1
        } else {
            end.parse::<u64>()
                .map_err(|_| AppError::RangeNotSatisfiable { size })?
        };
        (start, end.min(size - 1))
    };
    if start >= size || start > end {
        return Err(AppError::RangeNotSatisfiable { size });
    }
    Ok((start, end))
}

struct PreviewClassification {
    kind: &'static str,
    mime: &'static str,
    language: &'static str,
}

fn classify_preview(sample: &[u8], mime: &str, filename: &str) -> PreviewClassification {
    if sample.starts_with(b"%PDF-") {
        return PreviewClassification {
            kind: "pdf",
            mime: "application/pdf",
            language: "",
        };
    }
    let image_mime = if sample.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if sample.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if sample.starts_with(b"GIF87a") || sample.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if sample.len() >= 12 && &sample[..4] == b"RIFF" && &sample[8..12] == b"WEBP" {
        Some("image/webp")
    } else if sample.starts_with(b"BM") {
        Some("image/bmp")
    } else if sample.starts_with(&[0, 0, 1, 0]) {
        Some("image/x-icon")
    } else if sample.len() >= 12
        && &sample[4..8] == b"ftyp"
        && (&sample[8..12] == b"avif" || &sample[8..12] == b"avis")
    {
        Some("image/avif")
    } else {
        None
    };
    if let Some(mime) = image_mime {
        return PreviewClassification {
            kind: "image",
            mime,
            language: "",
        };
    }
    let extension = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let textual_mime = mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-yaml"
        )
        || matches!(
            extension.as_str(),
            "txt"
                | "md"
                | "markdown"
                | "html"
                | "htm"
                | "css"
                | "js"
                | "jsx"
                | "ts"
                | "tsx"
                | "json"
                | "xml"
                | "yaml"
                | "yml"
                | "toml"
                | "rs"
                | "py"
                | "rb"
                | "go"
                | "java"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "cs"
                | "php"
                | "sh"
                | "bash"
                | "ps1"
                | "sql"
                | "ini"
                | "cfg"
                | "conf"
                | "log"
                | "csv"
                | "tsv"
                | "svg"
        );
    let binary = sample.iter().take(8192).filter(|byte| **byte == 0).count()
        > sample.len().saturating_div(16).max(1)
        && !sample.starts_with(&[0xff, 0xfe])
        && !sample.starts_with(&[0xfe, 0xff]);
    let control_bytes = sample
        .iter()
        .filter(|byte| **byte < 0x20 && !matches!(**byte, b'\n' | b'\r' | b'\t' | 0x0c))
        .count();
    let textual_sample = sample.starts_with(&[0xff, 0xfe])
        || sample.starts_with(&[0xfe, 0xff])
        || (std::str::from_utf8(sample).is_ok()
            && control_bytes <= sample.len().saturating_div(32));
    if (!textual_mime && !textual_sample) || binary {
        return PreviewClassification {
            kind: "unsupported",
            mime: "application/octet-stream",
            language: "",
        };
    }
    let (kind, language) = match extension.as_str() {
        "md" | "markdown" => ("markdown", "markdown"),
        "html" | "htm" => ("html", "html"),
        "txt" | "log" => ("text", "text"),
        "svg" | "xml" => ("code", "xml"),
        "yml" | "yaml" => ("code", "yaml"),
        other if !other.is_empty() => ("code", language_for_extension(other)),
        _ => ("text", "text"),
    };
    PreviewClassification {
        kind,
        mime: "text/plain; charset=utf-8",
        language,
    }
}

fn language_for_extension(ext: &str) -> &'static str {
    match ext {
        "js" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "rs" => "rust",
        "py" => "python",
        "rb" => "ruby",
        "sh" | "bash" => "bash",
        "ps1" => "powershell",
        "c" | "h" => "c",
        "cpp" | "hpp" => "cpp",
        "cs" => "csharp",
        "json" => "json",
        "css" => "css",
        "sql" => "sql",
        "toml" => "toml",
        "java" => "java",
        "go" => "go",
        _ => "text",
    }
}

fn decode_text(bytes: &[u8]) -> AppResult<String> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        let words = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&words).map_err(|_| AppError::PreviewUnavailable);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        let words = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&words).map_err(|_| AppError::PreviewUnavailable);
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_owned());
    }
    let (decoded, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
    Ok(decoded.into_owned())
}

fn set_preview_headers(response: &mut Response, preview: &PreviewClassification, truncated: bool) {
    let headers = response.headers_mut();
    headers.insert("x-preview-kind", HeaderValue::from_static(preview.kind));
    headers.insert(
        "x-preview-truncated",
        HeaderValue::from_static(if truncated { "true" } else { "false" }),
    );
    if !preview.language.is_empty() {
        headers.insert(
            "x-preview-language",
            HeaderValue::from_static(preview.language),
        );
    }
}

fn sanitize_filename(filename: &str) -> String {
    filename.replace(['\r', '\n', '"'], "_")
}

fn checked_blob_path(state: &AppState, storage_key: &str) -> AppResult<std::path::PathBuf> {
    let path = FsPath::new(storage_key);
    if !path
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Err(AppError::Validation("unsafe storage key".into()));
    }
    Ok(state.upload_root.join(path))
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct StorageCleanupItem {
    pub id: Uuid,
    pub filename: String,
    pub size_bytes: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub deleted: bool,
    pub reclaimable_bytes: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStatusResponse {
    pub workspace_name: String,
    pub free_bytes: u64,
    pub reserve_bytes: u64,
    pub workspace_bytes: i64,
    pub reserved_upload_bytes: i64,
    pub reclaimable_bytes: i64,
    pub low_storage: bool,
    pub items: Vec<StorageCleanupItem>,
}

pub async fn storage_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<StorageStatusResponse>> {
    let auth = authorize(&state.auth, &headers, "items:read").await?;
    require_workspace_manager(&auth)?;
    Ok(Json(storage_details(&state, &auth).await?))
}

pub(crate) async fn storage_details(
    state: &AppState,
    auth: &crate::models::AuthContext,
) -> AppResult<StorageStatusResponse> {
    let workspace_name =
        sqlx::query_scalar::<_, String>("SELECT name FROM organization WHERE id=$1")
            .bind(&auth.organization_id)
            .fetch_one(&state.pool)
            .await?;
    let workspace_bytes = sqlx::query_scalar::<_, i64>(
        "SELECT least(coalesce(sum(size_bytes), 0), 9223372036854775807)::bigint FROM vault_blobs WHERE organization_id=$1",
    )
    .bind(&auth.organization_id)
    .fetch_one(&state.pool)
    .await?;
    let reserved_upload_bytes = sqlx::query_scalar::<_, i64>(
        "SELECT least(coalesce(sum(expected_bytes-acknowledged_bytes),0), 9223372036854775807)::bigint FROM vault_upload_sessions WHERE organization_id=$1 AND state='uploading' AND expires_at>now()",
    )
    .bind(&auth.organization_id)
    .fetch_one(&state.pool)
    .await?;
    let items = sqlx::query_as::<_, StorageCleanupItem>(
        r#"SELECT i.id,
                  coalesce(i.original_filename, 'Untitled') AS filename,
                  i.size_bytes,
                  i.created_at,
                  (i.deleted_at IS NOT NULL) AS deleted,
                  CASE WHEN i.blob_id IS NOT NULL
                         AND (SELECT count(*) FROM vault_items refs WHERE refs.blob_id=i.blob_id)=1
                       THEN i.size_bytes ELSE 0 END AS reclaimable_bytes
           FROM vault_items i
           WHERE i.organization_id=$1 AND i.blob_id IS NOT NULL
           ORDER BY (i.deleted_at IS NOT NULL) DESC, i.size_bytes DESC, i.created_at ASC
           LIMIT 250"#,
    )
    .bind(&auth.organization_id)
    .fetch_all(&state.pool)
    .await?;
    let reclaimable_bytes = items
        .iter()
        .filter(|item| item.deleted)
        .map(|item| item.reclaimable_bytes)
        .sum();
    let free_bytes = crate::uploads::available_space(&state.upload_root)?;
    let low_storage = free_bytes
        < state
            .upload_disk_reserve_bytes
            .saturating_add(reserved_upload_bytes.max(0) as u64);
    Ok(StorageStatusResponse {
        workspace_name,
        free_bytes,
        reserve_bytes: state.upload_disk_reserve_bytes,
        workspace_bytes,
        reserved_upload_bytes,
        reclaimable_bytes,
        low_storage,
        items,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkPurgeRequest {
    item_ids: Vec<Uuid>,
    confirmation: String,
}

pub async fn bulk_purge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<BulkPurgeRequest>,
) -> AppResult<Json<Value>> {
    let auth = authorize(&state.auth, &headers, "items:delete").await?;
    require_workspace_manager(&auth)?;
    if auth.api_key_id.is_none() {
        require_same_origin(&state, &headers)?;
    }
    let (purged, reclaimed_bytes) =
        purge_items(&state, &auth, input.item_ids, &input.confirmation).await?;
    Ok(Json(
        json!({"purged": purged, "reclaimedBytes": reclaimed_bytes}),
    ))
}

pub(crate) async fn purge_items(
    state: &AppState,
    auth: &crate::models::AuthContext,
    item_ids: Vec<Uuid>,
    confirmation: &str,
) -> AppResult<(usize, i64)> {
    require_workspace_manager(auth)?;
    if item_ids.is_empty() || item_ids.len() > 250 {
        return Err(AppError::Validation(
            "select between 1 and 250 items".into(),
        ));
    }
    let workspace_name =
        sqlx::query_scalar::<_, String>("SELECT name FROM organization WHERE id=$1")
            .bind(&auth.organization_id)
            .fetch_one(&state.pool)
            .await?;
    if confirmation.trim() != workspace_name {
        return Err(AppError::Validation(
            "workspace name confirmation does not match".into(),
        ));
    }
    let unique = item_ids
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut tx = state.pool.begin().await?;
    let rows = sqlx::query_as::<_, (Uuid, Option<Uuid>, Option<String>)>(
        r#"SELECT i.id, i.blob_id, b.storage_key
           FROM vault_items i LEFT JOIN vault_blobs b ON b.id=i.blob_id
           WHERE i.organization_id=$1 AND i.id = ANY($2)
           FOR UPDATE OF i"#,
    )
    .bind(&auth.organization_id)
    .bind(&unique)
    .fetch_all(&mut *tx)
    .await?;
    if rows.len() != unique.len() {
        return Err(AppError::Validation(
            "one or more selected items were not found".into(),
        ));
    }
    sqlx::query("DELETE FROM vault_items WHERE organization_id=$1 AND id=ANY($2)")
        .bind(&auth.organization_id)
        .bind(&unique)
        .execute(&mut *tx)
        .await?;
    let mut seen_blobs = HashSet::new();
    let mut storage_keys = Vec::new();
    let mut reclaimed_bytes = 0_i64;
    for (_, blob_id, storage_key) in rows {
        let Some(blob_id) = blob_id else { continue };
        if !seen_blobs.insert(blob_id) {
            continue;
        }
        let references = sqlx::query_scalar::<_, i64>(
            "SELECT count(*)::bigint FROM vault_items WHERE blob_id=$1",
        )
        .bind(blob_id)
        .fetch_one(&mut *tx)
        .await?;
        if references == 0 {
            let size = sqlx::query_scalar::<_, i64>(
                "DELETE FROM vault_blobs WHERE id=$1 RETURNING size_bytes",
            )
            .bind(blob_id)
            .fetch_one(&mut *tx)
            .await?;
            reclaimed_bytes = reclaimed_bytes.saturating_add(size);
            if let Some(key) = storage_key {
                storage_keys.push(key);
            }
        }
    }
    tx.commit().await?;
    for key in storage_keys {
        if let Ok(path) = checked_blob_path(state, &key)
            && let Err(error) = tokio::fs::remove_file(path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, storage_key=%key, "failed to remove purged blob");
        }
    }
    audit(
        state,
        auth,
        "bulk_purge",
        None,
        json!({"itemIds": unique, "reclaimedBytes": reclaimed_bytes}),
    )
    .await;
    Ok((unique.len(), reclaimed_bytes))
}

pub(crate) fn require_workspace_manager(auth: &crate::models::AuthContext) -> AppResult<()> {
    if matches!(auth.role.as_str(), "owner" | "admin") {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

pub(crate) fn require_same_origin(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    let supplied = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or(AppError::Forbidden)?;
    let supplied = url::Url::parse(supplied).map_err(|_| AppError::Forbidden)?;
    let expected = url::Url::parse(&state.public_base_url).map_err(|_| AppError::Forbidden)?;
    if supplied.origin() != expected.origin() {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

pub async fn export_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let auth = authorize(&state.auth, &headers, "items:read").await?;
    let items = sqlx::query_as::<_, ItemRow>("SELECT * FROM vault_items WHERE organization_id = $1 AND deleted_at IS NULL ORDER BY created_at DESC")
        .bind(&auth.organization_id).fetch_all(&state.pool).await?;
    if query.get("format").map(String::as_str) == Some("csv") {
        let mut writer = csv::Writer::from_writer(Vec::new());
        writer
            .write_record([
                "id",
                "type",
                "filename",
                "virtual_path",
                "size_bytes",
                "created_at",
                "payload",
            ])
            .map_err(|error| AppError::Other(error.into()))?;
        for item in &items {
            writer
                .write_record([
                    item.id.to_string(),
                    item.kind.clone(),
                    item.original_filename.clone().unwrap_or_default(),
                    item.virtual_path.clone(),
                    item.size_bytes.to_string(),
                    item.created_at.to_rfc3339(),
                    item.text_payload.clone().unwrap_or_default(),
                ])
                .map_err(|error| AppError::Other(error.into()))?;
        }
        let bytes = writer
            .into_inner()
            .map_err(|error| AppError::Other(error.into_error().into()))?;
        let mut response = Response::new(Body::from(bytes));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/csv; charset=utf-8"),
        );
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"clipboard-vault.csv\""),
        );
        return Ok(response);
    }
    let mut response = Json(json!({"items": items})).into_response();
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"clipboard-vault.json\""),
    );
    Ok(response)
}

pub async fn list_tags(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let auth = authorize(&state.auth, &headers, "items:read").await?;
    let tags = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT jsonb_array_elements_text(tags) FROM vault_items WHERE organization_id = $1 AND deleted_at IS NULL ORDER BY 1",
    ).bind(&auth.organization_id).fetch_all(&state.pool).await?;
    Ok(Json(json!({"tags": tags})))
}

async fn fetch_item(state: &AppState, organization_id: &str, id: Uuid) -> AppResult<ItemRow> {
    sqlx::query_as::<_, ItemRow>("SELECT * FROM vault_items WHERE id = $1 AND organization_id = $2")
        .bind(id)
        .bind(organization_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(AppError::NotFound)
}

async fn workspace_limit(state: &AppState, organization_id: &str) -> AppResult<u64> {
    let configured = sqlx::query_scalar::<_, i64>(
        "SELECT max_upload_bytes FROM vault_workspace_settings WHERE organization_id = $1",
    )
    .bind(organization_id)
    .fetch_optional(&state.pool)
    .await?
    .unwrap_or(104_857_600);
    Ok((configured.max(1) as u64).min(state.server_max_upload_bytes))
}

fn validate_source_url(value: Option<String>) -> AppResult<Option<String>> {
    let Some(value) = value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if value.len() > 2048 {
        return Err(AppError::Validation("source_url is too long".into()));
    }
    let parsed = url::Url::parse(&value)
        .map_err(|_| AppError::Validation("source_url is invalid".into()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::Validation(
            "source_url must use http or https".into(),
        ));
    }
    Ok(Some(value))
}

async fn audit(
    state: &AppState,
    auth: &crate::models::AuthContext,
    action: &str,
    item_id: Option<Uuid>,
    detail: Value,
) {
    let result = sqlx::query(
        "INSERT INTO vault_activity (organization_id, actor_user_id, actor_key_id, action, item_id, detail) VALUES ($1,$2,$3,$4,$5,$6)",
    ).bind(&auth.organization_id).bind(&auth.user_id).bind(&auth.api_key_id).bind(action).bind(item_id).bind(detail).execute(&state.pool).await;
    if let Err(error) = result {
        tracing::warn!(%error, "failed to record activity");
    }
}
