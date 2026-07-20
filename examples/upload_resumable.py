#!/usr/bin/env python3
"""Upload a file through Clipboard Vault's Tus 1.0 resumable endpoint.

Install the only dependency with:
    python -m pip install requests
"""

from __future__ import annotations

import argparse
import base64
import json
import mimetypes
import time
from pathlib import Path
from urllib.parse import urljoin, urlsplit

import requests


TUS_VERSION = "1.0.0"
DEFAULT_CHUNK_BYTES = 16 * 1024 * 1024


def encode_metadata(values: dict[str, str]) -> str:
    return ",".join(
        f"{key} {base64.b64encode(value.encode('utf-8')).decode('ascii')}"
        for key, value in values.items()
        if value
    )


def fail(response: requests.Response, action: str) -> None:
    try:
        body = response.json()
        message = body.get("error", {}).get("message") or body
    except ValueError:
        message = response.text or f"HTTP {response.status_code}"
    raise SystemExit(f"{action} failed: {message}")


def validated_upload_url(server: str, candidate: str) -> str:
    absolute = urljoin(server.rstrip("/") + "/", candidate)
    expected = urlsplit(server)
    supplied = urlsplit(absolute)
    expected_port = expected.port or (443 if expected.scheme == "https" else 80)
    supplied_port = supplied.port or (443 if supplied.scheme == "https" else 80)
    if (
        supplied.scheme not in {"http", "https"}
        or supplied.username is not None
        or supplied.password is not None
        or (supplied.scheme, supplied.hostname, supplied_port)
        != (expected.scheme, expected.hostname, expected_port)
    ):
        raise SystemExit("Upload URL must use the same origin as --server")
    return absolute


def server_state(session: requests.Session, upload_url: str) -> tuple[int, int]:
    response = session.head(
        upload_url,
        headers={"Tus-Resumable": TUS_VERSION},
        timeout=60,
    )
    if response.status_code not in (200, 204):
        fail(response, "Reading upload offset")
    return int(response.headers["Upload-Offset"]), int(response.headers["Upload-Length"])


def server_chunk_bytes(session: requests.Session, server: str) -> int:
    try:
        response = session.options(server.rstrip("/") + "/api/v1/uploads", timeout=60)
        advertised = int(response.headers.get("X-Tus-Chunk-Size", DEFAULT_CHUNK_BYTES))
        if response.ok and advertised > 0:
            return min(advertised, DEFAULT_CHUNK_BYTES)
    except (requests.RequestException, ValueError):
        pass
    return DEFAULT_CHUNK_BYTES


def create_upload(
    session: requests.Session,
    server: str,
    file: Path,
    virtual_path: str,
    tags: list[str],
    source_url: str | None,
) -> str:
    metadata = {
        "filename": file.name,
        "virtualPath": virtual_path,
        "tags": json.dumps(tags, separators=(",", ":")),
        "sourceUrl": source_url or "",
        "contentType": mimetypes.guess_type(file.name)[0] or "application/octet-stream",
    }
    response = session.post(
        server.rstrip("/") + "/api/v1/uploads",
        headers={
            "Tus-Resumable": TUS_VERSION,
            "Upload-Length": str(file.stat().st_size),
            "Upload-Metadata": encode_metadata(metadata),
        },
        timeout=60,
    )
    if response.status_code != 201:
        fail(response, "Creating upload")
    location = response.headers.get("Location")
    if not location:
        raise SystemExit("Creating upload failed: response did not include Location")
    return validated_upload_url(server, location)


def wait_for_item(session: requests.Session, upload_url: str, timeout: int) -> None:
    status_url = upload_url.rstrip("/") + "/status"
    deadline = time.monotonic() + timeout
    while True:
        response = session.get(status_url, timeout=60)
        if response.status_code != 200:
            fail(response, "Reading finalization status")
        status = response.json()
        state = status.get("state")
        if state == "completed":
            print(f"Item ID: {status.get('itemId')}")
            return
        if state in {"failed", "canceled", "expired"}:
            raise SystemExit(f"Finalization {state}: {status.get('error') or 'no detail'}")
        if time.monotonic() >= deadline:
            print(f"Upload is {state}; check later: {status_url}")
            return
        time.sleep(1)


def main() -> None:
    parser = argparse.ArgumentParser(description="Resumably upload a file to Clipboard Vault")
    parser.add_argument("file", type=Path, help="File to upload")
    parser.add_argument("--server", required=True, help="Example: https://vault.example.com")
    parser.add_argument("--api-key", required=True, help="Workspace API key")
    parser.add_argument("--path", default="/", help="Virtual folder")
    parser.add_argument("--tag", action="append", default=[], help="May be supplied more than once")
    parser.add_argument("--source-url", help="Optional source URL")
    parser.add_argument("--upload-url", help="Existing session URL to resume")
    parser.add_argument("--chunk-mib", type=int, choices=range(1, 17), help="Override the server-advertised chunk size")
    parser.add_argument("--finalize-timeout", type=int, default=300)
    args = parser.parse_args()

    if not args.file.is_file():
        parser.error(f"File does not exist: {args.file}")

    session = requests.Session()
    session.headers["Authorization"] = f"Bearer {args.api_key}"
    upload_url = validated_upload_url(args.server, args.upload_url) if args.upload_url else create_upload(
        session, args.server, args.file, args.path, args.tag, args.source_url
    )
    print(f"Upload URL: {upload_url}")

    expected_size = args.file.stat().st_size
    offset, upload_length = server_state(session, upload_url)
    if upload_length != expected_size:
        raise SystemExit(
            f"Upload session expects {upload_length} bytes, but the selected file has {expected_size}"
        )
    if offset > expected_size:
        raise SystemExit("Server offset is larger than the selected file")

    chunk_bytes = (
        min(args.chunk_mib * 1024 * 1024, DEFAULT_CHUNK_BYTES)
        if args.chunk_mib is not None
        else server_chunk_bytes(session, args.server)
    )
    with args.file.open("rb") as handle:
        while offset < expected_size:
            handle.seek(offset)
            chunk = handle.read(min(chunk_bytes, expected_size - offset))
            for attempt in range(5):
                try:
                    response = session.patch(
                        upload_url,
                        headers={
                            "Tus-Resumable": TUS_VERSION,
                            "Upload-Offset": str(offset),
                            "Content-Type": "application/offset+octet-stream",
                        },
                        data=chunk,
                        timeout=300,
                    )
                    if response.status_code == 204:
                        offset = int(response.headers["Upload-Offset"])
                        break
                    if response.status_code == 409:
                        offset, _ = server_state(session, upload_url)
                        break
                    fail(response, "Uploading chunk")
                except requests.RequestException:
                    if attempt == 4:
                        raise
                    time.sleep(2**attempt)
                    offset, _ = server_state(session, upload_url)
                    if offset != handle.tell() - len(chunk):
                        break
            print(f"{offset}/{expected_size} bytes ({offset / max(expected_size, 1):.1%})")

    wait_for_item(session, upload_url, args.finalize_timeout)


if __name__ == "__main__":
    main()
