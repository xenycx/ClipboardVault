//! End-to-end tests of the router against a temporary SQLite database and upload
//! folder. No Docker, PostgreSQL or second service is needed.

use std::path::PathBuf;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Method, Request, StatusCode, header},
};
use clipboard_vault::{AppState, build_router, config::Config, db::Db, identity::oauth, uploads};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE: &str = "https://vault.example.com";
const SETUP_TOKEN: &str = "setup-token-0123456789-0123456789-abcdef";
// Produced by Better Auth 1.6.23 (scrypt N=16384 r=16 p=1, hex salt string as salt).
const LEGACY_PASSWORD_HASH: &str = "06f2dfcd5e94507bfc19b341cbe98a90:f5a0b7f0eb93538e3b751f3516fa8215b6057c61ea3ca7d1909cf7b73711d646deb1c354dc74b3a397c4ed0cd35b6932a1d19fdca6e854a6f43a35b1667e3836";
const LEGACY_PASSWORD: &str = "correct horse battery";
// A 2.x key and the SHA-256/base64url hash Better Auth stored for it.
const LEGACY_KEY: &str = "cv_live_AbCdEfGhIjKlMnOpQrStUvWxYzAbCdEfGhIjKlMnOpQrStUvWxYzAbCdEfGhIjKl";
const LEGACY_KEY_HASH: &str = "hrxplUbqtwmqu4OzxpwbjjtfCauGcy60-AcJVJzpz78";

struct App {
    router: Router,
    state: AppState,
    _directory: tempfile::TempDir,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn session_cookie(&self) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| {
                value.starts_with("vault_session=") && !value.starts_with("vault_session=;")
            })
            .map(|value| value.split(';').next().unwrap().to_owned())
    }
    fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
    }
}

async fn app() -> App {
    let directory = tempfile::tempdir().unwrap();
    let data_dir: PathBuf = directory.path().to_owned();
    let config = Config {
        port: 0,
        upload_root: data_dir.join("uploads"),
        data_dir: data_dir.clone(),
        public_base_url: BASE.into(),
        vault_secret: "vault-secret-0123456789-0123456789-abcdef".into(),
        setup_token: SETUP_TOKEN.into(),
        trust_proxy_headers: false,
        database_read_connections: 4,
        backup_keep: 7,
        server_max_upload_bytes: 10 * 1024 * 1024 * 1024,
        legacy_max_upload_bytes: 100 * 1024 * 1024,
        upload_disk_reserve_bytes: 0,
        tus_chunk_size_bytes: 1024,
        tus_session_ttl_seconds: 3600,
    };
    std::fs::create_dir_all(config.upload_root.join("tmp")).unwrap();
    let db = Db::open(&data_dir, 4).await.unwrap();
    let state = AppState::new(&config, db, oauth::Registry::default());
    App {
        router: build_router(state.clone()),
        state,
        _directory: directory,
    }
}

impl App {
    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn post_json(&self, uri: &str, body: Value, cookie: Option<&str>) -> Reply {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::ORIGIN, BASE)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.send(request.body(Body::from(body.to_string())).unwrap())
            .await
    }

    async fn get(&self, uri: &str, cookie: Option<&str>) -> Reply {
        let mut request = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    async fn post_form(&self, uri: &str, form: &str, cookie: &str) -> Reply {
        self.send(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header(header::ORIGIN, BASE)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form.to_owned()))
                .unwrap(),
        )
        .await
    }

    async fn api(&self, method: Method, uri: &str, key: &str, body: Option<Value>) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-api-key", key);
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        self.send(request.body(body).unwrap()).await
    }

    async fn sign_up(&self, name: &str, email: &str, password: &str) -> String {
        let reply = self
            .post_json(
                "/api/auth/sign-up/email",
                json!({"name": name, "email": email, "password": password}),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        reply
            .session_cookie()
            .expect("sign-up sets a session cookie")
    }

    async fn sign_in(&self, email: &str, password: &str) -> Reply {
        self.post_json(
            "/api/auth/sign-in/email",
            json!({"email": email, "password": password}),
            None,
        )
        .await
    }

    /// Signs up the first account and claims administrator with the setup token.
    async fn admin(&self) -> String {
        let cookie = self
            .sign_up("Ada Admin", "ada@example.com", "a strong password")
            .await;
        let reply = self
            .post_json(
                "/api/auth/vault/bootstrap",
                json!({"token": SETUP_TOKEN}),
                Some(&cookie),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        cookie
    }

    async fn create_key(&self, cookie: &str, extra: &str) -> String {
        let reply = self
            .post_form(
                "/keys/create",
                &format!("name=Script&expires_in_days=30{extra}"),
                cookie,
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        let html = reply.text();
        let start = html
            .find("cv_live_")
            .expect("the created key is shown once");
        html[start..start + 72].to_owned()
    }

    async fn scalar(&self, sql: &str) -> String {
        sqlx::query_scalar::<_, String>(sql)
            .fetch_one(&self.state.db.read)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn first_administrator_setup_flow() {
    let app = app().await;
    // Auth writes without this server's Origin are refused, as Better Auth did.
    let forged = app
        .send(
            Request::builder()
                .method(Method::POST)
                .uri("/api/auth/sign-up/email")
                .header(header::ORIGIN, "https://evil.example.com")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"name": "x", "email": "x@example.com", "password": "12345678"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert_eq!(forged.json()["code"], "INVALID_ORIGIN");

    let cookie = app
        .sign_up("Ada", "Ada@Example.com", "a strong password")
        .await;
    assert_eq!(app.get("/", Some(&cookie)).await.location(), "/pending");
    assert_eq!(app.get("/setup", None).await.status, StatusCode::OK);
    assert_eq!(app.get("/", None).await.location(), "/login");

    let wrong = app
        .post_json(
            "/api/auth/vault/bootstrap",
            json!({"token": "nope"}),
            Some(&cookie),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    let anonymous = app
        .post_json(
            "/api/auth/vault/bootstrap",
            json!({"token": SETUP_TOKEN}),
            None,
        )
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    let claimed = app
        .post_json(
            "/api/auth/vault/bootstrap",
            json!({"token": SETUP_TOKEN}),
            Some(&cookie),
        )
        .await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.text());
    let again = app
        .post_json(
            "/api/auth/vault/bootstrap",
            json!({"token": SETUP_TOKEN}),
            Some(&cookie),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    let dashboard = app.get("/", Some(&cookie)).await;
    assert_eq!(dashboard.status, StatusCode::OK);
    assert!(
        dashboard.text().contains("s Vault"),
        "a personal workspace exists"
    );
    assert_eq!(app.get("/setup", None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        app.get("/admin", Some(&cookie)).await.status,
        StatusCode::OK
    );

    // Emails are case-insensitive and stored lower-case.
    let duplicate = app
        .post_json(
            "/api/auth/sign-up/email",
            json!({"name": "Again", "email": "ADA@example.com", "password": "another password"}),
            None,
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        duplicate.json()["code"],
        "USER_ALREADY_EXISTS_USE_ANOTHER_EMAIL"
    );
    assert_eq!(
        app.sign_in("ada@EXAMPLE.com", "a strong password")
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn sessions_are_hashed_revocable_and_slide() {
    let app = app().await;
    let cookie = app.admin().await;
    let token = cookie.trim_start_matches("vault_session=");
    let stored = app.scalar("SELECT token_hash FROM sessions LIMIT 1").await;
    assert_ne!(stored, token, "only a hash of the token is stored");
    assert_eq!(stored.len(), 64);

    // A session last extended more than a day ago is extended and its cookie re-issued.
    sqlx::query("UPDATE sessions SET updated_at = '2000-01-01T00:00:00.000000Z'")
        .execute(&app.state.db.write)
        .await
        .unwrap();
    let before = app.scalar("SELECT expires_at FROM sessions").await;
    let page = app.get("/keys", Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.session_cookie().is_some(), "the cookie is refreshed");
    assert!(app.scalar("SELECT expires_at FROM sessions").await >= before);
    // Within a day nothing is rewritten.
    assert!(
        app.get("/keys", Some(&cookie))
            .await
            .session_cookie()
            .is_none()
    );

    let out = app
        .post_json("/api/auth/sign-out", json!({}), Some(&cookie))
        .await;
    assert_eq!(out.status, StatusCode::OK);
    assert_eq!(app.get("/", Some(&cookie)).await.location(), "/login");

    // Expired sessions are refused.
    let fresh = app
        .sign_in("ada@example.com", "a strong password")
        .await
        .session_cookie()
        .unwrap();
    sqlx::query("UPDATE sessions SET expires_at = '2000-01-01T00:00:00.000000Z'")
        .execute(&app.state.db.write)
        .await
        .unwrap();
    assert_eq!(app.get("/", Some(&fresh)).await.location(), "/login");
}

#[tokio::test]
async fn items_search_tags_trash_and_export() {
    let app = app().await;
    let cookie = app.admin().await;
    let key = app.create_key(&cookie, "&read=on&delete=on").await;

    let first = app
        .api(
            Method::POST,
            "/api/v1/items",
            &key,
            Some(json!({"payload": "deploy logs from prod", "tags": ["Ops", "logs"]})),
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.text());
    let first_id = first.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(first.json()["tags"], json!(["ops", "logs"]));
    let georgian = app
        .api(
            Method::POST,
            "/api/v1/items",
            &key,
            Some(json!({"payload": "ყოველთვიური ანგარიში", "virtual_path": "/reports/2026"})),
        )
        .await;
    assert_eq!(georgian.status, StatusCode::CREATED);
    app.api(
        Method::POST,
        "/api/v1/items",
        &key,
        Some(json!({"payload": "https://example.com/a"})),
    )
    .await;

    let search = |q: &str| {
        let uri = format!("/api/v1/items?q={}", url_encode(q));
        let key = key.clone();
        let app = &app;
        async move {
            app.api(Method::GET, &uri, &key, None).await.json()["items"]
                .as_array()
                .unwrap()
                .len()
        }
    };
    assert_eq!(search("depl").await, 1, "prefix search");
    assert_eq!(search("deploy prod").await, 1, "every word must match");
    assert_eq!(search("deploy missing").await, 0);
    assert_eq!(search("ანგარ").await, 1, "Georgian prefix search");
    assert_eq!(search("reports").await, 1, "virtual path is searchable");
    assert_eq!(
        search("depl\"*:(-^").await,
        1,
        "FTS syntax in the input is inert"
    );
    assert_eq!(
        search("!!!").await,
        3,
        "no searchable words means no filter"
    );

    let tagged = app
        .api(Method::GET, "/api/v1/items?tag=ops", &key, None)
        .await;
    assert_eq!(tagged.json()["items"].as_array().unwrap().len(), 1);
    let tags = app.api(Method::GET, "/api/v1/tags", &key, None).await;
    assert_eq!(tags.json()["tags"], json!(["logs", "ops"]));

    let page = app.get("/?q=depl", Some(&cookie)).await;
    assert!(page.text().contains("deploy logs from prod"));
    assert!(!page.text().contains("ყოველთვიური"));

    assert_eq!(
        search("ops").await,
        0,
        "tags are not part of the text index"
    );
    let pinned = app
        .api(
            Method::PATCH,
            &format!("/api/v1/items/{first_id}"),
            &key,
            Some(json!({"pinned": true, "virtual_path": "/ops"})),
        )
        .await;
    assert_eq!(pinned.json()["pinned"], true);
    assert_eq!(search("ops").await, 1, "a renamed path is reindexed");
    let listed = app
        .api(Method::GET, "/api/v1/items?limit=2", &key, None)
        .await
        .json();
    assert_eq!(
        listed["items"][0]["id"],
        first_id.as_str(),
        "pinned items come first"
    );
    let cursor = listed["next_cursor"]
        .as_str()
        .expect("a second page exists")
        .to_owned();
    let next = app
        .api(
            Method::GET,
            &format!("/api/v1/items?before={}", url_encode(&cursor)),
            &key,
            None,
        )
        .await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.text());

    assert_eq!(
        app.api(
            Method::DELETE,
            &format!("/api/v1/items/{first_id}"),
            &key,
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    let trash = app
        .api(Method::GET, "/api/v1/items?trash=true", &key, None)
        .await;
    assert_eq!(trash.json()["items"].as_array().unwrap().len(), 1);
    let restored = app
        .api(
            Method::POST,
            &format!("/api/v1/items/{first_id}/restore"),
            &key,
            None,
        )
        .await;
    assert_eq!(restored.status, StatusCode::OK);

    let csv = app
        .api(Method::GET, "/api/v1/items/export?format=csv", &key, None)
        .await;
    assert!(csv.text().starts_with("id,type,filename"));
    assert!(csv.text().contains("ყოველთვიური"));

    let all: Vec<String> = app
        .api(Method::GET, "/api/v1/items", &key, None)
        .await
        .json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect();
    let bulk = app
        .api(
            Method::POST,
            "/api/v1/items/bulk",
            &key,
            Some(json!({"action": "trash", "ids": all})),
        )
        .await;
    assert_eq!(bulk.json()["affected"], 3);
    let emptied = app
        .api(Method::POST, "/api/v1/trash/empty", &key, None)
        .await;
    assert_eq!(emptied.json()["purged"], 3);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM vault_items_fts")
        .fetch_one(&app.state.db.read)
        .await
        .unwrap();
    assert_eq!(count, 0, "the search index follows deletions");
}

#[tokio::test]
async fn api_keys_are_scoped_by_permission_and_workspace() {
    let app = app().await;
    let cookie = app.admin().await;
    let write_only = app.create_key(&cookie, "").await;
    let created = app
        .api(
            Method::POST,
            "/api/v1/items",
            &write_only,
            Some(json!({"payload": "hello"})),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(
        app.api(Method::GET, "/api/v1/items", &write_only, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "a write-only key cannot read"
    );
    assert_eq!(
        app.api(Method::GET, "/api/v1/items", "cv_live_not-a-real-key", None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let stored = app.scalar("SELECT key_hash FROM api_keys LIMIT 1").await;
    assert!(!stored.starts_with("cv_live_"), "only the hash is stored");

    let id = app.scalar("SELECT id FROM api_keys LIMIT 1").await;
    assert!(
        app.get("/keys", Some(&cookie))
            .await
            .text()
            .contains(&format!("/keys/{id}/revoke"))
    );
    let revoked = app
        .post_form(&format!("/keys/{id}/revoke"), "", &cookie)
        .await;
    assert_eq!(revoked.status, StatusCode::SEE_OTHER);
    assert_eq!(
        app.api(
            Method::POST,
            "/api/v1/items",
            &write_only,
            Some(json!({"payload": "x"}))
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED,
        "revoked keys stop working"
    );
}

#[tokio::test]
async fn credentials_imported_from_better_auth_keep_working() {
    let app = app().await;
    let now = "2026-01-01T00:00:00.000000Z";
    for statement in [
        format!(
            "INSERT INTO users (id, name, email, password_hash, role, approval_status, created_at, updated_at) VALUES ('legacyUser', 'Legacy', 'legacy@example.com', '{LEGACY_PASSWORD_HASH}', 'user', 'approved', '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO workspaces (id, name, slug, created_at) VALUES ('legacyWs', 'Legacy''s Vault', 'legacy', '{now}')"
        ),
        format!(
            "INSERT INTO memberships (id, workspace_id, user_id, role, created_at) VALUES ('m1', 'legacyWs', 'legacyUser', 'owner', '{now}')"
        ),
        format!(
            "INSERT INTO api_keys (id, workspace_id, created_by_user_id, name, start, key_hash, permissions, enabled, expires_at, created_at, updated_at) VALUES ('legacyKey', 'legacyWs', 'legacyUser', 'old', 'cv_liv', '{LEGACY_KEY_HASH}', '{{\"items\":[\"write\",\"read\"]}}', 1, '2099-01-01T00:00:00.000000Z', '{now}', '{now}')"
        ),
    ] {
        sqlx::query(&statement)
            .execute(&app.state.db.write)
            .await
            .unwrap();
    }
    let listed = app
        .api(Method::GET, "/api/v1/items", LEGACY_KEY, None)
        .await;
    assert_eq!(
        listed.status,
        StatusCode::OK,
        "a 2.x API key verifies: {}",
        listed.text()
    );
    let created = app
        .api(
            Method::POST,
            "/api/v1/items",
            LEGACY_KEY,
            Some(json!({"payload": "from vault.py"})),
        )
        .await;
    assert_eq!(
        created.json()["organization_id"],
        "legacyWs",
        "the key decides the workspace"
    );

    assert_eq!(
        app.sign_in("legacy@example.com", "wrong password")
            .await
            .json()["code"],
        "INVALID_EMAIL_OR_PASSWORD"
    );
    let signed_in = app.sign_in("legacy@example.com", LEGACY_PASSWORD).await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.text());
    let rehashed = app
        .scalar("SELECT password_hash FROM users WHERE id = 'legacyUser'")
        .await;
    assert!(
        rehashed.starts_with("$argon2id$"),
        "legacy hashes are replaced on sign-in"
    );
    assert_eq!(
        app.sign_in("legacy@example.com", LEGACY_PASSWORD)
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn multipart_and_resumable_uploads() {
    let app = app().await;
    let cookie = app.admin().await;
    let key = app.create_key(&cookie, "&read=on&delete=on").await;

    let boundary = "XBOUNDARYX";
    let multipart = |name: &str, content: &str| {
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"tags\"\r\n\r\nfiles\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: text/plain\r\n\r\n{content}\r\n--{boundary}--\r\n"
        )
    };
    let upload = |body: String| {
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/items")
            .header("x-api-key", &key)
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap()
    };
    let first = app.send(upload(multipart("notes.txt", "hello file"))).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.text());
    let second = app.send(upload(multipart("copy.txt", "hello file"))).await;
    assert_eq!(
        first.json()["blob_id"],
        second.json()["blob_id"],
        "identical files share a blob"
    );
    let blobs: i64 = sqlx::query_scalar("SELECT count(*) FROM vault_blobs")
        .fetch_one(&app.state.db.read)
        .await
        .unwrap();
    assert_eq!(blobs, 1);
    let id = first.json()["id"].as_str().unwrap().to_owned();
    let ranged = app
        .send(
            Request::builder()
                .uri(format!("/api/v1/items/{id}/content"))
                .header("x-api-key", &key)
                .header(header::RANGE, "bytes=6-9")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.text(), "file");

    // Tus: create, two chunks, a stale offset, finalization.
    let data: Vec<u8> = (0..1500_u32).map(|n| (n % 251) as u8).collect();
    let created = app
        .send(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/uploads")
                .header("x-api-key", &key)
                .header("tus-resumable", "1.0.0")
                .header("upload-length", data.len().to_string())
                .header("upload-metadata", "filename YmlnLmJpbg==")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let location = created.location().trim_start_matches(BASE).to_owned();
    let patch = |offset: usize, chunk: &[u8]| {
        Request::builder()
            .method(Method::PATCH)
            .uri(&location)
            .header("x-api-key", &key)
            .header("tus-resumable", "1.0.0")
            .header("upload-offset", offset.to_string())
            .header(header::CONTENT_TYPE, "application/offset+octet-stream")
            .header(header::CONTENT_LENGTH, chunk.len().to_string())
            .body(Body::from(chunk.to_vec()))
            .unwrap()
    };
    let one = app.send(patch(0, &data[..1000])).await;
    assert_eq!(one.status, StatusCode::NO_CONTENT, "{}", one.text());
    assert_eq!(one.headers["upload-offset"], "1000");
    let stale = app.send(patch(0, &data[..1000])).await;
    assert_eq!(
        stale.status,
        StatusCode::CONFLICT,
        "the server offset is authoritative"
    );
    assert_eq!(stale.json()["error"]["detail"]["expectedOffset"], 1000);
    let two = app.send(patch(1000, &data[1000..])).await;
    assert_eq!(two.status, StatusCode::NO_CONTENT);
    uploads::maintenance_tick(&app.state).await.unwrap();
    let status = app
        .api(Method::GET, &format!("{location}/status"), &key, None)
        .await
        .json();
    assert_eq!(status["state"], "completed", "{status}");
    let item_id = status["itemId"].as_str().unwrap();
    let content = app
        .api(
            Method::GET,
            &format!("/api/v1/items/{item_id}/content?download=1"),
            &key,
            None,
        )
        .await;
    assert_eq!(content.body, data);
    let partials = std::fs::read_dir(app.state.upload_root.join("tmp"))
        .unwrap()
        .count();
    assert_eq!(partials, 0, "no partial files are left behind");

    let storage = app.get("/api/v1/storage", Some(&cookie)).await;
    assert_eq!(storage.status, StatusCode::OK);
    assert_eq!(storage.json()["workspaceBytes"], 10 + 1500);
}

#[tokio::test]
async fn administrators_manage_accounts_within_the_safety_rules() {
    let app = app().await;
    let cookie = app.admin().await;
    let created = app
        .post_form(
            "/admin/users/create",
            "name=Bea&email=bea%40example.com&password=bea+password+1&role=user",
            &cookie,
        )
        .await;
    assert_eq!(created.location(), "/admin?notice=created");
    let bea_id = app
        .scalar("SELECT id FROM users WHERE email = 'bea@example.com'")
        .await;
    let ada_id = app
        .scalar("SELECT id FROM users WHERE email = 'ada@example.com'")
        .await;
    let bea = app
        .sign_in("bea@example.com", "bea password 1")
        .await
        .session_cookie()
        .unwrap();
    assert_eq!(
        app.get("/", Some(&bea)).await.status,
        StatusCode::OK,
        "admin-created accounts are approved"
    );
    assert_eq!(
        app.get("/admin", Some(&bea)).await.status,
        StatusCode::FORBIDDEN
    );

    let self_ban = app
        .post_form(&format!("/admin/users/{ada_id}/ban"), "", &cookie)
        .await;
    assert!(
        self_ban
            .text()
            .contains("You cannot disable your own account.")
    );
    let self_demote = app
        .post_form(&format!("/admin/users/{ada_id}/role"), "role=user", &cookie)
        .await;
    assert!(
        self_demote
            .text()
            .contains("You cannot remove your own administrator role.")
    );

    let banned = app
        .post_form(&format!("/admin/users/{bea_id}/ban"), "", &cookie)
        .await;
    assert_eq!(banned.location(), "/admin?notice=banned");
    assert_eq!(
        app.get("/", Some(&bea)).await.location(),
        "/login",
        "banning signs out"
    );
    assert_eq!(
        app.sign_in("bea@example.com", "bea password 1")
            .await
            .json()["code"],
        "BANNED_USER"
    );
    app.post_form(&format!("/admin/users/{bea_id}/unban"), "", &cookie)
        .await;

    let reset = app
        .post_form(&format!("/admin/users/{bea_id}/reset"), "", &cookie)
        .await;
    let html = reset.text();
    let start = html.find("token=").unwrap() + 6;
    let token: String = html[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let changed = app
        .post_json(
            "/api/auth/reset-password",
            json!({"token": token, "newPassword": "brand new password"}),
            None,
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.text());
    let reused = app
        .post_json(
            "/api/auth/reset-password",
            json!({"token": token, "newPassword": "another new password"}),
            None,
        )
        .await;
    assert_eq!(
        reused.status,
        StatusCode::BAD_REQUEST,
        "reset links work once"
    );
    assert_eq!(
        app.sign_in("bea@example.com", "brand new password")
            .await
            .status,
        StatusCode::OK
    );

    let wrong_confirmation = app
        .post_form(
            &format!("/admin/users/{bea_id}/delete"),
            "confirmation=nope",
            &cookie,
        )
        .await;
    assert!(wrong_confirmation.text().contains("Type the account"));
    let deleted = app
        .post_form(
            &format!("/admin/users/{bea_id}/delete"),
            "confirmation=BEA%40example.com",
            &cookie,
        )
        .await;
    assert_eq!(deleted.location(), "/admin?notice=deleted");
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM workspaces")
        .fetch_one(&app.state.db.read)
        .await
        .unwrap();
    assert_eq!(
        remaining, 1,
        "the deleted account's own workspace went with it"
    );

    // Registration can be closed and approval switched off.
    app.post_form("/admin/settings", "default_max_upload_mb=100", &cookie)
        .await;
    let closed = app
        .post_json(
            "/api/auth/sign-up/email",
            json!({"name": "C", "email": "c@example.com", "password": "12345678"}),
            None,
        )
        .await;
    assert_eq!(closed.json()["code"], "REGISTRATION_CLOSED");
    app.post_form(
        "/admin/settings",
        "registration_enabled=on&default_max_upload_mb=100",
        &cookie,
    )
    .await;
    let carl = app
        .sign_up("Carl", "carl@example.com", "carl password")
        .await;
    assert_eq!(
        app.get("/", Some(&carl)).await.status,
        StatusCode::OK,
        "no approval required now"
    );

    let snapshot = app.get("/admin/backup", Some(&cookie)).await;
    assert_eq!(snapshot.status, StatusCode::OK);
    assert!(snapshot.body.starts_with(b"SQLite format 3\0"));
    assert_eq!(
        app.get("/admin/backup", Some(&carl)).await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn workspace_membership_rules() {
    let app = app().await;
    let owner = app.admin().await;
    let invite = app
        .post_form("/invitations/create", "role=member", &owner)
        .await
        .text();
    let start = invite.find("/join/").unwrap() + 6;
    let token: String = invite[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();

    let member = app.sign_up("Mia", "mia@example.com", "mia password").await;
    let pending_join = app.post_form(&format!("/join/{token}"), "", &member).await;
    assert_eq!(
        pending_join.status,
        StatusCode::FORBIDDEN,
        "pending accounts cannot join"
    );
    let mia_id = app
        .scalar("SELECT id FROM users WHERE email = 'mia@example.com'")
        .await;
    app.post_form(&format!("/admin/users/{mia_id}/approve"), "", &owner)
        .await;
    let joined = app.post_form(&format!("/join/{token}"), "", &member).await;
    assert_eq!(joined.location(), "/");
    assert_eq!(
        app.post_form(&format!("/join/{token}"), "", &member)
            .await
            .status,
        StatusCode::NOT_FOUND,
        "invitations work once"
    );

    let shared = app
        .scalar("SELECT workspace_id FROM memberships WHERE role = 'member'")
        .await;
    let member_in_shared = format!("{member}; vault_workspace={shared}");
    let ada_id = app
        .scalar("SELECT id FROM users WHERE email = 'ada@example.com'")
        .await;
    let refused = app
        .post_form(
            &format!("/workspaces/members/{ada_id}/remove"),
            "",
            &member_in_shared,
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "members cannot manage members"
    );

    let bad_delete = app
        .post_form("/workspaces/delete", "confirmation=wrong", &owner)
        .await;
    assert_eq!(bad_delete.status, StatusCode::BAD_REQUEST);
    let transferred = app
        .post_form(
            &format!("/workspaces/members/{mia_id}/transfer"),
            "",
            &owner,
        )
        .await;
    assert_eq!(transferred.location(), "/keys");
    assert_eq!(
        app.scalar(&format!(
            "SELECT role FROM memberships WHERE user_id = '{mia_id}' AND workspace_id = '{shared}'"
        ))
        .await,
        "owner"
    );
    assert_eq!(
        app.scalar(&format!(
            "SELECT role FROM memberships WHERE user_id = '{ada_id}' AND workspace_id = '{shared}'"
        ))
        .await,
        "admin"
    );

    // A workspace cookie naming a workspace the user is not in is ignored.
    let foreign = format!("{owner}; vault_workspace=not-mine");
    assert_eq!(app.get("/", Some(&foreign)).await.status, StatusCode::OK);
}

#[tokio::test]
async fn sign_in_is_rate_limited() {
    let app = app().await;
    for _ in 0..10 {
        assert_eq!(
            app.sign_in("nobody@example.com", "whatever1").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    let limited = app.sign_in("nobody@example.com", "whatever1").await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.json()["code"], "TOO_MANY_REQUESTS");
}

#[tokio::test]
async fn social_callbacks_need_the_signed_state_cookie() {
    let app = app().await;
    let providers = app.get("/api/auth/vault/providers", None).await;
    assert_eq!(providers.json(), json!({"data": {"providers": []}}));
    assert_eq!(providers.headers[header::CACHE_CONTROL], "no-store");
    let unknown = app
        .post_json(
            "/api/auth/sign-in/social",
            json!({"provider": "github"}),
            None,
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let callback = app
        .get("/api/auth/callback/github?code=x&state=y", None)
        .await;
    assert_eq!(callback.status, StatusCode::FOUND);
    assert_eq!(callback.location(), "/login?error=state_mismatch");
    assert_eq!(
        app.get("/api/auth/internal/session", None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get("/internal/session", None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn health_and_security_headers() {
    let app = app().await;
    assert_eq!(app.get("/health/live", None).await.status, StatusCode::OK);
    let ready = app.get("/health/ready", None).await;
    assert_eq!(ready.json(), json!({"status": "ready"}));
    let login = app.get("/login", None).await;
    assert_eq!(login.headers["cache-control"], "no-store");
    assert!(
        login.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'self'")
    );
}

fn url_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
