//! Imports a seeded 2.x PostgreSQL database and checks that the result works.
//!
//! Runs only when `IMPORT_TEST_DATABASE_URL` points at a disposable PostgreSQL
//! database (CI starts one; locally: `docker run -p 55432:5432 -e
//! POSTGRES_PASSWORD=test postgres:17-alpine`). The schema there is dropped and
//! recreated from `migrations-postgres/`.

use std::path::Path;

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use clipboard_vault::{
    AppState, build_router,
    config::Config,
    db::Db,
    identity::oauth,
    import::{Options, import},
};
use serde_json::{Value, json};
use sqlx::{Executor, postgres::PgPoolOptions};
use tower::ServiceExt;

// Produced by Better Auth 1.6.23's hashPassword and defaultKeyHasher.
const LEGACY_PASSWORD_HASH: &str = "06f2dfcd5e94507bfc19b341cbe98a90:f5a0b7f0eb93538e3b751f3516fa8215b6057c61ea3ca7d1909cf7b73711d646deb1c354dc74b3a397c4ed0cd35b6932a1d19fdca6e854a6f43a35b1667e3836";
const LEGACY_PASSWORD: &str = "correct horse battery";
const LEGACY_KEY: &str = "cv_live_AbCdEfGhIjKlMnOpQrStUvWxYzAbCdEfGhIjKlMnOpQrStUvWxYzAbCdEfGhIjKl";
const LEGACY_KEY_HASH: &str = "hrxplUbqtwmqu4OzxpwbjjtfCauGcy60-AcJVJzpz78";
const BLOB_ID: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
const BLOB_KEY: &str = "org_1/b9/4d/7c9e6679-7425-40de-944b-e07fc1f90ae7";
const FILE_BYTES: &[u8] = b"hello world";

async fn seed(url: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap();
    pool.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .await
        .unwrap();
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations-postgres");
    let mut files: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    for file in files {
        pool.execute(std::fs::read_to_string(&file).unwrap().as_str())
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
    }
    let fixtures = format!(
        r#"
        INSERT INTO "user" (id, name, email, "emailVerified", role, "approvalStatus", "approvedAt", "createdAt", "updatedAt")
        VALUES ('user_ada', 'Ada', 'Ada@Example.com', true, 'admin', 'approved', now(), now() - interval '3 days', now());
        INSERT INTO "user" (id, name, email, "emailVerified", role, "approvalStatus", "approvedBy", "createdAt", "updatedAt")
        VALUES ('user_bob', 'Bob', 'bob@example.com', true, 'user', 'approved', 'user_ada', now() - interval '2 days', now());
        INSERT INTO "user" (id, name, email, "emailVerified", "approvalStatus", banned, "banReason", "banExpires", "createdAt", "updatedAt")
        VALUES ('user_eve', 'Eve', 'eve@example.com', false, 'pending', true, 'spam', now() - interval '1 day', now() - interval '1 day', now());
        INSERT INTO account (id, "accountId", "providerId", "userId", password, "createdAt", "updatedAt")
        VALUES ('acct_ada', 'user_ada', 'credential', 'user_ada', '{LEGACY_PASSWORD_HASH}', now(), now()),
               ('acct_bob_gh', '424242', 'github', 'user_bob', NULL, now(), now());
        INSERT INTO session (id, "expiresAt", token, "userId") VALUES ('s1', now() + interval '1 day', 'tok', 'user_ada');
        INSERT INTO organization (id, name, slug, "createdAt") VALUES ('org_1', 'Ada''s Vault', 'ada-user_ada', now());
        INSERT INTO member (id, "organizationId", "userId", role, "createdAt")
        VALUES ('mem_1', 'org_1', 'user_ada', 'owner', now() - interval '1 day'),
               ('mem_2', 'org_1', 'user_bob', 'member', now());
        INSERT INTO apikey (id, name, start, "referenceId", prefix, key, enabled, "expiresAt", "createdAt", "updatedAt", permissions, metadata, "requestCount")
        VALUES ('key_1', 'vault.py', 'cv_liv', 'org_1', 'cv_live_', '{LEGACY_KEY_HASH}', true, now() + interval '30 days', now(), now(),
                '{{"items":["write","read"]}}', '{{"createdByUserId":"user_ada"}}', 7),
               ('key_orphan', 'old', 'cv_liv', 'org_1', 'cv_live_', 'some-other-hash', true, NULL, now(), now(),
                '{{"items":["write"]}}', '{{"createdByUserId":"deleted_user"}}', 0);
        INSERT INTO vault_blobs (id, organization_id, sha256, storage_key, size_bytes, mime_type)
        VALUES ('{BLOB_ID}', 'org_1', 'b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9', '{BLOB_KEY}', 11, 'text/plain');
        INSERT INTO vault_items (id, organization_id, created_by_user_id, created_by_key_id, kind, text_payload, virtual_path, tags, content_hash, size_bytes, pinned, created_at)
        VALUES ('11111111-1111-4111-8111-111111111111', 'org_1', 'user_ada', NULL, 'text', 'ყოველთვიური ანგარიში deploy', '/reports', '["finance","ka"]', 'h1', 50, true, now() - interval '2 hours'),
               ('22222222-2222-4222-8222-222222222222', 'org_1', NULL, 'key_1', 'text', 'old note', '/', '[]', 'h2', 8, false, now() - interval '1 hour');
        UPDATE vault_items SET deleted_at = now() WHERE id = '22222222-2222-4222-8222-222222222222';
        INSERT INTO vault_items (id, organization_id, created_by_user_id, kind, blob_id, original_filename, virtual_path, content_hash, size_bytes)
        VALUES ('33333333-3333-4333-8333-333333333333', 'org_1', 'user_ada', 'file', '{BLOB_ID}', 'hello.txt', '/', 'b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9', 11);
        INSERT INTO vault_invitations (organization_id, token_hash, role, created_by_user_id, expires_at)
        VALUES ('org_1', 'invite-hash', 'member', 'user_ada', now() + interval '1 day');
        INSERT INTO vault_activity (organization_id, actor_user_id, action, item_id, detail)
        VALUES ('org_1', 'user_ada', 'create', '11111111-1111-4111-8111-111111111111', '{{"kind":"text"}}'),
               ('org_1', NULL, 'purge', NULL, '{{}}');
        INSERT INTO vault_workspace_settings (organization_id, max_upload_bytes) VALUES ('org_1', 5368709120);
        UPDATE vault_server_settings SET registration_enabled = false, default_max_upload_bytes = 2147483648;
        INSERT INTO vault_upload_sessions (organization_id, temp_storage_key, original_filename, expected_bytes, expires_at)
        VALUES ('org_1', 'tmp/abandoned.part', 'big.iso', 1000, now() + interval '1 day');
        "#
    );
    pool.execute(fixtures.as_str()).await.unwrap();
    pool.close().await;
}

fn config(data_dir: &Path) -> Config {
    Config {
        port: 0,
        upload_root: data_dir.join("uploads"),
        data_dir: data_dir.to_owned(),
        public_base_url: "https://vault.example.com".into(),
        vault_secret: "vault-secret-0123456789-0123456789-abcdef".into(),
        setup_token: "setup-token-0123456789-0123456789-abcdef".into(),
        trust_proxy_headers: false,
        database_read_connections: 2,
        backup_keep: 7,
        server_max_upload_bytes: 10 * 1024 * 1024 * 1024,
        legacy_max_upload_bytes: 100 * 1024 * 1024,
        upload_disk_reserve_bytes: 0,
        tus_chunk_size_bytes: 1024 * 1024,
        tus_session_ttl_seconds: 3600,
    }
}

async fn call(router: &axum::Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
}

#[tokio::test]
async fn a_seeded_2x_database_imports_and_keeps_working() {
    let Ok(url) = std::env::var("IMPORT_TEST_DATABASE_URL") else {
        eprintln!("skipped: set IMPORT_TEST_DATABASE_URL to run the PostgreSQL import test");
        return;
    };
    seed(&url).await;
    let old_uploads = tempfile::tempdir().unwrap();
    let file = old_uploads.path().join(BLOB_KEY);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, FILE_BYTES).unwrap();

    // A dry run reports but writes nothing.
    let dry = tempfile::tempdir().unwrap();
    let report = import(&Options {
        from: url.clone(),
        data_dir: dry.path().to_owned(),
        uploads_from: Some(old_uploads.path().to_owned()),
        dry_run: true,
    })
    .await
    .unwrap();
    assert!(report.is_clean(), "{}", report.render());
    assert!(!dry.path().join("vault.db").exists());
    assert!(!dry.path().join("uploads").exists());

    let data = tempfile::tempdir().unwrap();
    let options = Options {
        from: url.clone(),
        data_dir: data.path().to_owned(),
        uploads_from: Some(old_uploads.path().to_owned()),
        dry_run: false,
    };
    let report = import(&options).await.unwrap();
    assert!(report.is_clean(), "{}", report.render());
    assert_eq!(report.files_copied, 1);
    let count = |table: &str| {
        report
            .counts
            .iter()
            .find(|count| count.table == table)
            .unwrap()
            .target
    };
    assert_eq!(count("users"), 3);
    assert_eq!(count("oauth_accounts"), 1);
    assert_eq!(count("memberships"), 2);
    assert_eq!(count("api_keys"), 2);
    assert_eq!(count("vault_items"), 3);
    assert_eq!(count("vault_activity"), 2);
    assert_eq!(
        std::fs::read(data.path().join("uploads").join(BLOB_KEY)).unwrap(),
        FILE_BYTES
    );

    // A second run refuses to touch a database that has users.
    let again = import(&options).await.unwrap_err().to_string();
    assert!(again.contains("already has"), "{again}");

    let db = Db::open(data.path(), 2).await.unwrap();
    let scalar = |sql: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, String>(sql)
                .fetch_one(&db.read)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        scalar("SELECT email FROM users WHERE id = 'user_ada'").await,
        "ada@example.com"
    );
    assert_eq!(
        scalar("SELECT approved_by FROM users WHERE id = 'user_bob'").await,
        "user_ada"
    );
    let eve_banned: bool = sqlx::query_scalar("SELECT banned FROM users WHERE id = 'user_eve'")
        .fetch_one(&db.read)
        .await
        .unwrap();
    assert!(
        !eve_banned,
        "an expired ban is lifted, as Better Auth would on sign-in"
    );
    let orphan: Option<String> =
        sqlx::query_scalar("SELECT created_by_user_id FROM api_keys WHERE id = 'key_orphan'")
            .fetch_one(&db.read)
            .await
            .unwrap();
    assert_eq!(orphan, None);
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&db.read)
        .await
        .unwrap();
    assert_eq!(sessions, 0, "sessions are not imported");
    assert_eq!(
        scalar("SELECT CAST(default_max_upload_bytes AS TEXT) FROM server_settings").await,
        "2147483648"
    );
    let state = AppState::new(&config(data.path()), db, oauth::Registry::default());
    let router = build_router(state);

    // The 2.x API key still works, scoped to its workspace.
    let (status, body) = call(
        &router,
        Request::builder()
            .uri("/api/v1/items?q=%E1%83%90%E1%83%9C%E1%83%92%E1%83%90%E1%83%A0")
            .header("x-api-key", LEGACY_KEY)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let listed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        listed["items"].as_array().unwrap().len(),
        1,
        "Georgian search works on imported text"
    );
    assert_eq!(
        listed["items"][0]["id"],
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(listed["items"][0]["tags"], json!(["finance", "ka"]));

    let (status, body) = call(
        &router,
        Request::builder()
            .uri("/api/v1/items/33333333-3333-4333-8333-333333333333/content?download=1")
            .header("x-api-key", LEGACY_KEY)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body, FILE_BYTES,
        "files are served from their original storage keys"
    );

    // The Better Auth password signs in (and is rehashed).
    let (status, body) = call(
        &router,
        Request::builder()
            .method(Method::POST)
            .uri("/api/auth/sign-in/email")
            .header(header::ORIGIN, "https://vault.example.com")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"email": "ada@example.com", "password": LEGACY_PASSWORD}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
}
