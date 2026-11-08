#!/usr/bin/env python3
"""vault.py - Clipboard Vault from the terminal.

Standard library only. Python 3.8 or newer. Works on Linux, macOS, and Windows.

    python vault.py up FILE_OR_FOLDER...      upload (resumable, any size)
    python vault.py ls [SEARCH]               list items, newest first
    python vault.py get ID... [-o DIR]        download files or text items
    python vault.py cat ID                    print an item to stdout
    python vault.py paste [TEXT]              save text (reads stdin when TEXT is omitted)
    python vault.py rm ID...                  move items to trash
    python vault.py restore ID...             bring items back from trash
    python vault.py empty-trash               delete everything in trash for good

IDs may be shortened to their first characters, as shown by `ls`.

Configuration, highest priority first:
    --server / --key flags
    VAULT_SERVER / VAULT_API_KEY environment variables
    the DEFAULT_SERVER / DEFAULT_API_KEY values below

The key decides which workspace you reach and what you may do. Reading needs a key
with "Read items"; rm, restore, and empty-trash need "Delete items".
"""

from __future__ import annotations

import argparse
import base64
import http.client
import json
import mimetypes
import os
import re
import sys
import threading
import time
import urllib.parse
import uuid
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

DEFAULT_SERVER = ""
DEFAULT_API_KEY = ""

VERSION = "2.0"
TUS_VERSION = "1.0.0"
MULTIPART_MAX = 90 * 1024 * 1024          # below the server's 100 MiB classic limit
DEFAULT_CHUNK = 16 * 1024 * 1024
READ_BLOCK = 1024 * 1024
RESUME_FILE = Path(os.environ.get("VAULT_STATE", Path.home() / ".vault-uploads.json"))
UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$", re.I)


class VaultError(Exception):
    pass


# ---------------------------------------------------------------- HTTP client

class Client:
    """One keep-alive connection per thread; reconnects transparently."""

    def __init__(self, server: str, key: str):
        parsed = urllib.parse.urlsplit(server)
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            raise VaultError(f"Invalid server address: {server!r}. Example: https://vault.example.com")
        self.scheme = parsed.scheme
        self.host = parsed.hostname
        self.port = parsed.port
        self.base = parsed.path.rstrip("/")
        self.origin = f"{parsed.scheme}://{parsed.netloc}"
        self.key = key
        self.local = threading.local()

    def conn(self, fresh: bool = False) -> http.client.HTTPConnection:
        current = getattr(self.local, "conn", None)
        if current is not None and not fresh:
            return current
        if current is not None:
            current.close()
        cls = http.client.HTTPSConnection if self.scheme == "https" else http.client.HTTPConnection
        self.local.conn = cls(self.host, self.port, timeout=300)
        return self.local.conn

    def headers(self, extra: dict | None = None) -> dict:
        headers = {"Authorization": f"Bearer {self.key}", "User-Agent": f"vault.py/{VERSION}"}
        headers.update(extra or {})
        return headers

    def path(self, path: str) -> str:
        return self.base + path

    def request(self, method: str, path: str, body=None, headers: dict | None = None, stream: bool = False):
        for attempt in range(3):
            conn = self.conn(fresh=attempt > 0)
            try:
                conn.request(method, self.path(path), body=body, headers=self.headers(headers))
                response = conn.getresponse()
                if stream:
                    return response
                data = response.read()
                return response, data
            except (http.client.RemoteDisconnected, ConnectionResetError, BrokenPipeError, http.client.CannotSendRequest):
                if attempt == 2 or (body is not None and not isinstance(body, (bytes, str))):
                    raise
            except OSError as error:
                if attempt == 2:
                    raise VaultError(f"Could not reach {self.origin}: {error}") from error
                time.sleep(1 + attempt)
        raise VaultError("unreachable")

    def json(self, method: str, path: str, payload=None, ok=(200, 201, 204)):
        body = json.dumps(payload).encode() if payload is not None else None
        headers = {"Accept": "application/json"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        response, data = self.request(method, path, body=body, headers=headers)
        if response.status not in ok:
            raise VaultError(error_text(response.status, data))
        return json.loads(data) if data else None


def error_text(status: int, data: bytes) -> str:
    try:
        error = json.loads(data).get("error", {})
        message = error.get("message") if isinstance(error, dict) else str(error)
    except (ValueError, AttributeError):
        message = data.decode("utf-8", "replace").strip()[:200]
    hints = {
        401: "the API key is missing, wrong, expired, or revoked",
        403: "this key does not have permission for that action",
        413: "the file is larger than the workspace upload limit",
        507: "the server is low on disk space",
    }
    return f"{message or 'request failed'} (HTTP {status}{'; ' + hints[status] if status in hints else ''})"


# ------------------------------------------------------------------- output

TTY = sys.stderr.isatty()
PRINT_LOCK = threading.Lock()


def fmt_size(n) -> str:
    size = float(n or 0)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if size < 1024 or unit == "TiB":
            return f"{int(size)} B" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} TiB"


def fmt_age(iso: str) -> str:
    from datetime import datetime, timezone
    text = (iso or "").replace("Z", "+00:00")
    # Python 3.8 accepts exactly 6 fractional digits; the server sends up to 9.
    text = re.sub(r"\.(\d+)", lambda m: "." + (m.group(1) + "000000")[:6], text)
    try:
        created = datetime.fromisoformat(text)
        if created.tzinfo is None:
            created = created.replace(tzinfo=timezone.utc)
        seconds = (datetime.now(timezone.utc) - created).total_seconds()
    except ValueError:
        return (iso or "")[:10]
    for limit, div, unit in ((60, 1, "s"), (3600, 60, "m"), (86400, 3600, "h"), (604800, 86400, "d")):
        if seconds < limit:
            return f"{int(seconds // div)}{unit} ago"
    return iso[:10]


class Progress:
    def __init__(self, label: str, total: int):
        self.label = label if len(label) <= 40 else label[:37] + "..."
        self.total = max(total, 1)
        self.start = time.monotonic()
        self.base = 0
        self.last = 0.0

    def update(self, done: int, force: bool = False):
        now = time.monotonic()
        if not TTY or (not force and now - self.last < 0.2):
            return
        self.last = now
        speed = (done - self.base) / max(now - self.start, 0.01)
        line = f"\r  {self.label:<40} {done * 100 // self.total:>3}%  {fmt_size(done):>10}  {fmt_size(speed)}/s   "
        with PRINT_LOCK:
            sys.stderr.write(line)
            sys.stderr.flush()

    def finish(self):
        if TTY:
            with PRINT_LOCK:
                sys.stderr.write("\r" + " " * 90 + "\r")
                sys.stderr.flush()


def say(message: str):
    with PRINT_LOCK:
        print(message, flush=True)


# ------------------------------------------------------------------- upload

def load_resumes() -> dict:
    try:
        return json.loads(RESUME_FILE.read_text("utf-8"))
    except (OSError, ValueError):
        return {}


def save_resume(key: str, url: str | None):
    with PRINT_LOCK:
        data = load_resumes()
        if url:
            data[key] = url
        else:
            data.pop(key, None)
        try:
            RESUME_FILE.write_text(json.dumps(data), "utf-8")
        except OSError:
            pass


def multipart_upload(client: Client, path: Path, folder: str, tags: list) -> dict:
    boundary = "vault" + uuid.uuid4().hex
    mime = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
    name = path.name.replace('"', "_").replace("\r", "_").replace("\n", "_")
    fields = b""
    for key, value in (("virtual_path", folder), ("tags", json.dumps(tags))):
        fields += f'--{boundary}\r\nContent-Disposition: form-data; name="{key}"\r\n\r\n{value}\r\n'.encode()
    head = fields + (
        f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{name}"\r\n'
        f"Content-Type: {mime}\r\n\r\n"
    ).encode()
    tail = f"\r\n--{boundary}--\r\n".encode()
    size = path.stat().st_size
    progress = Progress(path.name, size)
    for attempt in range(2):
        conn = client.conn(fresh=attempt > 0)
        try:
            conn.putrequest("POST", client.path("/api/v1/items"))
            for key, value in client.headers({
                "Content-Type": f"multipart/form-data; boundary={boundary}",
                "Content-Length": str(len(head) + size + len(tail)),
                "Accept": "application/json",
            }).items():
                conn.putheader(key, value)
            conn.endheaders()
            conn.send(head)
            sent = 0
            with path.open("rb") as handle:
                while True:
                    block = handle.read(READ_BLOCK)
                    if not block:
                        break
                    conn.send(block)
                    sent += len(block)
                    progress.update(sent)
            conn.send(tail)
            response = conn.getresponse()
            data = response.read()
            break
        except (http.client.RemoteDisconnected, ConnectionResetError, BrokenPipeError, http.client.CannotSendRequest):
            if attempt == 1:
                raise
    progress.finish()
    if response.status != 201:
        raise VaultError(error_text(response.status, data))
    return json.loads(data)


def tus_upload(client: Client, path: Path, folder: str, tags: list) -> str:
    stat = path.stat()
    size = stat.st_size
    resume_key = f"{client.origin}|{path.resolve()}|{size}|{int(stat.st_mtime)}"
    chunk = DEFAULT_CHUNK
    response, _ = client.request("OPTIONS", "/api/v1/uploads")
    try:
        advertised = int(response.getheader("X-Tus-Chunk-Size") or 0)
        if advertised > 0:
            chunk = min(advertised, DEFAULT_CHUNK)
    except ValueError:
        pass

    upload_path = None
    offset = 0
    saved = load_resumes().get(resume_key)
    if saved:
        response, _ = client.request("HEAD", saved, headers={"Tus-Resumable": TUS_VERSION})
        if response.status in (200, 204):
            upload_path = saved
            offset = int(response.getheader("Upload-Offset") or 0)
            say(f"  resuming {path.name} at {fmt_size(offset)}")
        else:
            save_resume(resume_key, None)

    if upload_path is None:
        metadata = {
            "filename": path.name,
            "virtualPath": folder,
            "tags": ",".join(tags),
            "contentType": mimetypes.guess_type(path.name)[0] or "",
        }
        encoded = ",".join(f"{k} {base64.b64encode(v.encode()).decode()}" for k, v in metadata.items() if v)
        response, data = client.request("POST", "/api/v1/uploads", body=b"", headers={
            "Tus-Resumable": TUS_VERSION, "Upload-Length": str(size), "Upload-Metadata": encoded, "Content-Length": "0",
        })
        if response.status not in (200, 201):
            raise VaultError(error_text(response.status, data))
        location = response.getheader("Location") or ""
        parsed = urllib.parse.urlsplit(urllib.parse.urljoin(client.origin + "/", location))
        if f"{parsed.scheme}://{parsed.netloc}" != client.origin:
            raise VaultError("The server returned an upload address on another host; refusing to send data there.")
        upload_path = parsed.path[len(client.base):] if client.base and parsed.path.startswith(client.base) else parsed.path
        save_resume(resume_key, upload_path)

    progress = Progress(path.name, size)
    progress.base = offset
    with path.open("rb") as handle:
        while offset < size:
            handle.seek(offset)
            block = handle.read(min(chunk, size - offset))
            for attempt in range(5):
                try:
                    response, data = client.request("PATCH", upload_path, body=block, headers={
                        "Tus-Resumable": TUS_VERSION, "Upload-Offset": str(offset),
                        "Content-Type": "application/offset+octet-stream",
                    })
                except (OSError, http.client.HTTPException, VaultError):
                    response = None
                if response is not None and response.status in (200, 204):
                    offset = int(response.getheader("Upload-Offset"))
                    break
                if response is not None and response.status == 409:
                    head, _ = client.request("HEAD", upload_path, headers={"Tus-Resumable": TUS_VERSION})
                    offset = int(head.getheader("Upload-Offset") or offset)
                    break
                if response is not None and response.status < 500 and response.status != 408:
                    save_resume(resume_key, None)
                    raise VaultError(error_text(response.status, data))
                time.sleep(min(2 ** attempt, 15))
            else:
                raise VaultError(f"{path.name}: connection kept failing at {fmt_size(offset)}. Run the same command again to resume.")
            progress.update(offset)
    progress.finish()

    for _ in range(600):
        status = client.json("GET", upload_path.rstrip("/") + "/status")
        state = str(status.get("state", "")).lower()
        if state == "completed" or status.get("itemId"):
            save_resume(resume_key, None)
            return status.get("itemId") or ""
        if state in ("failed", "canceled", "cancelled", "expired"):
            save_resume(resume_key, None)
            raise VaultError(f"{path.name}: upload {state} ({status.get('error') or status.get('errorCode') or 'no detail'})")
        time.sleep(0.5)
    raise VaultError(f"{path.name}: the server is still verifying the file; check `ls` shortly.")


def expand(paths: list) -> list:
    files = []
    for raw in paths:
        path = Path(raw).expanduser()
        if path.is_dir():
            files.extend(sorted(p for p in path.rglob("*") if p.is_file() and not p.name.startswith(".")))
        elif path.is_file():
            files.append(path)
        else:
            raise VaultError(f"No such file or folder: {raw}")
    return files


def cmd_up(client: Client, args):
    files = expand(args.paths)
    if not files:
        raise VaultError("Nothing to upload.")
    tags = [t.strip() for t in ",".join(args.tag).split(",") if t.strip()]
    total = sum(f.stat().st_size for f in files)
    say(f"Uploading {len(files)} file{'s' if len(files) != 1 else ''} ({fmt_size(total)}) to {client.origin}")

    def one(path: Path):
        size = path.stat().st_size
        if size == 0:
            raise VaultError(f"{path.name}: empty files are not stored")
        if size <= MULTIPART_MAX and not args.resumable:
            return path, multipart_upload(client, path, args.folder, tags).get("id", "")
        return path, tus_upload(client, path, args.folder, tags)

    failures = 0
    started = time.monotonic()
    with ThreadPoolExecutor(max_workers=max(1, min(args.jobs, len(files)))) as pool:
        futures = [pool.submit(one, f) for f in files]
        for future in as_completed(futures):
            try:
                path, item_id = future.result()
                say(f"  ok  {str(item_id)[:8]:<8}  {path.name}")
            except (VaultError, OSError, http.client.HTTPException) as error:
                failures += 1
                say(f"  !!  {error}")
    elapsed = max(time.monotonic() - started, 0.01)
    say(f"Done: {len(files) - failures} uploaded, {failures} failed, {fmt_size(total / elapsed)}/s")
    if failures:
        sys.exit(1)


# --------------------------------------------------------------- list & read

def list_items(client: Client, query: str = "", kind: str = "", trash: bool = False, limit: int = 0) -> list:
    items, before = [], None
    while True:
        params = {"limit": 200}
        if query:
            params["q"] = query
        if kind:
            params["type"] = kind
        if trash:
            params["trash"] = "true"
        if before:
            params["before"] = before
        data = client.json("GET", "/api/v1/items?" + urllib.parse.urlencode(params))
        batch = data.get("items", [])
        items.extend(batch)
        if limit and len(items) >= limit:
            return items[:limit]
        before = data.get("next_cursor")
        if not before or not batch:
            return items


def resolve(client: Client, ref: str, trash: bool = False) -> dict:
    if UUID_RE.match(ref):
        return client.json("GET", f"/api/v1/items/{ref}")
    ref = ref.lower()
    matches = [i for i in list_items(client, trash=trash) if i["id"].startswith(ref)]
    if not matches and not trash:
        matches = [i for i in list_items(client, trash=True) if i["id"].startswith(ref)]
    if not matches:
        raise VaultError(f"No item id starts with {ref!r}")
    if len(matches) > 1:
        raise VaultError(f"{ref!r} matches {len(matches)} items; type more characters")
    return matches[0]


def item_name(item: dict) -> str:
    if item.get("original_filename"):
        return item["original_filename"]
    first = next((line.strip() for line in (item.get("text_payload") or "").splitlines() if line.strip()), "")
    return first[:60] or f"untitled {item.get('kind', 'item')}"


def cmd_ls(client: Client, args):
    items = list_items(client, args.query or "", args.type or "", args.trash, args.limit)
    if args.json:
        print(json.dumps(items, indent=2))
        return
    if not items:
        say("No items.")
        return
    for item in items:
        pin = "*" if item.get("pinned") else " "
        say(f"{item['id'][:8]} {pin} {item.get('kind', ''):<5} {fmt_size(item.get('size_bytes')):>10}  {fmt_age(item.get('created_at', '')):>8}  {item_name(item)}")
    say(f"{len(items)} item{'s' if len(items) != 1 else ''}")


def unique_path(directory: Path, name: str) -> Path:
    safe = "".join("_" if c in '\\/:*?"<>|' or ord(c) < 32 else c for c in name).strip(" .") or "download"
    target = directory / safe
    stem, suffix, n = target.stem, target.suffix, 1
    while target.exists():
        target = directory / f"{stem} ({n}){suffix}"
        n += 1
    return target


def download(client: Client, item: dict, directory: Path) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    if item.get("text_payload") is not None:
        target = unique_path(directory, item_name(item)[:60] + ".txt")
        target.write_text(item["text_payload"], "utf-8")
        return target
    target = unique_path(directory, item_name(item))
    part = target.with_name(target.name + ".part")
    offset = part.stat().st_size if part.exists() else 0
    headers = {"Range": f"bytes={offset}-"} if offset else {}
    response = client.request("GET", f"/api/v1/items/{item['id']}/content?download=1", headers=headers, stream=True)
    if response.status == 416:
        response.read()
        part.rename(target)
        return target
    if response.status not in (200, 206):
        raise VaultError(error_text(response.status, response.read()))
    if response.status == 200:
        offset = 0
    total = int(item.get("size_bytes") or 0)
    progress = Progress(target.name, total)
    progress.base = offset
    with part.open("ab" if offset else "wb") as out:
        done = offset
        while True:
            block = response.read(READ_BLOCK)
            if not block:
                break
            out.write(block)
            done += len(block)
            progress.update(done)
    progress.finish()
    part.rename(target)
    return target


def cmd_get(client: Client, args):
    directory = Path(args.output).expanduser()
    failures = 0
    for ref in args.ids:
        try:
            item = resolve(client, ref)
            path = download(client, item, directory)
            say(f"  ok  {item['id'][:8]}  {path}")
        except (VaultError, OSError, http.client.HTTPException) as error:
            failures += 1
            say(f"  !!  {ref}: {error}")
    if failures:
        sys.exit(1)


def cmd_cat(client: Client, args):
    item = resolve(client, args.id)
    if item.get("text_payload") is not None:
        sys.stdout.write(item["text_payload"])
        if not item["text_payload"].endswith("\n"):
            sys.stdout.write("\n")
        return
    response = client.request("GET", f"/api/v1/items/{item['id']}/content?download=1", stream=True)
    if response.status != 200:
        raise VaultError(error_text(response.status, response.read()))
    out = sys.stdout.buffer
    while True:
        block = response.read(READ_BLOCK)
        if not block:
            break
        out.write(block)
    out.flush()


# ------------------------------------------------------------------- write

def cmd_paste(client: Client, args):
    text = " ".join(args.text) if args.text else sys.stdin.read()
    if not text.strip():
        raise VaultError("Nothing to save: the text is empty.")
    payload = {"payload": text, "type": args.type, "virtual_path": args.folder,
               "tags": [t.strip() for t in ",".join(args.tag).split(",") if t.strip()]}
    item = client.json("POST", "/api/v1/items", payload)
    say(f"saved {item['id'][:8]}  {item_name(item)}")


def cmd_rm(client: Client, args):
    ids = [resolve(client, ref)["id"] for ref in args.ids]
    result = client.json("POST", "/api/v1/items/bulk", {"action": "trash", "ids": ids})
    say(f"Moved {result.get('affected', len(ids))} item(s) to trash.")


def cmd_restore(client: Client, args):
    ids = [resolve(client, ref, trash=True)["id"] for ref in args.ids]
    result = client.json("POST", "/api/v1/items/bulk", {"action": "restore", "ids": ids})
    say(f"Restored {result.get('affected', len(ids))} item(s).")


def cmd_empty_trash(client: Client, args):
    if not args.yes:
        if not sys.stdin.isatty():
            raise VaultError("Refusing to empty the trash without --yes.")
        if input("Delete everything in trash forever? Type yes: ").strip().lower() != "yes":
            say("Cancelled.")
            return
    result = client.json("POST", "/api/v1/trash/empty")
    say(f"Deleted {result.get('purged', 0)} item(s), freed {fmt_size(result.get('reclaimedBytes', 0))}.")


# --------------------------------------------------------------------- main

def main(argv=None):
    parser = argparse.ArgumentParser(prog="vault.py", description="Clipboard Vault from the terminal.",
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--server", default=os.environ.get("VAULT_SERVER") or DEFAULT_SERVER, help="e.g. https://vault.example.com")
    parser.add_argument("--key", default=os.environ.get("VAULT_API_KEY") or DEFAULT_API_KEY, help="workspace API key")
    parser.add_argument("--version", action="version", version=f"vault.py {VERSION}")
    sub = parser.add_subparsers(dest="command", metavar="COMMAND")

    up = sub.add_parser("up", aliases=["upload"], help="upload files or folders")
    up.add_argument("paths", nargs="+")
    up.add_argument("-f", "--folder", default="/", help="vault folder, e.g. /reports/2026")
    up.add_argument("-t", "--tag", action="append", default=[], help="tag; repeat or comma-separate")
    up.add_argument("-j", "--jobs", type=int, default=4, help="parallel uploads (default 4)")
    up.add_argument("--resumable", action="store_true", help="use resumable uploads even for small files")
    up.set_defaults(run=cmd_up)

    ls = sub.add_parser("ls", aliases=["list"], help="list or search items")
    ls.add_argument("query", nargs="?")
    ls.add_argument("--type", choices=["text", "code", "url", "html", "image", "file"])
    ls.add_argument("--trash", action="store_true", help="list the trash instead")
    ls.add_argument("-n", "--limit", type=int, default=50, help="max items (0 = all, default 50)")
    ls.add_argument("--json", action="store_true", help="print raw JSON")
    ls.set_defaults(run=cmd_ls)

    get = sub.add_parser("get", aliases=["download"], help="download items")
    get.add_argument("ids", nargs="+")
    get.add_argument("-o", "--output", default=".", help="directory to save into (default: current)")
    get.set_defaults(run=cmd_get)

    cat = sub.add_parser("cat", help="print an item to stdout")
    cat.add_argument("id")
    cat.set_defaults(run=cmd_cat)

    paste = sub.add_parser("paste", help="save text from arguments or stdin")
    paste.add_argument("text", nargs="*")
    paste.add_argument("--type", default="auto", choices=["auto", "text", "code", "html", "url"])
    paste.add_argument("-f", "--folder", default="/")
    paste.add_argument("-t", "--tag", action="append", default=[])
    paste.set_defaults(run=cmd_paste)

    rm = sub.add_parser("rm", help="move items to trash")
    rm.add_argument("ids", nargs="+")
    rm.set_defaults(run=cmd_rm)

    restore = sub.add_parser("restore", help="restore items from trash")
    restore.add_argument("ids", nargs="+")
    restore.set_defaults(run=cmd_restore)

    empty = sub.add_parser("empty-trash", help="permanently delete everything in trash")
    empty.add_argument("-y", "--yes", action="store_true", help="do not ask for confirmation")
    empty.set_defaults(run=cmd_empty_trash)

    args = parser.parse_args(argv)
    if not getattr(args, "run", None):
        parser.print_help()
        return
    if not args.server or not args.key:
        parser.error("set the server and key: --server/--key, VAULT_SERVER/VAULT_API_KEY, or DEFAULT_SERVER/DEFAULT_API_KEY in this file")
    try:
        args.run(Client(args.server.rstrip("/"), args.key.strip()), args)
    except VaultError as error:
        sys.exit(f"vault: {error}")
    except KeyboardInterrupt:
        sys.exit("\nvault: interrupted. Run the same upload again to resume it.")


if __name__ == "__main__":
    main()
