pub mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod filters;
pub mod models;
pub mod pages;
pub mod storage;
pub mod uploads;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderName, Request},
    middleware,
    routing::{get, post},
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower_http::{
    catch_panic::CatchPanicLayer,
    compression::CompressionLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    services::ServeDir,
    trace::TraceLayer,
};

use crate::{auth::AuthBridge, config::Config};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub auth: AuthBridge,
    pub upload_root: Arc<PathBuf>,
    pub public_base_url: Arc<String>,
    pub server_max_upload_bytes: u64,
    pub legacy_max_upload_bytes: u64,
    pub upload_disk_reserve_bytes: u64,
    pub tus_chunk_size_bytes: u64,
    pub tus_session_ttl_seconds: u64,
}

impl AppState {
    pub fn new(config: &Config, pool: PgPool) -> Self {
        Self {
            pool,
            auth: AuthBridge::new(&config.auth_internal_url, &config.auth_bridge_secret),
            upload_root: Arc::new(config.upload_root.clone()),
            public_base_url: Arc::new(config.public_base_url.clone()),
            server_max_upload_bytes: config.server_max_upload_bytes,
            legacy_max_upload_bytes: config.legacy_max_upload_bytes,
            upload_disk_reserve_bytes: config.upload_disk_reserve_bytes,
            tus_chunk_size_bytes: config.tus_chunk_size_bytes,
            tus_session_ttl_seconds: config.tus_session_ttl_seconds,
        }
    }
}

/// Fingerprint of everything under `static/`, appended to asset URLs as `?v=`.
///
/// A deploy replaces the stylesheet and the markup together. Without a new URL a
/// browser can pair freshly rendered HTML with a cached copy of the old CSS, which
/// renders the application unusable until a hard refresh.
static ASSET_VERSION: LazyLock<String> = LazyLock::new(|| {
    asset_fingerprint(Path::new("static")).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
});

pub fn asset_version() -> &'static str {
    &ASSET_VERSION
}

fn asset_fingerprint(root: &Path) -> Option<String> {
    let mut files = Vec::new();
    collect_files(root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.to_string_lossy().as_bytes());
        hasher.update(std::fs::read(&file).ok()?);
    }
    Some(hex::encode(hasher.finalize())[..12].to_string())
}

fn collect_files(directory: &Path, out: &mut Vec<PathBuf>) -> Option<()> {
    for entry in std::fs::read_dir(directory).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Some(())
}

pub fn build_router(state: AppState) -> Router {
    let request_id = HeaderName::from_static("x-request-id");
    let max_body = state
        .legacy_max_upload_bytes
        .saturating_add(1024 * 1024)
        .min(usize::MAX as u64) as usize;

    Router::new()
        .route("/", get(pages::dashboard))
        .route("/login", get(pages::login))
        .route("/pending", get(pages::pending))
        .route("/setup", get(pages::setup))
        .route("/admin", get(pages::admin))
        .route("/keys", get(pages::keys))
        .route("/storage", get(pages::storage_page))
        .route("/storage/purge", post(pages::storage_purge))
        .route("/reset-password", get(pages::reset_password))
        .route("/join/{token}", get(pages::join).post(pages::redeem_invite))
        .route("/workspaces/switch", post(pages::switch_workspace))
        .route(
            "/workspaces/settings",
            post(pages::update_workspace_settings),
        )
        .route(
            "/workspaces/members/{id}/role",
            post(pages::update_member_role),
        )
        .route(
            "/workspaces/members/{id}/remove",
            post(pages::remove_member),
        )
        .route(
            "/workspaces/members/{id}/transfer",
            post(pages::transfer_ownership),
        )
        .route("/workspaces/delete", post(pages::delete_workspace))
        .route("/capture", post(pages::capture_text))
        .route("/keys/create", post(pages::create_key))
        .route("/keys/{id}/revoke", post(pages::revoke_key))
        .route("/invitations/create", post(pages::create_invitation))
        .route("/admin/users/{id}/approve", post(pages::approve_user))
        .route("/admin/users/{id}/reject", post(pages::reject_user))
        .route("/admin/users/{id}/reset", post(pages::manual_reset))
        .route("/api/v1/items", get(api::list_items).post(api::create_item))
        .route(
            "/api/v1/items/{id}",
            get(api::get_item)
                .patch(api::update_item)
                .delete(api::delete_item),
        )
        .route("/api/v1/items/{id}/restore", post(api::restore_item))
        .route("/api/v1/items/{id}/purge", post(api::purge_item))
        .route(
            "/api/v1/items/{id}/content",
            get(api::download_content).head(api::download_content),
        )
        .route(
            "/api/v1/items/{id}/preview",
            get(api::preview_content).head(api::preview_content),
        )
        .route(
            "/api/v1/uploads",
            post(uploads::create_upload).options(uploads::upload_options),
        )
        .route(
            "/api/v1/uploads/{id}",
            get(uploads::upload_status)
                .head(uploads::head_upload)
                .patch(uploads::patch_upload)
                .delete(uploads::cancel_upload),
        )
        .route("/api/v1/uploads/{id}/status", get(uploads::upload_status))
        .route("/api/v1/storage", get(api::storage_status))
        .route("/api/v1/storage/purge", post(api::bulk_purge))
        .route("/api/v1/items/export", get(api::export_items))
        .route("/api/v1/tags", get(api::list_tags))
        .route("/health/live", get(api::live))
        .route("/health/ready", get(api::ready))
        .nest_service("/static", ServeDir::new("static"))
        .fallback(pages::not_found)
        .layer(DefaultBodyLimit::max(max_body))
        .layer(middleware::from_fn(security_headers))
        .layer(CompressionLayer::new())
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

async fn security_headers(
    mut request: Request<axum::body::Body>,
    next: middleware::Next,
) -> axum::response::Response {
    request.headers_mut().remove("oai-authenticated-user-email");
    request
        .headers_mut()
        .remove("oai-authenticated-user-full-name");
    let preview = request.uri().path().ends_with("/preview");
    let tus = request.uri().path().starts_with("/api/v1/uploads");
    let asset = request.uri().path().starts_with("/static/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert(
        "x-frame-options",
        if preview { "SAMEORIGIN" } else { "DENY" }.parse().unwrap(),
    );
    headers.insert(
        "referrer-policy",
        "strict-origin-when-cross-origin".parse().unwrap(),
    );
    headers.insert(
        "permissions-policy",
        "clipboard-read=(), camera=(), microphone=(), geolocation=()"
            .parse()
            .unwrap(),
    );
    headers.insert(
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self' https://accounts.google.com"
            .parse()
            .unwrap(),
    );
    if tus {
        headers.insert("tus-resumable", "1.0.0".parse().unwrap());
    }
    if asset {
        // Every asset URL carries a content fingerprint, so the bytes behind a
        // given URL never change and may be cached indefinitely.
        headers.insert(
            "cache-control",
            "public, max-age=31536000, immutable".parse().unwrap(),
        );
    } else if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"))
    {
        // Rendered pages contain workspace data. Keep them out of shared and
        // on-disk caches, including after sign-out on a shared machine.
        headers.insert("cache-control", "no-store".parse().unwrap());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::asset_fingerprint;
    use std::path::Path;

    #[test]
    fn fingerprint_is_stable_for_unchanged_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("app.css"), "body{}").unwrap();
        let first = asset_fingerprint(directory.path()).unwrap();
        let second = asset_fingerprint(directory.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 12);
    }

    #[test]
    fn fingerprint_changes_when_an_asset_changes() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("app.css");
        std::fs::write(&file, "body{color:red}").unwrap();
        let before = asset_fingerprint(directory.path()).unwrap();
        std::fs::write(&file, "body{color:blue}").unwrap();
        assert_ne!(before, asset_fingerprint(directory.path()).unwrap());
    }

    #[test]
    fn fingerprint_covers_nested_directories() {
        let directory = tempfile::tempdir().unwrap();
        let vendor = directory.path().join("vendor");
        std::fs::create_dir(&vendor).unwrap();
        std::fs::write(vendor.join("lib.js"), "one").unwrap();
        let before = asset_fingerprint(directory.path()).unwrap();
        std::fs::write(vendor.join("lib.js"), "two").unwrap();
        assert_ne!(before, asset_fingerprint(directory.path()).unwrap());
    }

    #[test]
    fn the_shipped_assets_produce_a_real_fingerprint() {
        // Guards the production path: the server hashes ./static relative to its
        // working directory, which is /app in the container image.
        let version = asset_fingerprint(Path::new("static")).expect("static/ must be readable");
        assert_eq!(version.len(), 12);
        assert!(version.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(super::asset_version(), version);
    }

    #[test]
    fn a_missing_directory_falls_back_instead_of_failing() {
        assert!(asset_fingerprint(Path::new("does-not-exist")).is_none());
    }
}
