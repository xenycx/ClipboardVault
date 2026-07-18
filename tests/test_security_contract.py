from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


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
