//! Consistent database snapshots.
//!
//! Copying a live SQLite file can capture it mid-write. Instead, a background task
//! writes `DATA_DIR/backups/vault-YYYY-MM-DD.db` once a day with `VACUUM INTO`
//! (a complete, consistent database) and keeps the newest `BACKUP_KEEP` of them.
//! Volume-level backups of `DATA_DIR` therefore always include a usable copy; to
//! restore, use one of these snapshots rather than the raw `vault.db`.
//! Uploaded files are not inside the database: back up `uploads/` alongside.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use axum::{
    body::Body,
    http::{HeaderValue, header},
    response::Response,
};
use tokio_util::io::ReaderStream;

use crate::{AppState, error::AppResult};

pub const DIRECTORY: &str = "backups";
const PREFIX: &str = "vault-";
const SUFFIX: &str = ".db";

pub fn directory(data_dir: &Path) -> PathBuf {
    data_dir.join(DIRECTORY)
}

/// Writes today's snapshot if it does not exist yet, then prunes old ones.
pub async fn daily_snapshot(state: &AppState) -> anyhow::Result<Option<PathBuf>> {
    let directory = directory(&state.data_dir);
    tokio::fs::create_dir_all(&directory).await?;
    let name = format!("{PREFIX}{}{SUFFIX}", chrono::Utc::now().format("%Y-%m-%d"));
    let target = directory.join(&name);
    let created = if tokio::fs::try_exists(&target).await? {
        None
    } else {
        let partial = directory.join(format!(".{name}.partial"));
        let _ = tokio::fs::remove_file(&partial).await;
        state.db.snapshot(&partial).await?;
        tokio::fs::rename(&partial, &target).await?;
        Some(target)
    };
    prune(&directory, state.backup_keep).await?;
    Ok(created)
}

/// Removes all but the newest `keep` snapshots. `keep = 0` keeps everything.
pub async fn prune(directory: &Path, keep: usize) -> anyhow::Result<()> {
    if keep == 0 {
        return Ok(());
    }
    let mut snapshots = Vec::new();
    let mut entries = tokio::fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(PREFIX) && name.ends_with(SUFFIX) {
            snapshots.push(name);
        }
    }
    // Names embed the date, so lexical order is chronological.
    snapshots.sort();
    let excess = snapshots.len().saturating_sub(keep);
    for name in snapshots.into_iter().take(excess) {
        tokio::fs::remove_file(directory.join(&name)).await?;
        tracing::info!(snapshot = %name, "removed old database snapshot");
    }
    Ok(())
}

pub async fn worker(state: AppState) {
    if state.backup_keep == 0 {
        return;
    }
    loop {
        match daily_snapshot(&state).await {
            Ok(Some(path)) => tracing::info!(path = %path.display(), "database snapshot written"),
            Ok(None) => {}
            Err(error) => tracing::error!(error = format!("{error:#}"), "database snapshot failed"),
        }
        tokio::time::sleep(Duration::from_secs(60 * 60)).await;
    }
}

/// Streams a fresh snapshot to a global administrator.
pub async fn download(state: &AppState) -> AppResult<Response> {
    let directory = directory(&state.data_dir);
    tokio::fs::create_dir_all(&directory).await?;
    let path = directory.join(format!(".download-{}.db", uuid::Uuid::new_v4()));
    state.db.snapshot(&path).await?;
    let file = tokio::fs::File::open(&path).await;
    // The open handle keeps the bytes readable; the name is gone immediately.
    let _ = tokio::fs::remove_file(&path).await;
    let file = file?;
    let length = file.metadata().await?.len();
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    let name = format!(
        "clipboard-vault-{}.db",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.sqlite3"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pruning_keeps_the_newest_snapshots_only() {
        let directory = tempfile::tempdir().unwrap();
        for day in ["2026-01-01", "2026-01-02", "2026-01-03", "2026-01-04"] {
            std::fs::write(directory.path().join(format!("vault-{day}.db")), b"x").unwrap();
        }
        std::fs::write(directory.path().join("unrelated.txt"), b"x").unwrap();
        prune(directory.path(), 2).await.unwrap();
        let mut left: Vec<String> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "unrelated.txt",
                "vault-2026-01-03.db",
                "vault-2026-01-04.db"
            ]
        );
    }
}
