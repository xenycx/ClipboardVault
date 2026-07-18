use std::path::{Path, PathBuf};

use axum::extract::multipart::Field;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::BlobRow,
};

pub struct TempUpload {
    pub path: PathBuf,
    pub original_filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
}

impl Drop for TempUpload {
    fn drop(&mut self) {
        // The path no longer exists after a successful move. On every early error this
        // best-effort cleanup prevents abandoned partial uploads.
        let _ = std::fs::remove_file(&self.path);
    }
}

pub async fn stream_field_to_temp(
    mut field: Field<'_>,
    upload_root: &Path,
    limit: u64,
) -> AppResult<TempUpload> {
    let filename = field
        .file_name()
        .map(safe_filename)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "upload.bin".into());
    let declared_mime = field.content_type().map(str::to_owned);
    let temp_path = upload_root
        .join("tmp")
        .join(format!("{}.part", Uuid::new_v4()));
    let mut output = fs::File::create(&temp_path).await?;
    let mut hasher = Sha256::new();
    let mut received = 0_u64;
    let mut signature = Vec::with_capacity(512);

    loop {
        let chunk = match field.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                drop(output);
                let _ = fs::remove_file(&temp_path).await;
                return Err(error.into());
            }
        };
        received = received.saturating_add(chunk.len() as u64);
        if received > limit {
            drop(output);
            let _ = fs::remove_file(&temp_path).await;
            return Err(AppError::PayloadTooLarge { received, limit });
        }
        if signature.len() < 512 {
            let take = (512 - signature.len()).min(chunk.len());
            signature.extend_from_slice(&chunk[..take]);
        }
        hasher.update(&chunk);
        if let Err(error) = output.write_all(&chunk).await {
            drop(output);
            let _ = fs::remove_file(&temp_path).await;
            return Err(error.into());
        }
    }
    if let Err(error) = output.flush().await {
        drop(output);
        let _ = fs::remove_file(&temp_path).await;
        return Err(error.into());
    }
    drop(output);
    if received == 0 {
        let _ = fs::remove_file(&temp_path).await;
        return Err(AppError::Validation("file cannot be empty".into()));
    }
    let mime_type = sniff_mime(&signature, &filename, declared_mime.as_deref());
    Ok(TempUpload {
        path: temp_path,
        original_filename: filename,
        mime_type,
        size_bytes: received,
        sha256: hex::encode(hasher.finalize()),
    })
}

pub async fn persist_blob(
    tx: &mut Transaction<'_, Postgres>,
    pool: &PgPool,
    upload_root: &Path,
    organization_id: &str,
    upload: &TempUpload,
) -> AppResult<(BlobRow, bool)> {
    if let Some(existing) = sqlx::query_as::<_, BlobRow>(
        "SELECT id, storage_key, size_bytes, mime_type FROM vault_blobs WHERE organization_id = $1 AND sha256 = $2",
    )
    .bind(organization_id)
    .bind(&upload.sha256)
    .fetch_optional(&mut **tx)
    .await?
    {
        let _ = fs::remove_file(&upload.path).await;
        return Ok((existing, false));
    }

    let blob_id = Uuid::new_v4();
    let storage_key = format!(
        "{}/{}/{}/{}",
        safe_component(organization_id),
        &upload.sha256[0..2],
        &upload.sha256[2..4],
        blob_id
    );
    let destination = upload_root.join(&storage_key);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::rename(&upload.path, &destination).await?;

    let inserted_result = sqlx::query_as::<_, BlobRow>(
        r#"INSERT INTO vault_blobs (id, organization_id, sha256, storage_key, size_bytes, mime_type)
           VALUES ($1, $2, $3, $4, $5, $6)
           ON CONFLICT (organization_id, sha256) DO NOTHING
           RETURNING id, storage_key, size_bytes, mime_type"#,
    )
    .bind(blob_id)
    .bind(organization_id)
    .bind(&upload.sha256)
    .bind(&storage_key)
    .bind(upload.size_bytes as i64)
    .bind(&upload.mime_type)
    .fetch_optional(&mut **tx)
    .await;
    let inserted = match inserted_result {
        Ok(inserted) => inserted,
        Err(error) => {
            let _ = fs::remove_file(&destination).await;
            return Err(error.into());
        }
    };

    if let Some(blob) = inserted {
        return Ok((blob, true));
    }
    let _ = fs::remove_file(&destination).await;
    let existing = sqlx::query_as::<_, BlobRow>(
        "SELECT id, storage_key, size_bytes, mime_type FROM vault_blobs WHERE organization_id = $1 AND sha256 = $2",
    )
    .bind(organization_id)
    .bind(&upload.sha256)
    .fetch_one(pool)
    .await?;
    Ok((existing, false))
}

pub fn normalize_virtual_path(value: &str) -> AppResult<String> {
    if value.len() > 1024 || value.contains('\0') {
        return Err(AppError::Validation("virtual_path is invalid".into()));
    }
    let normalized = value.replace('\\', "/");
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        match part.trim() {
            "" | "." => {}
            ".." => {
                return Err(AppError::Validation(
                    "virtual_path cannot contain ..".into(),
                ));
            }
            other if other.len() > 255 => {
                return Err(AppError::Validation(
                    "virtual_path segment is too long".into(),
                ));
            }
            other => parts.push(other),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

pub fn validate_tags(tags: Vec<String>) -> AppResult<Vec<String>> {
    if tags.len() > 20 {
        return Err(AppError::Validation("at most 20 tags are allowed".into()));
    }
    let mut clean = Vec::new();
    for tag in tags {
        let tag = tag.trim().to_lowercase();
        if tag.is_empty() {
            continue;
        }
        if tag.len() > 40
            || !tag
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(AppError::Validation(format!("invalid tag: {tag}")));
        }
        if !clean.contains(&tag) {
            clean.push(tag);
        }
    }
    Ok(clean)
}

pub fn detect_text_kind(payload: &str, requested: &str) -> AppResult<String> {
    if requested != "auto" {
        return matches!(requested, "text" | "html" | "code" | "url")
            .then(|| requested.to_owned())
            .ok_or_else(|| {
                AppError::Validation("type must be auto, text, html, code, or url".into())
            });
    }
    let trimmed = payload.trim();
    if (trimmed.starts_with("https://") || trimmed.starts_with("http://"))
        && !trimmed.contains(char::is_whitespace)
    {
        return Ok("url".into());
    }
    if trimmed.starts_with("<!DOCTYPE html") || trimmed.starts_with("<html") {
        return Ok("html".into());
    }
    let code_markers = [
        "function ",
        "const ",
        "let ",
        "import ",
        "class ",
        "def ",
        "#include",
        "fn ",
    ];
    let marker = code_markers.iter().any(|needle| payload.contains(needle));
    let punctuation = payload.contains('\n')
        && ['{', '}', ';', '=']
            .iter()
            .filter(|c| payload.contains(**c))
            .count()
            >= 2;
    let indented = payload
        .lines()
        .filter(|line| line.starts_with("  ") || line.starts_with('\t'))
        .count()
        >= 2;
    if [marker, punctuation, indented]
        .into_iter()
        .filter(|v| *v)
        .count()
        >= 2
    {
        Ok("code".into())
    } else {
        Ok("text".into())
    }
}

pub fn file_kind(mime: &str) -> &'static str {
    if mime.starts_with("image/") && !matches!(mime, "image/svg+xml") {
        "image"
    } else if mime.starts_with("text/html") {
        "html"
    } else {
        "file"
    }
}

fn safe_filename(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("upload.bin")
        .chars()
        .filter(|c| !c.is_control())
        .take(255)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn sniff_mime(signature: &[u8], filename: &str, declared: Option<&str>) -> String {
    let magic = if signature.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if signature.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if signature.starts_with(b"GIF87a") || signature.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if signature.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if signature.starts_with(b"PK\x03\x04") {
        Some("application/zip")
    } else {
        None
    };
    magic
        .or_else(|| mime_guess::from_path(filename).first_raw())
        .or(declared)
        .unwrap_or("application/octet-stream")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_paths_never_escape_storage() {
        assert!(normalize_virtual_path("../../etc/passwd").is_err());
        assert_eq!(
            normalize_virtual_path("reports\\2026//july").unwrap(),
            "/reports/2026/july"
        );
    }

    #[test]
    fn detects_code_and_urls() {
        assert_eq!(
            detect_text_kind("https://example.com/a", "auto").unwrap(),
            "url"
        );
        assert_eq!(
            detect_text_kind("fn main() {\n  println!(\"hi\");\n}", "auto").unwrap(),
            "code"
        );
    }
}
