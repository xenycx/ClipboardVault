pub mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod models;
pub mod pages;
pub mod storage;
pub mod uploads;

use std::{path::PathBuf, sync::Arc};

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderName, Request},
    middleware,
    routing::{get, post},
};
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
    response
}
