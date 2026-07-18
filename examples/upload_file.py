#!/usr/bin/env python3
"""Upload one file to Clipboard Vault.

Install the only dependency with:
    python -m pip install requests
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import requests


def main() -> None:
    parser = argparse.ArgumentParser(description="Upload a file to Clipboard Vault")
    parser.add_argument("file", type=Path, help="File to upload")
    parser.add_argument("--server", required=True, help="Example: https://vault.example.com")
    parser.add_argument("--api-key", required=True, help="Workspace API key shown by Clipboard Vault")
    parser.add_argument("--path", default="/", help="Virtual folder, for example /reports/2026")
    parser.add_argument("--tag", action="append", default=[], help="Tag; may be supplied more than once")
    parser.add_argument("--source-url", help="Optional source URL")
    args = parser.parse_args()

    if not args.file.is_file():
        parser.error(f"File does not exist: {args.file}")

    data = {
        "virtual_path": args.path,
        "tags": json.dumps(args.tag),
    }
    if args.source_url:
        data["source_url"] = args.source_url

    with args.file.open("rb") as handle:
        response = requests.post(
            args.server.rstrip("/") + "/api/v1/items",
            headers={"Authorization": f"Bearer {args.api_key}"},
            data=data,
            files={"file": (args.file.name, handle)},
            timeout=300,
        )

    if response.status_code != 201:
        try:
            message = response.json()["error"]["message"]
        except (ValueError, KeyError, TypeError):
            message = response.text or f"HTTP {response.status_code}"
        raise SystemExit(f"Upload failed: {message}")

    item = response.json()
    print(f"Uploaded {item.get('original_filename', args.file.name)}")
    print(f"Item ID: {item['id']}")
    print(f"Virtual folder: {item['virtual_path']}")


if __name__ == "__main__":
    main()

