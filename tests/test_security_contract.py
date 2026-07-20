import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


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
    source = "\n".join(
        path.read_text(encoding="utf-8")
        for path in (ROOT / "src").glob("*.rs")
    )
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


def test_api_keys_are_workspace_bound() -> None:
    auth = (ROOT / "auth" / "src" / "auth.ts").read_text(encoding="utf-8")
    bridge = (ROOT / "auth" / "src" / "index.ts").read_text(encoding="utf-8")
    assert 'references: "organization"' in auth
    assert 'organizationId: result.key.referenceId' in bridge
    assert 'body.organizationId' not in (ROOT / "examples" / "upload_file.py").read_text(encoding="utf-8")


def test_private_bridge_is_not_publicly_proxied() -> None:
    compose = (ROOT / "docker-compose.yml").read_text(encoding="utf-8")
    nginx = (ROOT / "nginx" / "https.conf.template").read_text(encoding="utf-8")
    assert "internal: true" in compose
    assert "location /internal" not in nginx
    assert "x-auth-bridge-secret" in (ROOT / "src" / "auth.rs").read_text(encoding="utf-8")


def test_large_upload_defaults_use_64_bit_safe_values() -> None:
    values = env_example()
    assert int(values["SERVER_MAX_UPLOAD_BYTES"]) == 10 * 1024**3
    assert int(values["SERVER_MAX_UPLOAD_BYTES"]) > 2**32
    assert int(values["LEGACY_MAX_UPLOAD_BYTES"]) == 100 * 1024**2
    assert int(values["TUS_CHUNK_SIZE_BYTES"]) == 16 * 1024**2
    assert int(values["TUS_SESSION_TTL_SECONDS"]) == 7 * 24 * 60 * 60
    assert int(values["UPLOAD_DISK_RESERVE_BYTES"]) == 20 * 1024**3
    migration = (ROOT / "migrations" / "0002_large_uploads.sql").read_text(encoding="utf-8")
    assert "ALTER COLUMN max_upload_bytes SET DEFAULT 104857600" in migration
    assert "WHERE max_upload_bytes = 26214400" in migration


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


def test_postgres_aggregates_decoded_as_i64_are_explicit_bigint() -> None:
    source = "\n".join(
        path.read_text(encoding="utf-8") for path in sorted((ROOT / "src").glob("*.rs"))
    )
    starts = re.findall(r"query_scalar::<_,\s*i64>\(", source)
    queries = re.findall(
        r'query_scalar::<_,\s*i64>\(\s*"((?:\\.|[^"\\])*)"\s*,?\s*\)',
        source,
        flags=re.DOTALL,
    )
    assert len(queries) == len(starts), "all i64 scalar SQL must remain literal and auditable"
    aggregates = [
        query
        for query in queries
        if re.search(r"\b(?:count|sum|avg|min|max)\s*\(", query, re.IGNORECASE)
    ]
    assert aggregates
    assert not [
        query
        for query in aggregates
        if not re.search(r"::\s*bigint\b|\bas\s+bigint\b", query, re.IGNORECASE)
    ]


def test_removed_backup_workflow_is_not_referenced() -> None:
    assert not (ROOT / "scripts" / "backup.sh").exists()
    for name in ("README.md", "DEPLOYMENT.md"):
        document = (ROOT / name).read_text(encoding="utf-8")
        assert "scripts/backup.sh" not in document
