use std::collections::HashMap;

use axum::{
    Json,
    body::Body,
    extract::{FromRequest, Multipart, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, QueryBuilder};
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
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("multipart/form-data") {
        let multipart = Multipart::from_request(request, &state)
            .await
            .map_err(|error| AppError::Validation(error.to_string()))?;
        create_file_item(&state, auth, multipart).await
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
    let limit = workspace_limit(state, &auth.organization_id).await?;
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
) -> AppResult<Response> {
    let limit = workspace_limit(state, &auth.organization_id).await?;
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
    let mut tx = state.pool.begin().await?;
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
        let references =
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM vault_items WHERE blob_id = $1")
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
        {
            if let Err(error) = tokio::fs::remove_file(state.upload_root.join(path)).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(%error, %storage_key, "failed to remove purged blob");
                }
            }
        }
    }
    audit(&state, &auth, "purge", None, json!({"itemId": id})).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn download_content(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
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
    let file = tokio::fs::File::open(state.upload_root.join(row.0)).await?;
    let disposition = if row.1.starts_with("image/") && row.1 != "image/svg+xml" {
        "inline"
    } else {
        "attachment"
    };
    let filename = row
        .2
        .unwrap_or_else(|| "download.bin".into())
        .replace(['\r', '\n', '"'], "_");
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&row.1)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("{disposition}; filename=\"{filename}\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
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
    .unwrap_or(26_214_400);
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
