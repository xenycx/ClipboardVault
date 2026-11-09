import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def rust_sources() -> dict[str, str]:
    """Every Rust source file under src/, keyed by its path relative to src/."""
    return {
        path.relative_to(ROOT / "src").as_posix(): path.read_text(encoding="utf-8")
        for path in sorted((ROOT / "src").rglob("*.rs"))
    }


def code_only(source: str) -> str:
    """Rust source without line comments, so prose about old behavior is ignored."""
    return "\n".join(line for line in source.splitlines() if not line.lstrip().startswith("//"))


def env_example() -> dict[str, str]:
    return dict(
        line.split("=", 1)
        for line in (ROOT / ".env.example").read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#") and "=" in line
    )


def test_browser_never_reads_clipboard() -> None:
    client = (ROOT / "static" / "app.js").read_text(encoding="utf-8")
    forbidden = (
        "navigator.clipboard.read(",
        "navigator.clipboard.readText(",
        "clipboardchange",
        "setInterval(pollClipboard",
    )
    for value in forbidden:
        assert value not in client
    assert "navigator.clipboard.writeText(" in client


def test_chatgpt_headers_are_not_authentication() -> None:
    source = "\n".join(rust_sources().values())
    assert 'remove("oai-authenticated-user-email")' in source
    assert "signin-with-chatgpt" not in source.lower()


def test_upload_paths_are_virtual() -> None:
    storage = (ROOT / "src" / "storage.rs").read_text(encoding="utf-8")
    assert 'virtual_path cannot contain ..' in storage
    assert 'Uuid::new_v4()' in storage


def test_legacy_platform_entrypoints_are_gone() -> None:
    forbidden = (
        ROOT / ".openai" / "hosting.json",
        ROOT / "worker" / "index.ts",
        ROOT / "drizzle.config.ts",
        ROOT / "app" / "chatgpt-auth.ts",
    )
    assert not any(path.exists() for path in forbidden)


def test_proxy_has_route_specific_streaming_limits() -> None:
    compose = (ROOT / "docker-compose.yml").read_text(encoding="utf-8")
    for variable in (
        "SERVER_MAX_UPLOAD_BYTES",
        "LEGACY_MAX_UPLOAD_BYTES",
        "TUS_CHUNK_SIZE_BYTES",
        "TUS_SESSION_TTL_SECONDS",
        "UPLOAD_DISK_RESERVE_BYTES",
        "NGINX_LEGACY_CLIENT_MAX_BODY_SIZE",
        "NGINX_TUS_CLIENT_MAX_BODY_SIZE",
    ):
        assert variable in compose

    for name in ("http.conf.template", "https.conf.template"):
        nginx = (ROOT / "nginx" / name).read_text(encoding="utf-8")
        assert "location = /api/v1/uploads" in nginx
        assert "location ^~ /api/v1/uploads/" in nginx
        assert "location = /api/v1/items" in nginx
        assert "location = /capture" in nginx
        assert "client_max_body_size ${NGINX_TUS_CLIENT_MAX_BODY_SIZE};" in nginx
        assert "client_max_body_size ${NGINX_LEGACY_CLIENT_MAX_BODY_SIZE};" in nginx
        assert nginx.count("proxy_request_buffering off;") >= 4
        assert nginx.count("proxy_buffering off;") >= 4


def test_resumable_example_streams_bounded_chunks() -> None:
    uploader = (ROOT / "examples" / "upload_resumable.py").read_text(encoding="utf-8")
    assert "Tus-Resumable" in uploader
    assert "Upload-Offset" in uploader
    assert "application/offset+octet-stream" in uploader
    assert "handle.read(min(chunk_bytes" in uploader
    assert "validated_upload_url" in uploader
    assert 'response.headers["Upload-Length"]' in uploader
    assert ".read()" not in uploader


def test_browser_discovers_chunk_limit_and_cleanup_checks_origin() -> None:
    client = (ROOT / "static" / "app.js").read_text(encoding="utf-8")
    api = (ROOT / "src" / "api.rs").read_text(encoding="utf-8")
    pages = (ROOT / "src" / "pages.rs").read_text(encoding="utf-8")
    assert 'response.headers.get("X-Tus-Chunk-Size")' in client
    assert "require_same_origin(&state, &headers)" in api
    assert "api::require_same_origin(&state, &headers)" in pages


def test_removed_backup_workflow_is_not_referenced() -> None:
    assert not (ROOT / "scripts" / "backup.sh").exists()
    for name in ("README.md", "DEPLOYMENT.md"):
        document = (ROOT / name).read_text(encoding="utf-8")
        assert "scripts/backup.sh" not in document


def test_templates_contain_no_inline_scripts_or_handlers() -> None:
    """The page CSP is script-src 'self' with no nonce, so inline JavaScript is dead code."""
    for template in (ROOT / "templates").glob("*.html"):
        markup = template.read_text(encoding="utf-8")
        assert "<script>" not in markup, f"{template.name} has an inline script block"
        for handler in ("onclick=", "onchange=", "onsubmit=", "onload=", "oninput=", "onerror="):
            assert handler not in markup, f"{template.name} has an inline {handler} handler"


def test_templates_reference_static_assets_through_the_version_filter() -> None:
    """Unversioned asset URLs let a browser pair new HTML with a cached old stylesheet."""
    pattern = re.compile(r'(?:src|href)="(/static/[^"?]+)"')
    for template in (ROOT / "templates").glob("*.html"):
        stale = pattern.findall(template.read_text(encoding="utf-8"))
        assert not stale, f"{template.name} references {stale} without the |asset filter"


def test_static_responses_are_cached_and_pages_are_not() -> None:
    source = (ROOT / "src" / "lib.rs").read_text(encoding="utf-8")
    assert "public, max-age=31536000, immutable" in source
    assert '"cache-control", "no-store"' in source


def test_every_referenced_asset_exists_on_disk() -> None:
    """A typo in an asset path is a 404 that only shows up in a browser."""
    pattern = re.compile(r'"(/static/[^"?]+)"')
    for template in (ROOT / "templates").glob("*.html"):
        for reference in pattern.findall(template.read_text(encoding="utf-8")):
            asset = ROOT / reference.lstrip("/")
            assert asset.is_file(), f"{template.name} references missing {reference}"


def test_icon_symbols_used_by_templates_are_defined() -> None:
    """Icons are <use> references into the sprite in base.html; a missing symbol renders blank."""
    base = (ROOT / "templates" / "base.html").read_text(encoding="utf-8")
    defined = set(re.findall(r'<symbol id="([^"]+)"', base))
    assert defined, "the icon sprite is missing from base.html"
    for template in (ROOT / "templates").glob("*.html"):
        markup = template.read_text(encoding="utf-8")
        for used in re.findall(r'<use href="#(i-[a-z-]+)"', markup):
            assert used in defined, f"{template.name} uses undefined icon #{used}"


def test_paste_reads_only_the_users_own_paste_gesture() -> None:
    """Ctrl+V uploads read the paste event the person triggered, never the clipboard API."""
    client = (ROOT / "static" / "app.js").read_text(encoding="utf-8")
    assert 'addEventListener("paste"' in client
    assert "event.clipboardData" in client
    assert "navigator.clipboard.read" not in client


def test_bulk_and_trash_endpoints_require_delete_permission_and_origin() -> None:
    api = (ROOT / "src" / "api.rs").read_text(encoding="utf-8")
    for handler in ("pub async fn bulk_items", "pub async fn empty_trash"):
        body = api.split(handler, 1)[1].split("\npub ", 1)[0]
        assert 'authorize(&state, &headers, "items:delete")' in body
        assert "require_same_origin(&state, &headers)" in body
        assert "auth.organization_id" in body or "purge_trashed(&state, &auth" in body


def test_python_client_uses_only_the_standard_library() -> None:
    client = (ROOT / "static" / "vault.py").read_text(encoding="utf-8")
    imports = re.findall(r"^(?:from|import) ([a-z0-9_.]+)", client, flags=re.MULTILINE)
    allowed = {"__future__", "argparse", "base64", "http.client", "json", "mimetypes", "os", "re",
               "sys", "threading", "time", "urllib.parse", "uuid", "concurrent.futures", "pathlib", "datetime",
               "shutil", "traceback", "unicodedata"}
    assert set(imports) <= allowed, set(imports) - allowed
    assert 'DEFAULT_SERVER = ""' in client and 'DEFAULT_API_KEY = ""' in client
    assert "cv_live_" not in client, "never ship a real key in the template"


def test_confirm_dialog_is_available_on_every_signed_in_page() -> None:
    # Revoke, remove-member and similar forms confirm through [data-confirm]. If the dialog only
    # exists on the vault page, every other page falls back to window.confirm(), which a browser
    # can suppress, and the action then silently does nothing.
    shell = (ROOT / "templates" / "app_base.html").read_text(encoding="utf-8")
    assert "data-confirm " in shell and "data-confirm-ok" in shell
    assert "data-confirm-ok" not in (ROOT / "templates" / "dashboard.html").read_text(encoding="utf-8")


def test_fonts_are_self_hosted() -> None:
    css = (ROOT / "static" / "app.css").read_text(encoding="utf-8")
    for reference in re.findall(r'url\("([^"]+)"\)', css):
        assert not reference.startswith(("http:", "https:", "//"))
        assert (ROOT / "static" / reference).is_file(), reference



# ---------------------------------------------------------------- 3.0: one service


def test_the_node_auth_service_and_its_bridge_are_gone() -> None:
    assert not (ROOT / "auth").exists(), "the Hono/Better Auth service was replaced by src/identity"
    assert not (ROOT / "src" / "auth_proxy.rs").exists()
    # config.rs names the retired variables on purpose, to warn when they are still set.
    source = "\n".join(code_only(text) for name, text in rust_sources().items() if name != "config.rs")
    for retired in ("x-auth-bridge-secret", "/internal/", "AuthBridge", "AUTH_BRIDGE_SECRET", "AUTH_INTERNAL_URL"):
        assert retired not in source, retired
    config = (ROOT / "src" / "config.rs").read_text(encoding="utf-8")
    retired_list = config.split("pub const RETIRED_VARIABLES", 1)[1].split("];", 1)[0]
    for name in ("DATABASE_URL", "BETTER_AUTH_SECRET", "AUTH_BRIDGE_SECRET", "BOOTSTRAP_TOKEN"):
        assert f'"{name}"' in retired_list, name
    compose = (ROOT / "docker-compose.yml").read_text(encoding="utf-8")
    assert "postgres" not in compose and "auth:" not in compose
    assert "internal: true" in compose, "the app network stays private behind nginx"
    for name in ("http.conf.template", "https.conf.template"):
        nginx = (ROOT / "nginx" / name).read_text(encoding="utf-8")
        assert "location /internal" not in nginx
        assert "auth:3001" not in nginx


def test_api_keys_are_workspace_bound_and_keep_the_2x_format() -> None:
    keys = (ROOT / "src" / "identity" / "keys.rs").read_text(encoding="utf-8")
    assert 'pub const PREFIX: &str = "cv_live_";' in keys
    # Better Auth stored SHA-256 of the whole key, base64url without padding.
    assert "URL_SAFE_NO_PAD.encode(Sha256::digest(key.as_bytes()))" in keys
    assert "organization_id: row.workspace_id" in keys, "the key row decides the workspace"
    assert "body.organizationId" not in (ROOT / "examples" / "upload_file.py").read_text(encoding="utf-8")
    auth = (ROOT / "src" / "auth.rs").read_text(encoding="utf-8")
    assert "organization_id: verified.organization_id" in auth


def test_large_upload_defaults_use_64_bit_safe_values() -> None:
    values = env_example()
    assert int(values["SERVER_MAX_UPLOAD_BYTES"]) == 10 * 1024**3
    assert int(values["LEGACY_MAX_UPLOAD_BYTES"]) == 100 * 1024**2
    assert int(values["TUS_CHUNK_SIZE_BYTES"]) == 16 * 1024**2
    assert int(values["TUS_SESSION_TTL_SECONDS"]) == 7 * 24 * 60 * 60
    assert int(values["UPLOAD_DISK_RESERVE_BYTES"]) == 20 * 1024**3
    schema = (ROOT / "migrations" / "0001_init.sql").read_text(encoding="utf-8")
    assert "max_upload_bytes INTEGER NOT NULL DEFAULT 10737418240" in schema


def test_sqlite_queries_carry_no_postgres_syntax() -> None:
    """The import tool reads PostgreSQL; everything else talks to SQLite."""
    forbidden = (
        (r"::\s*(?:bigint|int|uuid|text)\b", "a PostgreSQL cast"),
        (r"(?<!::)(?<!fn )\bnow\(\)", "SQL now(): timestamps are written by Rust"),
        (r"\$\d", "a PostgreSQL placeholder"),
        (r"FOR UPDATE", "row locks: use the single writer"),
        (r"pg_advisory", "advisory locks: use the single writer"),
        (r"=\s*ANY\(", "ANY(): use json_each"),
        (r"\bjsonb", "JSONB"),
        (r"to_tsvector|to_tsquery", "tsvector search: use FTS5"),
        (r"\bILIKE\b", "ILIKE cannot use the search index"),
    )
    for name, source in rust_sources().items():
        if name == "import.rs":
            continue
        code = code_only(source)
        for pattern, reason in forbidden:
            assert not re.search(pattern, code), f"src/{name} contains {reason}"


def test_timestamps_are_bound_in_the_one_stored_format() -> None:
    """Mixed timestamp formats would break ORDER BY and range comparisons on TEXT."""
    db = (ROOT / "src" / "db.rs").read_text(encoding="utf-8")
    assert 'value.format("%Y-%m-%dT%H:%M:%S%.6fZ")' in db
    for name, source in rust_sources().items():
        code = code_only(source)
        assert not re.search(r"\.bind\(\s*(?:chrono::)?Utc::now\(\)", code), f"src/{name} binds a raw DateTime"
        assert not re.search(r"\.bind\(\s*expires_at\s*\)", code), f"src/{name} binds a raw DateTime"


def test_all_writes_go_through_one_immediate_writer() -> None:
    db = (ROOT / "src" / "db.rs").read_text(encoding="utf-8")
    assert ".max_connections(1)" in db, "exactly one write connection"
    assert 'begin_with("BEGIN IMMEDIATE")' in db
    assert '.pragma("query_only", "ON")' in db, "the read pool refuses writes"
    assert "SqliteJournalMode::Wal" in db and ".foreign_keys(true)" in db
    for name, source in rust_sources().items():
        code = code_only(source)
        assert ".write.begin()" not in code, f"src/{name} opens a deferred write transaction; use db.begin()"
        assert ".read.begin()" not in code, f"src/{name} opens a transaction on the read pool"


def test_migrations_are_additive_so_image_rollback_works() -> None:
    db = (ROOT / "src" / "db.rs").read_text(encoding="utf-8")
    assert "set_ignore_missing(true)" in db
    for migration in sorted((ROOT / "migrations").glob("*.sql")):
        if migration.name == "0001_init.sql":
            continue
        code = "\n".join(
            line for line in migration.read_text(encoding="utf-8").splitlines() if not line.lstrip().startswith("--")
        ).upper()
        for statement in (part.strip() for part in code.split(";") if part.strip()):
            assert not statement.startswith(("DROP", "DELETE", "TRUNCATE")), f"{migration.name}: {statement[:60]}"
            assert "DROP COLUMN" not in statement and "RENAME" not in statement, f"{migration.name}: {statement[:60]}"


def test_search_uses_fts5_with_escaped_terms() -> None:
    api = (ROOT / "src" / "api.rs").read_text(encoding="utf-8")
    schema = (ROOT / "migrations" / "0001_init.sql").read_text(encoding="utf-8")
    assert "vault_items_fts MATCH" in api
    assert "tokenize = 'unicode61 remove_diacritics 2'" in schema
    assert "content_rowid = 'seq'" in schema, "the index must survive VACUUM renumbering"
    for trigger in ("vault_items_fts_insert", "vault_items_fts_delete", "vault_items_fts_update"):
        assert trigger in schema
    builder = api.split("pub(crate) fn search_query", 1)[1].split("\n}\n", 1)[0]
    assert "!character.is_alphanumeric()" in builder, "only letters and digits reach the query"
    assert ".push_bind(query)" in api, "the query is bound, never interpolated"


def test_sessions_store_only_token_hashes() -> None:
    identity = (ROOT / "src" / "identity" / "mod.rs").read_text(encoding="utf-8")
    create = identity.split("pub async fn create_session", 1)[1].split("\n}\n", 1)[0]
    assert ".bind(sha256_hex(&token))" in create
    assert ".bind(&token)" not in create and ".bind(token)" not in create
    assert "random_token(32)" in create, "32 random bytes per session"
    assert 'pub const SESSION_COOKIE: &str = "vault_session";' in identity
    assert "HttpOnly; SameSite=Lax" in identity
    assert "pub const SESSION_DAYS: i64 = 7;" in identity


def test_auth_writes_require_this_servers_origin() -> None:
    routes = (ROOT / "src" / "identity" / "routes.rs").read_text(encoding="utf-8")
    for handler in ("sign_in_email", "sign_up_email", "sign_out", "reset_password", "bootstrap", "sign_in_social"):
        body = routes.split(f"pub async fn {handler}(", 1)[1].split("\npub async fn ", 1)[0]
        assert "untrusted_origin(&state, &headers)" in body, f"{handler} skips the Origin check"
    check = routes.split("pub fn origin_is_trusted", 1)[1].split("\n}\n", 1)[0]
    assert "supplied.origin() == expected.origin()" in check
    assert "is_some_and" in check, "a request with no Origin or Referer is refused"


def test_auth_routes_are_rate_limited() -> None:
    routes = (ROOT / "src" / "identity" / "routes.rs").read_text(encoding="utf-8")
    limits = (ROOT / "src" / "identity" / "rate_limit.rs").read_text(encoding="utf-8")
    for rule in ("rate_limit::SIGN_IN", "rate_limit::SIGN_UP", "rate_limit::RESET_PASSWORD", "rate_limit::SETUP"):
        assert rule in routes, rule
    assert "pub const SIGN_IN: Rule = Rule {\n    max: 10," in limits
    assert "pub const API_KEY: Rule = Rule {\n    max: 120," in limits


def test_proxy_headers_are_untrusted_by_default() -> None:
    config = (ROOT / "src" / "config.rs").read_text(encoding="utf-8")
    assert 'trust_proxy_headers: flag("TRUST_PROXY_HEADERS")?' in config
    assert '"" | "0" | "false" | "no" | "off" => Ok(false)' in config
    limits = (ROOT / "src" / "identity" / "rate_limit.rs").read_text(encoding="utf-8")
    assert "if trust_proxy" in limits and "rsplit(',')" in limits, "only the proxy-appended address is used"


def test_legacy_password_hashes_are_verified_and_replaced() -> None:
    password = (ROOT / "src" / "identity" / "password.rs").read_text(encoding="utf-8")
    assert "scrypt::Params::new(14, 16, 1, 64)" in password, "Better Auth: N=16384, r=16, p=1, 64-byte key"
    assert "salt.as_bytes()" in password, "the hex salt string itself is the salt"
    assert "password.nfkc()" in password
    assert "ct_eq" in password
    routes = (ROOT / "src" / "identity" / "routes.rs").read_text(encoding="utf-8")
    assert "Verification::Valid { rehash }" in routes and "UPDATE users SET password_hash" in routes


def test_social_provider_registry_is_typed_and_secret_free() -> None:
    values = env_example()
    for provider in ("GOOGLE", "GITHUB", "DISCORD"):
        assert f"{provider}_CLIENT_ID" in values
        assert f"{provider}_CLIENT_SECRET" in values
    oauth = (ROOT / "src" / "identity" / "oauth.rs").read_text(encoding="utf-8")
    assert 'scopes: &["read:user", "user:email"]' in oauth
    assert 'scopes: &["identify", "email"]' in oauth
    assert 'scopes: &["openid", "email", "profile"]' in oauth
    public = oauth.split("pub struct PublicProvider", 1)[1].split("}", 1)[0]
    assert "secret" not in public.lower()
    assert "code_challenge_method" in oauth and "constant_time_eq(state, &pending.state)" in (
        ROOT / "src" / "identity" / "routes.rs"
    ).read_text(encoding="utf-8")
    routes = (ROOT / "src" / "identity" / "routes.rs").read_text(encoding="utf-8")
    assert 'HeaderValue::from_static("no-store")' in routes
    client = (ROOT / "static" / "app.js").read_text(encoding="utf-8")
    assert "data-google-login" not in client
    assert "errorCallbackURL" in client


def test_ci_builds_the_image_once_and_runs_the_import_test() -> None:
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    override = (ROOT / "docker-compose.ci.yml").read_text(encoding="utf-8")
    assert "branches: [main]" in workflow
    assert "cancel-in-progress: true" in workflow
    assert workflow.count("docker/build-push-action@v6") == 1
    assert "docker compose up -d --no-build" in workflow
    assert "docker compose up -d --no-deps --force-recreate vault" in workflow
    assert "IMPORT_TEST_DATABASE_URL" in workflow
    assert "start_interval: 1s" in override


def test_upgrade_path_snapshots_and_keeps_a_rollback_image() -> None:
    upgrade = (ROOT / "scripts" / "upgrade.sh").read_text(encoding="utf-8")
    assert "Docker Engine 25.0.0+ is required" in upgrade
    assert "Docker Compose 2.20.2+ is required" in upgrade
    assert "clipboard-vault backup" in upgrade, "a consistent snapshot precedes every upgrade"
    assert "check_not_2x" in upgrade, "2.x stacks must use the import runbook"
    assert "compose build vault" in upgrade and "compose build --pull vault" in upgrade
    assert "--no-build --wait --wait-timeout 120 vault" in upgrade
    assert "restore_image" in upgrade
    assert "pg_dump" not in upgrade


def test_runtime_image_is_a_single_static_binary() -> None:
    dockerfile = (ROOT / "Dockerfile").read_text(encoding="utf-8")
    runtime = dockerfile.split("AS runtime", 1)[1]
    assert "rust:1.97-alpine" in dockerfile, "musl builds a static binary"
    assert "distroless/static" in dockerfile
    assert "COPY static ./static" in runtime, "assets are served from the working directory"
    assert "USER 10001:10001" in runtime
    assert '"healthcheck"' in runtime, "distroless has no curl; the binary checks itself"
    assert "VOLUME [\"/data\"]" in runtime


def test_backups_use_vacuum_into() -> None:
    db = (ROOT / "src" / "db.rs").read_text(encoding="utf-8")
    backup = (ROOT / "src" / "backup.rs").read_text(encoding="utf-8")
    assert "VACUUM INTO" in db
    assert "state.db.snapshot(&partial)" in backup and "tokio::fs::rename(&partial, &target)" in backup
    lib = (ROOT / "src" / "lib.rs").read_text(encoding="utf-8")
    assert '"/admin/backup"' in lib
    pages = (ROOT / "src" / "pages.rs").read_text(encoding="utf-8")
    download = pages.split("pub async fn download_backup(", 1)[1].split("\npub async fn ", 1)[0]
    assert "require_global_admin(&session)" in download


def test_import_tool_never_writes_to_postgres() -> None:
    importer = code_only((ROOT / "src" / "import.rs").read_text(encoding="utf-8"))
    postgres_sql = re.findall(r'sqlx::query(?:_scalar)?(?:::<[^>]+>)?\(\s*r?#?"([^"]*)', importer)
    for statement in postgres_sql:
        upper = statement.upper()
        if "INSERT INTO" in upper or "UPDATE SERVER_SETTINGS" in upper or "PRAGMA" in upper:
            continue  # SQLite target statements
        assert not re.search(r"\b(INSERT|UPDATE|DELETE|DROP|ALTER|TRUNCATE)\b", upper), statement[:80]
    assert "already has" in importer, "the import refuses a database that has users"
