#!/usr/bin/env python3
"""vault.py - Clipboard Vault from the terminal.

Standard library only. Python 3.8 or newer. Works on Linux, macOS, and Windows.

Run it with no arguments (on Windows: double-click it) for the full-screen interface:
browse, search and page through the vault, pick files to upload from local or network
(\\\\server\\share) folders, select items to download, and manage the trash.

The same things are available as commands for scripts:

    python vault.py                           full-screen interface (also: ui)
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
    the settings saved by the full-screen interface (Settings, or the first-run setup)
    the DEFAULT_SERVER / DEFAULT_API_KEY values below

Saved settings live in your user profile, never next to this file, so the script can sit
on a shared network folder without exposing your key:
    Windows   %APPDATA%\\ClipboardVault\\config.json
    others    ~/.config/clipboard-vault/config.json     (VAULT_CONFIG overrides)

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
import queue
import re
import shutil
import socket
import sys
import threading
import time
import traceback
import unicodedata
import urllib.parse
import uuid
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

DEFAULT_SERVER = ""
DEFAULT_API_KEY = ""

VERSION = "3.0"
TUS_VERSION = "1.0.0"
MULTIPART_MAX = 8 * 1024 * 1024           # bigger files go in resumable chunks (the server allows 100 MiB per classic request)
SEND_STEP = 256 * 1024                    # progress and cancel granularity while a chunk is on the wire
DEFAULT_CHUNK = 16 * 1024 * 1024
READ_BLOCK = 1024 * 1024
WIN = os.name == "nt"
UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$", re.I)
KINDS = ["text", "code", "url", "html", "image", "file"]


class VaultError(Exception):
    pass


# ----------------------------------------------------- paths, settings, UNC

def home_dir() -> Path:
    try:
        return Path.home()
    except (RuntimeError, KeyError):
        return Path(os.path.expanduser("~"))


def app_dir() -> Path:
    if WIN:
        base = os.environ.get("APPDATA") or os.environ.get("LOCALAPPDATA") or str(home_dir())
        return Path(base) / "ClipboardVault"
    return Path(os.environ.get("XDG_CONFIG_HOME") or home_dir() / ".config") / "clipboard-vault"


CONFIG_FILE = Path(os.environ["VAULT_CONFIG"]) if os.environ.get("VAULT_CONFIG") else app_dir() / "config.json"
# Same place as every earlier version, so uploads started by an older vault.py still resume.
RESUME_FILE = Path(os.environ.get("VAULT_STATE", home_dir() / ".vault-uploads.json"))


def fs(path) -> str:
    """A path string that Windows accepts past 260 characters, including \\\\server\\share paths."""
    text = os.path.abspath(str(path))
    if not WIN or len(text) < 240 or text.startswith("\\\\?\\"):
        return text
    return "\\\\?\\UNC\\" + text[2:] if text.startswith("\\\\") else "\\\\?\\" + text


def load_config() -> dict:
    try:
        data = json.loads(CONFIG_FILE.read_text("utf-8-sig"))
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def save_config(**updates) -> None:
    data = load_config()
    for name, value in updates.items():
        if value is None:
            data.pop(name, None)
        else:
            data[name] = value
    CONFIG_FILE.parent.mkdir(parents=True, exist_ok=True)
    temp = CONFIG_FILE.with_name(CONFIG_FILE.name + ".tmp")
    temp.write_text(json.dumps(data, indent=2), "utf-8")
    try:
        os.chmod(temp, 0o600)
    except OSError:
        pass
    os.replace(temp, CONFIG_FILE)


def downloads_dir(cfg: dict | None = None) -> Path:
    configured = (cfg if cfg is not None else load_config()).get("download_dir") or os.environ.get("VAULT_DOWNLOADS")
    if configured:
        return Path(os.path.expandvars(os.path.expanduser(str(configured))))
    candidate = home_dir() / "Downloads"
    return candidate if candidate.is_dir() else home_dir()


def safe_cwd() -> Path:
    """The current folder, unless it is useless as a destination.

    A script started from a network share has no real working directory on Windows:
    the shell falls back to C:\\Windows, which must never become a download target.
    """
    try:
        cwd = Path.cwd()
    except OSError:
        return downloads_dir()
    if WIN:
        system = os.path.normcase(os.environ.get("SystemRoot", r"C:\Windows"))
        if os.path.normcase(str(cwd)).startswith(system):
            return downloads_dir()
    return cwd


def clean_path(text: str) -> str:
    """What a person types or pastes into a path box: quotes (Explorer's "Copy as path"), ~, %VAR%."""
    text = text.strip().strip('"').strip("'").strip()
    return os.path.expandvars(os.path.expanduser(text)) if text else ""


def script_dir() -> str | None:
    """The folder this script was started from (a \\\\server\\share\\... folder when it lives on a share)."""
    try:
        return os.path.dirname(os.path.abspath(__file__))
    except NameError:                    # embedded or frozen: there is no file
        return None


def start_folder(cfg: dict) -> str:
    """Where the upload picker opens: an explicit setting, else the folder holding this script."""
    for candidate in (os.environ.get("VAULT_START_DIR"), cfg.get("start_dir"), script_dir()):
        if candidate and os.path.isdir(fs(clean_path(str(candidate)))):
            return os.path.abspath(clean_path(str(candidate)))
    return str(home_dir())


def windows_drives() -> list:
    import ctypes
    kernel, mpr = ctypes.windll.kernel32, getattr(ctypes.windll, "mpr", None)
    kinds = {2: "Removable", 3: "Local disk", 4: "Network drive", 5: "CD/DVD", 6: "RAM disk"}
    found = []
    mask = kernel.GetLogicalDrives()
    for index in range(26):
        if not mask & (1 << index):
            continue
        root = chr(65 + index) + ":\\"
        label = kinds.get(kernel.GetDriveTypeW(root), "Drive")
        if label == "Network drive" and mpr is not None:
            size = ctypes.c_uint32(520)
            buffer = ctypes.create_unicode_buffer(size.value)
            if mpr.WNetGetConnectionW(root[:2], buffer, ctypes.byref(size)) == 0 and buffer.value:
                label = buffer.value
        found.append((f"{root[:2]}  {label}", root))
    return found


def quick_places(cfg: dict) -> list:
    places = []
    if WIN:
        try:
            places.extend(windows_drives())
        except Exception:  # ctypes quirks on unusual consoles must not stop the picker
            places.append(("C:  Local disk", "C:\\"))
    else:
        places.append(("/  Root", "/"))
        for mount in ("/mnt", "/media", "/Volumes"):
            if os.path.isdir(mount):
                places.append((f"{mount}  Mounts", mount))
    home = home_dir()
    places.append(("Home", str(home)))
    for name in ("Desktop", "Documents", "Downloads"):
        if (home / name).is_dir():
            places.append((name, str(home / name)))
    for recent in cfg.get("recent_paths") or []:
        if isinstance(recent, str) and recent:
            places.append(("Recent  " + recent, recent))
    return places


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
        conn = self.local.conn = cls(self.host, self.port, timeout=300)
        try:                                   # connect now to tune the socket; failures resurface on the request
            conn.connect()
            conn.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            conn.sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4 * 1024 * 1024)
        except OSError:
            pass
        return conn

    def drop(self) -> None:
        """Forget this thread's connection; use after a request was abandoned half way."""
        current = getattr(self.local, "conn", None)
        self.local.conn = None
        if current is not None:
            current.close()

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
                # A streamed body is used up once sent, so only the caller can retry it.
                if attempt == 2 or (body is not None and not isinstance(body, (bytes, str))):
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
UI_ACTIVE = False        # while the full-screen interface owns the console, nothing else may print


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
            return f"{int(max(seconds, 0) // div)}{unit} ago"
    return iso[:10]


class Progress:
    """Console progress for the commands; feeds a Job (and honours cancel) inside the interface."""

    def __init__(self, label: str, total: int, job=None, cancel: threading.Event | None = None):
        self.label = label if len(label) <= 40 else label[:37] + "..."
        self.total = max(total, 1)
        self.start = time.monotonic()
        self.base = 0
        self.last = 0.0
        self.job = job
        self.cancel = cancel
        if job is not None:
            job.total = total

    def update(self, done: int, force: bool = False):
        if self.job is not None:
            self.job.done = done
            if self.cancel is not None and self.cancel.is_set():
                raise VaultError("cancelled")
            return
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
        if self.job is None and TTY:
            with PRINT_LOCK:
                sys.stderr.write("\r" + " " * 90 + "\r")
                sys.stderr.flush()


def say(message: str):
    if UI_ACTIVE:
        return
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


def multipart_upload(client: Client, path: Path, folder: str, tags: list, job=None, cancel=None) -> dict:
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
    size = os.stat(fs(path)).st_size
    progress = Progress(path.name, size, job, cancel)
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
            with open(fs(path), "rb") as handle:
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
                client.drop()
                raise
        except BaseException:
            client.drop()          # cancelled or failed mid-body: this connection cannot be reused
            raise
    progress.finish()
    if response.status != 201:
        raise VaultError(error_text(response.status, data))
    return json.loads(data)


class ChunkReader:
    """Reads the file ahead of the network, so a slow disk or network share overlaps with the upload."""

    def __init__(self, path, offset: int, size: int, chunk: int):
        self.path, self.size, self.chunk = path, size, chunk
        self.stop = threading.Event()
        self.queue = queue.Queue(maxsize=1)
        self.begin(offset)

    def begin(self, offset: int):
        self.stop, self.queue = threading.Event(), queue.Queue(maxsize=1)
        threading.Thread(target=self.run, args=(offset, self.stop, self.queue), daemon=True).start()

    def run(self, offset: int, stop: threading.Event, out: queue.Queue):
        def put(item) -> bool:
            while not stop.is_set():
                try:
                    out.put(item, timeout=0.2)
                    return True
                except queue.Full:
                    pass
            return False

        try:
            with open(fs(self.path), "rb") as handle:
                handle.seek(offset)
                while offset < self.size and not stop.is_set():
                    block = handle.read(min(self.chunk, self.size - offset))
                    if not block:
                        raise OSError(f"{os.path.basename(str(self.path))} ended early; did it change while uploading?")
                    if not put((offset, block)):
                        return
                    offset += len(block)
        except BaseException as error:
            put(error)

    def next(self, cancel=None):
        while True:
            try:
                item = self.queue.get(timeout=0.2)
            except queue.Empty:
                if cancel is not None and cancel.is_set():
                    raise VaultError("cancelled")
                continue
            if isinstance(item, BaseException):
                raise item
            return item

    def reset(self, offset: int):
        self.stop.set()
        self.begin(offset)

    def close(self):
        self.stop.set()


def chunk_body(block: bytes, base: int, progress: "Progress"):
    """Feeds one chunk to the socket in slices, so progress moves and Esc takes effect during the chunk."""
    view = memoryview(block)
    for start in range(0, len(view), SEND_STEP):
        piece = view[start:start + SEND_STEP]
        yield piece
        progress.update(base + start + len(piece))


def tus_upload(client: Client, path: Path, folder: str, tags: list, job=None, cancel=None) -> str:
    stat = os.stat(fs(path))
    size = stat.st_size
    resume_key = f"{client.origin}|{os.path.abspath(str(path))}|{size}|{int(stat.st_mtime)}"
    chunk = getattr(client, "tus_chunk", 0)
    if not chunk:
        chunk = DEFAULT_CHUNK
        response, _ = client.request("OPTIONS", "/api/v1/uploads")
        try:
            advertised = int(response.getheader("X-Tus-Chunk-Size") or 0)
            if advertised > 0:
                chunk = min(advertised, DEFAULT_CHUNK)
        except ValueError:
            pass
        client.tus_chunk = chunk

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

    progress = Progress(path.name, size, job, cancel)
    progress.base = offset
    progress.update(offset)
    reader = ChunkReader(path, offset, size, chunk) if offset < size else None
    try:
        while offset < size:
            at, block = reader.next(cancel)
            if at != offset:                       # the server's offset moved (resume or conflict): read from there
                reader.reset(offset)
                continue
            for attempt in range(5):
                try:
                    response, data = client.request("PATCH", upload_path, body=chunk_body(block, offset, progress), headers={
                        "Tus-Resumable": TUS_VERSION, "Upload-Offset": str(offset),
                        "Content-Type": "application/offset+octet-stream", "Content-Length": str(len(block)),
                    })
                except (OSError, http.client.HTTPException, VaultError):
                    client.drop()                  # a half-sent body leaves the connection unusable
                    if cancel is not None and cancel.is_set():
                        raise VaultError("cancelled")
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
                time.sleep(0 if attempt == 0 else min(2 ** attempt, 15))
                progress.update(offset)
            else:
                raise VaultError(f"{path.name}: connection kept failing at {fmt_size(offset)}. Run the same command again to resume.")
            progress.update(offset)
    finally:
        if reader is not None:
            reader.close()
    progress.finish()

    delay = 0.05
    for _ in range(600):
        status = client.json("GET", upload_path.rstrip("/") + "/status")
        state = str(status.get("state", "")).lower()
        if state == "completed" or status.get("itemId"):
            save_resume(resume_key, None)
            return status.get("itemId") or ""
        if state in ("failed", "canceled", "cancelled", "expired"):
            save_resume(resume_key, None)
            raise VaultError(f"{path.name}: upload {state} ({status.get('error') or status.get('errorCode') or 'no detail'})")
        if cancel is not None and cancel.is_set():
            raise VaultError("cancelled")
        time.sleep(delay)
        delay = min(delay * 2, 0.5)
    raise VaultError(f"{path.name}: the server is still verifying the file; check `ls` shortly.")


def upload_one(client: Client, path: Path, folder: str, tags: list, resumable: bool = False, job=None, cancel=None) -> str:
    size = os.stat(fs(path)).st_size
    if size == 0:
        raise VaultError(f"{path.name}: empty files are not stored")
    if size <= MULTIPART_MAX and not resumable:
        return multipart_upload(client, path, folder, tags, job, cancel).get("id", "")
    return tus_upload(client, path, folder, tags, job, cancel)


def vault_folder(base: str, relative: str) -> str:
    parts = [p for p in (base.replace("\\", "/").strip("/"), relative.replace("\\", "/").strip("/")) if p]
    return "/" + "/".join(parts)


def is_hidden(name: str, stat_result=None) -> bool:
    if name.startswith("."):
        return True
    return bool(stat_result is not None and getattr(stat_result, "st_file_attributes", 0) & 0x6)   # hidden | system


def expand_selection(selection: list, keep_structure: bool = False) -> list:
    """[(path, is_dir)] -> [(Path, folder inside the vault, size)], files only, no duplicates."""
    files, seen = [], set()

    def add(path: Path, relative: str):
        marker = os.path.normcase(os.path.abspath(str(path)))
        if marker in seen:
            return
        try:
            size = os.stat(fs(path)).st_size
        except OSError:
            return
        seen.add(marker)
        files.append((path, relative, size))

    for raw, is_dir in selection:
        path = Path(raw)
        if not is_dir:
            add(path, "")
            continue
        root = fs(path)
        top = os.path.basename(os.path.abspath(str(path)).rstrip("\\/")) or "root"
        for current, dirnames, names in os.walk(root):
            dirnames[:] = sorted(d for d in dirnames if not is_hidden(d))
            relative = os.path.relpath(current, root)
            relative = "" if relative == "." else relative
            for name in sorted(names):
                if is_hidden(name):
                    continue
                folder = (top + "/" + relative.replace("\\", "/")).strip("/") if keep_structure else ""
                add(Path(os.path.join(current, name)), folder)
    return files


def cmd_up(client: Client, args):
    selection = []
    for raw in args.paths:
        path = Path(os.path.expanduser(raw))
        if path.is_dir():
            selection.append((str(path), True))
        elif path.is_file():
            selection.append((str(path), False))
        else:
            raise VaultError(f"No such file or folder: {raw}")
    files = expand_selection(selection, args.keep_structure)
    if not files:
        raise VaultError("Nothing to upload.")
    tags = [t.strip() for t in ",".join(args.tag).split(",") if t.strip()]
    total = sum(size for _, _, size in files)
    say(f"Uploading {len(files)} file{'s' if len(files) != 1 else ''} ({fmt_size(total)}) to {client.origin}")

    def one(entry):
        path, relative, _ = entry
        return path, upload_one(client, path, vault_folder(args.folder, relative), tags, args.resumable)

    failures = 0
    started = time.monotonic()
    with ThreadPoolExecutor(max_workers=max(1, min(args.jobs, len(files)))) as pool:
        futures = [pool.submit(one, entry) for entry in files]
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

def fetch_items(client: Client, query: str = "", kind: str = "", trash: bool = False, limit: int = 200, before=None):
    params = {"limit": max(1, min(limit, 200))}
    if query:
        params["q"] = query
    if kind:
        params["type"] = kind
    if trash:
        params["trash"] = "true"
    if before:
        params["before"] = before
    data = client.json("GET", "/api/v1/items?" + urllib.parse.urlencode(params))
    return data.get("items", []), data.get("next_cursor")


def list_items(client: Client, query: str = "", kind: str = "", trash: bool = False, limit: int = 0) -> list:
    items, before = [], None
    while True:
        batch, before = fetch_items(client, query, kind, trash, 200, before)
        items.extend(batch)
        if limit and len(items) >= limit:
            return items[:limit]
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


RESERVED_NAMES = {"CON", "PRN", "AUX", "NUL", *(f"COM{i}" for i in range(1, 10)), *(f"LPT{i}" for i in range(1, 10))}


def safe_name(name: str) -> str:
    safe = "".join("_" if c in '\\/:*?"<>|' or ord(c) < 32 else c for c in name).strip(" .") or "download"
    if safe.split(".")[0].upper() in RESERVED_NAMES:
        safe = "_" + safe
    return safe


def unique_path(directory: Path, name: str) -> Path:
    target = directory / safe_name(name)
    stem, suffix, n = target.stem[:150], target.suffix[:20], 1
    target = directory / (stem + suffix)
    while os.path.exists(fs(target)):
        target = directory / f"{stem} ({n}){suffix}"
        n += 1
    return target


def download(client: Client, item: dict, directory: Path, job=None, cancel=None) -> Path:
    os.makedirs(fs(directory), exist_ok=True)
    if item.get("text_payload") is not None:
        target = unique_path(directory, item_name(item)[:60] + ".txt")
        with open(fs(target), "wb") as out:
            out.write(item["text_payload"].encode("utf-8"))
        if job is not None:
            job.total = job.done = len(item["text_payload"].encode("utf-8"))
        return target
    target = unique_path(directory, item_name(item))
    part = target.with_name(target.name + ".part")
    offset = os.stat(fs(part)).st_size if os.path.exists(fs(part)) else 0
    headers = {"Range": f"bytes={offset}-"} if offset else {}
    response = client.request("GET", f"/api/v1/items/{item['id']}/content?download=1", headers=headers, stream=True)
    if response.status == 416:
        response.read()
        os.replace(fs(part), fs(target))
        return target
    if response.status not in (200, 206):
        raise VaultError(error_text(response.status, response.read()))
    if response.status == 200:
        offset = 0
    total = int(item.get("size_bytes") or 0)
    progress = Progress(target.name, total, job, cancel)
    progress.base = offset
    try:
        with open(fs(part), "ab" if offset else "wb") as out:
            done = offset
            while True:
                block = response.read(READ_BLOCK)
                if not block:
                    break
                out.write(block)
                done += len(block)
                progress.update(done)
    except BaseException:
        client.drop()              # the response body was abandoned; do not reuse its connection
        raise
    progress.finish()
    os.replace(fs(part), fs(target))
    return target


def cmd_get(client: Client, args):
    directory = Path(os.path.expanduser(args.output)) if args.output else safe_cwd()
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

def split_tags(*parts) -> list:
    return [t.strip() for t in ",".join(parts).split(",") if t.strip()]


def cmd_paste(client: Client, args):
    text = " ".join(args.text) if args.text else sys.stdin.read()
    if not text.strip():
        raise VaultError("Nothing to save: the text is empty.")
    payload = {"payload": text, "type": args.type, "virtual_path": args.folder, "tags": split_tags(*args.tag)}
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


# ====================================================== full-screen interface

ESC = "\x1b"
USE_COLOR = not os.environ.get("NO_COLOR")
STYLES = {"bold": "1", "dim": "2", "rev": "7", "red": "31", "green": "32", "yellow": "33", "cyan": "36", "title": "1;7"}
WIN_KEYS = {"H": "up", "P": "down", "K": "left", "M": "right", "I": "pgup", "Q": "pgdn", "G": "home", "O": "end", "S": "del"}
POSIX_KEYS = {"A": "up", "B": "down", "C": "right", "D": "left", "H": "home", "F": "end"}
POSIX_TILDE = {"1": "home", "3": "del", "4": "end", "5": "pgup", "6": "pgdn", "7": "home", "8": "end"}


def sty(text: str, style: str) -> str:
    if not USE_COLOR or not style:
        return text
    return "".join(f"{ESC}[{STYLES[s]}m" for s in style.split()) + text + f"{ESC}[0m"


def cell(char: str) -> int:
    if unicodedata.combining(char):
        return 0
    return 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1


def text_width(text: str) -> int:
    return sum(cell(c) for c in text)


def clip(text: str, width: int) -> str:
    """Cut text to `width` terminal columns, ending in an ellipsis when something was removed."""
    text = "".join(" " if c in "\t\r\n" or ord(c) < 32 else c for c in text)
    if width <= 0:
        return ""
    if text_width(text) <= width:
        return text
    out, used = [], 0
    for char in text:
        step = cell(char)
        if used + step > width - 1:
            break
        out.append(char)
        used += step
    return "".join(out) + "…"


def fit(text: str, width: int) -> str:
    text = clip(text, width)
    return text + " " * (width - text_width(text))


def tail_clip(text: str, width: int) -> str:
    """Keep the END of a long path, which is the part that tells folders apart."""
    if text_width(text) <= width:
        return text
    out, used = [], 1
    for char in reversed(text):
        step = cell(char)
        if used + step > width:
            break
        out.append(char)
        used += step
    return "…" + "".join(reversed(out))


def wrap_text(text: str, width: int) -> list:
    lines = []
    for source in text.expandtabs(4).splitlines() or [""]:
        current, used = [], 0
        for char in source:
            if ord(char) < 32:
                char = " "
            step = cell(char)
            if used + step > width and current:
                lines.append("".join(current))
                current, used = [], 0
            current.append(char)
            used += step
        lines.append("".join(current))
    return lines


def hints_line(pairs: list, width: int) -> str:
    out, used = [], 0
    for key, label in pairs:
        piece = f"{key} {label}"
        need = len(piece) + (2 if out else 0)
        if used + need > width:
            break
        out.append(sty(key, "bold") + " " + sty(label, "dim"))
        used += need
    return "  ".join(out)


def fmt_mtime(value: float) -> str:
    try:
        return time.strftime("%Y-%m-%d %H:%M", time.localtime(value))
    except (OverflowError, OSError, ValueError):
        return ""


class Terminal:
    """Raw keyboard input and full-screen output on the Windows console or a POSIX terminal."""

    def __init__(self):
        self.fd = None
        self.saved = None
        self.win_mode = None
        self.last = None

    def __enter__(self):
        if not (sys.stdin.isatty() and sys.stdout.isatty()):
            raise VaultError("The full-screen interface needs an interactive terminal. Use the commands instead: python vault.py --help")
        try:
            sys.stdout.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass
        if WIN:
            import ctypes
            kernel = ctypes.windll.kernel32
            handle = kernel.GetStdHandle(-11)
            mode = ctypes.c_uint32()
            if not kernel.GetConsoleMode(handle, ctypes.byref(mode)) or not kernel.SetConsoleMode(handle, mode.value | 0x0004):
                raise VaultError("This console cannot draw the full-screen interface. Use Windows Terminal or Windows 10 or newer, or use the commands (python vault.py --help).")
            self.win_mode = (handle, mode.value)
            kernel.SetConsoleOutputCP(65001)
        else:
            import termios
            import tty
            self.fd = sys.stdin.fileno()
            self.saved = termios.tcgetattr(self.fd)
            tty.setcbreak(self.fd)
            attrs = termios.tcgetattr(self.fd)
            attrs[0] &= ~termios.IXON          # let Ctrl+S reach us instead of freezing the output
            termios.tcsetattr(self.fd, termios.TCSADRAIN, attrs)
        self.write(f"{ESC}]0;Clipboard Vault\x07{ESC}[?1049h{ESC}[?25l")
        return self

    def __exit__(self, *_):
        self.write(f"{ESC}[0m{ESC}[?25h{ESC}[?1049l")
        if self.win_mode:
            import ctypes
            ctypes.windll.kernel32.SetConsoleMode(self.win_mode[0], self.win_mode[1])
        if self.saved is not None:
            import termios
            termios.tcsetattr(self.fd, termios.TCSADRAIN, self.saved)

    def write(self, text: str):
        sys.stdout.write(text)
        sys.stdout.flush()

    def size(self) -> tuple:
        size = shutil.get_terminal_size((100, 30))
        # One column short of the width: a character in the last column makes Windows consoles wrap.
        return max(size.columns - 1, 40), max(size.lines, 12)

    def draw(self, lines: list):
        frame = (self.size(), tuple(lines))
        if frame == self.last:
            return
        self.last = frame
        out = [f"{ESC}[{i + 1};1H{line}{ESC}[0m{ESC}[K" for i, line in enumerate(lines)]
        out.append(f"{ESC}[{len(lines) + 1};1H{ESC}[J")
        self.write("".join(out))

    def key(self, timeout: float | None = None):
        """One key: a character, or up/down/left/right/home/end/pgup/pgdn/del/enter/esc/bs/tab. None on timeout."""
        return self._key_windows(timeout) if WIN else self._key_posix(timeout)

    def _key_windows(self, timeout):
        import msvcrt
        if timeout is not None:
            end = time.monotonic() + timeout
            while not msvcrt.kbhit():
                if time.monotonic() >= end:
                    return None
                time.sleep(0.02)
        char = msvcrt.getwch()
        if char in ("\x00", "\xe0"):
            return WIN_KEYS.get(msvcrt.getwch(), "")
        return {"\r": "enter", "\n": "enter", "\x1b": "esc", "\x08": "bs", "\x7f": "bs", "\t": "tab"}.get(char) or self._ctrl(char)

    def _key_posix(self, timeout):
        import select
        if timeout is not None and not select.select([self.fd], [], [], timeout)[0]:
            return None
        data = os.read(self.fd, 1)
        if not data:
            raise EOFError("the terminal was closed")
        first = data[0]
        if first == 0x1B:
            if not select.select([self.fd], [], [], 0.03)[0]:
                return "esc"
            if os.read(self.fd, 1) not in (b"[", b"O"):
                return "esc"
            sequence = b""
            while select.select([self.fd], [], [], 0.03)[0]:
                byte = os.read(self.fd, 1)
                sequence += byte
                if byte.isalpha() or byte == b"~":
                    break
            text = sequence.decode("ascii", "ignore")
            if text.endswith("~"):
                return POSIX_TILDE.get(text[:-1].split(";")[0], "")
            return POSIX_KEYS.get(text[-1:], "")
        if first >= 0x80:
            extra = 1 if first < 0xE0 else 2 if first < 0xF0 else 3
            return (data + os.read(self.fd, extra)).decode("utf-8", "replace")
        char = chr(first)
        return {"\r": "enter", "\n": "enter", "\x7f": "bs", "\x08": "bs", "\t": "tab"}.get(char) or self._ctrl(char)

    @staticmethod
    def _ctrl(char: str) -> str:
        if char == "\x03":
            raise KeyboardInterrupt
        return char


class Pos:
    """Cursor and scroll position of a list."""

    def __init__(self):
        self.cur = 0
        self.top = 0

    def fix(self, count: int, height: int):
        self.cur = max(0, min(self.cur, count - 1))
        if self.cur < self.top:
            self.top = self.cur
        if self.cur >= self.top + height:
            self.top = self.cur - height + 1
        self.top = max(0, min(self.top, max(count - height, 0)))

    def handle(self, key: str, count: int, height: int, pages: bool = False) -> bool:
        if key == "up":
            self.cur -= 1
        elif key == "down":
            self.cur += 1
        elif key == "home":
            self.cur = 0
        elif key == "end":
            self.cur = count - 1
        elif pages and key == "pgup":
            self.cur -= height
        elif pages and key == "pgdn":
            self.cur += height
        else:
            return False
        self.fix(count, height)
        return True


class Job:
    """One transfer shown on the progress screen."""

    def __init__(self, name: str, total: int = 0, **extra):
        self.name = name
        self.total = total
        self.done = 0
        self.state = "queued"       # queued active done failed cancelled
        self.error = ""
        self.result = None
        self.__dict__.update(extra)


class UI:
    def __init__(self, term, server: str, key: str):
        self.term = term
        self.server = server
        self.key = key
        self.cfg = load_config()
        self.client = None
        self.status = None
        self.workers = max(1, min(int(self.cfg.get("jobs") or 4), 16))

    # ---- drawing helpers

    def flash(self, message: str, kind: str = "ok"):
        self.status = (message, kind)

    def render(self, title: str, sub: str, body: list, hints: list):
        cols, rows = self.term.size()
        height = rows - 4
        host = urllib.parse.urlsplit(self.server).netloc or "not connected"
        head = fit(f" Clipboard Vault  ·  {title}", cols - text_width(host) - 2) + " " + host + " "
        lines = [sty(fit(head, cols), "title"), fit(sub, cols)]
        lines += [clip(line, cols) if "\x1b" not in line else line for line in body[:height]]
        lines += [""] * (height - len(body[:height]))
        message, kind = self.status or ("", "ok")
        lines.append(sty(fit(" " + message, cols), {"ok": "green", "err": "red", "warn": "yellow"}[kind]) if message else "")
        lines.append(hints_line(hints, cols - 1))
        self.term.draw(lines)

    def body_height(self) -> int:
        return self.term.size()[1] - 4

    def splash(self, text: str, title: str = "Working"):
        self.render(title, "", ["", "  " + text], [])

    def read_key(self, timeout=None):
        key = self.term.key(timeout)
        if key is not None:
            self.status = None
        return key

    def message(self, title: str, lines: list, kind: str = "ok"):
        cols = self.term.size()[0]
        body = [""]
        for line in lines:
            body.extend("  " + part for part in wrap_text(line, cols - 4))
        while True:
            self.render(title, "", body, [("Enter", "continue")])
            if self.read_key() in ("enter", "esc", "q", " "):
                return

    def confirm(self, question: str, default: bool = False) -> bool:
        suffix = "[Y/n]" if default else "[y/N]"
        last = self.term.last
        while True:
            if last is not None:
                lines = list(last[1])
                cols = last[0][0]
                lines[-2] = sty(fit(f" {question} {suffix}", cols), "yellow")
                self.term.draw(lines)
            key = self.term.key()
            if key in ("y", "Y"):
                answer = True
            elif key in ("n", "N", "esc", "q"):
                answer = False
            elif key == "enter":
                answer = default
            else:
                continue
            self.status = None
            self.term.last = None
            return answer

    def prompt(self, title: str, label: str, initial: str = "", secret: bool = False, hint: str = ""):
        buf, pos = list(initial), len(initial)
        while True:
            cols = self.term.size()[0]
            width = cols - 6
            start = 0
            while start < pos and text_width("".join(buf[start:pos])) > width - 1:
                start += 1
            shown = ["*" if secret else c for c in buf]
            before = "".join(shown[start:pos])
            current = shown[pos] if pos < len(shown) else " "
            after = "".join(shown[pos + 1:])
            room = max(width - text_width(before) - cell(current), 0)
            line = "  " + before + sty(current, "rev") + clip(after, room)
            body = ["", "  " + label, "", line]
            if hint:
                body += ["", sty("  " + hint, "dim")]
            self.render(title, "", body, [("Enter", "OK"), ("Esc", "cancel"), ("Ctrl+U", "clear")])
            keys = [self.read_key()]
            while True:                       # drain a paste in one go: one redraw, not one per character
                more = self.term.key(0)
                if more is None:
                    break
                keys.append(more)
            for key in keys:
                if key == "enter":
                    return "".join(buf).strip()
                if key == "esc":
                    return None
                if key == "bs" and pos:
                    del buf[pos - 1]
                    pos -= 1
                elif key == "del" and pos < len(buf):
                    del buf[pos]
                elif key == "left":
                    pos = max(0, pos - 1)
                elif key == "right":
                    pos = min(len(buf), pos + 1)
                elif key == "home":
                    pos = 0
                elif key == "end":
                    pos = len(buf)
                elif key == "\x15":
                    buf, pos = [], 0
                elif len(key) == 1 and ord(key) >= 32:
                    buf.insert(pos, key)
                    pos += 1

    # ---- start up

    def connect(self) -> bool:
        try:
            self.client = Client(self.server.rstrip("/"), self.key.strip())
            fetch_items(self.client, limit=1)
            return True
        except (VaultError, OSError, http.client.HTTPException) as error:
            self.flash(str(error), "err")
            return False

    def run(self):
        if not self.server or not self.key:
            if not self.setup(first=True):
                return
        elif not self.connect():
            self.message("Cannot connect", [str(self.status[0]) if self.status else "Could not reach the server.",
                                            "Open Settings to fix the address or the key."], "err")
        self.menu()

    def setup(self, first: bool = False) -> bool:
        intro = ["Enter the address of your vault and an API key (Keys and team > Create key).",
                 f"They are saved in your user profile: {CONFIG_FILE}"]
        while True:
            if first:
                self.message("Welcome", intro)
            server = self.prompt("Setup", "Vault address, for example https://vault.example.com", self.server)
            if server is None:
                return False
            key = self.prompt("Setup", "API key (typing is hidden)", "", secret=True)
            if key is None:
                return False
            self.server, self.key = server.strip().rstrip("/"), key.strip()
            self.splash("Checking the connection…", "Setup")
            if self.connect():
                self.persist(server=self.server, key=self.key)
                self.flash("Connected. Settings saved.")
                return True
            self.message("Could not connect", [str(self.status[0]) if self.status else "Unknown error", "Check the address and the key, then try again."], "err")
            first = False

    def persist(self, **updates):
        try:
            save_config(**updates)
            self.cfg = load_config()
        except OSError as error:
            self.flash(f"Could not save settings to {CONFIG_FILE}: {error}", "err")

    # ---- menu

    def menu(self):
        entries = [
            ("b", "Browse the vault", "search, page through, view and download items"),
            ("u", "Upload files", "pick files or folders, local or on a network share"),
            ("n", "New text note", "type or paste text into the vault"),
            ("t", "Trash", "restore items or delete them for good"),
            ("s", "Storage", "space used by this workspace"),
            ("c", "Settings", "server, key, download folder"),
            ("q", "Quit", ""),
        ]
        pos = Pos()
        while True:
            body = [""]
            for index, (letter, name, note) in enumerate(entries):
                row = f"  {letter}   {name:<22} {note}"
                body.append(sty(fit(row, self.term.size()[0]), "rev") if index == pos.cur else (
                    "  " + sty(letter, "cyan") + "   " + name.ljust(22) + " " + sty(note, "dim")))
            self.render("Menu", " What would you like to do?", body, [("↑↓", "move"), ("Enter", "open"), ("q", "quit")])
            key = self.read_key()
            if key == "enter":
                key = entries[pos.cur][0]
            elif pos.handle(key, len(entries), len(entries)):
                continue
            if key in ("q", "esc"):
                return
            action = {"b": self.browse, "u": self.upload_flow, "n": self.note, "t": lambda: self.browse(trash=True),
                      "s": self.storage, "c": self.settings}.get(key)
            if action:
                try:
                    action()
                except (VaultError, OSError, http.client.HTTPException) as error:
                    self.flash(str(error), "err")

    # ---- browse, search, page

    def browse(self, trash: bool = False):
        cols, rows = self.term.size()
        state = {"q": "", "kind": "", "trash": trash, "size": max(10, min(rows - 6, 200)),
                 "page": 0, "cursors": [None], "items": [], "next": None, "seen": []}
        selected = {}
        pos = Pos()
        if not self.load_page(state, 0):
            return
        while True:
            cols, rows = self.term.size()
            height = rows - 4
            items = state["items"]
            pos.fix(len(items), height)
            where = "Trash" if state["trash"] else "Vault"
            filters = []
            if state["q"]:
                filters.append(f'search "{state["q"]}"')
            if state["kind"]:
                filters.append("type " + state["kind"])
            sub = f" {where} · page {state['page'] + 1}" + (" · more →" if state["next"] else "") \
                + (" · " + " · ".join(filters) if filters else "") + (f" · {len(selected)} selected" if selected else "")
            body = []
            if not items:
                body = ["", "  " + ("Nothing matches." if filters else "Nothing here yet." if not state["trash"] else "The trash is empty.")]
            name_room = max(cols - 41, 10)
            for index in range(pos.top, min(len(items), pos.top + height)):
                item = items[index]
                mark = "[x]" if item["id"] in selected else "[ ]"
                tags = " ".join("#" + t for t in (item.get("tags") or []) if isinstance(t, str))
                folder = item.get("virtual_path") or "/"
                label = (folder.rstrip("/") + "/" if folder != "/" else "") + item_name(item) + (("   " + tags) if tags else "")
                row = (f"{mark} {item['id'][:8]} {item.get('kind', ''):<5}{'*' if item.get('pinned') else ' '}"
                       f"{fmt_size(item.get('size_bytes')):>9} {fmt_age(item.get('created_at', '')):>8}  ")
                if index == pos.cur:
                    body.append(sty(fit(row + label, cols), "rev"))
                else:
                    body.append((sty(mark, "green") if mark == "[x]" else mark) + row[3:] + clip(label, name_room))
            hints = [("↑↓", "move"), ("←→", "page"), ("Space", "select"), ("/", "search"), ("Enter", "open"), ("d", "download")]
            hints += [("r", "restore"), ("p", "delete forever")] if state["trash"] else [("x", "trash"), ("t", "type")]
            hints += [("Tab", "vault/trash"), ("?", "help"), ("q", "back")]
            self.render("Trash" if state["trash"] else "Browse", sub, body, hints)
            key = self.read_key()
            current = items[pos.cur] if items else None
            targets = list(selected.values()) or ([current] if current else [])
            try:
                if pos.handle(key, len(items), height):
                    continue
                if key in ("q", "esc"):
                    return
                if key in ("right", "pgdn", "n") and state["next"]:
                    if self.load_page(state, state["page"] + 1):
                        pos = Pos()
                elif key in ("left", "pgup", "b") and state["page"] > 0:
                    if self.load_page(state, state["page"] - 1):
                        pos = Pos()
                elif key == " " and current:
                    if current["id"] in selected:
                        del selected[current["id"]]
                    else:
                        selected[current["id"]] = current
                    pos.cur += 1
                elif key == "a" and items:
                    if all(i["id"] in selected for i in items):
                        for i in items:
                            selected.pop(i["id"], None)
                    else:
                        selected.update({i["id"]: i for i in items})
                elif key == "c":
                    selected.clear()
                elif key == "/":
                    text = self.prompt("Search", "Search words (empty clears the search)", state["q"])
                    if text is not None:
                        state["q"] = text
                        pos = Pos()
                        self.load_page(state, 0, reset=True)
                elif key == "t" and not state["trash"]:
                    order = [""] + KINDS
                    state["kind"] = order[(order.index(state["kind"]) + 1) % len(order)]
                    pos = Pos()
                    self.load_page(state, 0, reset=True)
                elif key == "T":
                    state["kind"] = ""
                    pos = Pos()
                    self.load_page(state, 0, reset=True)
                elif key == "tab":
                    state["trash"] = not state["trash"]
                    selected.clear()
                    pos = Pos()
                    self.load_page(state, 0, reset=True)
                elif key == "R":
                    self.load_page(state, state["page"])
                elif key == "enter" and current:
                    if self.view_item(current, state["trash"]) == "changed":
                        selected.pop(current["id"], None)
                        self.load_page(state, state["page"])
                elif key == "d" and targets:
                    self.download_items(targets)
                elif key == "x" and targets and not state["trash"]:
                    if self.confirm(f"Move {len(targets)} item(s) to the trash?"):
                        self.bulk("trash", targets)
                        selected.clear()
                        self.load_page(state, state["page"])
                elif key == "r" and targets and state["trash"]:
                    self.bulk("restore", targets)
                    selected.clear()
                    self.load_page(state, state["page"])
                elif key == "p" and targets and state["trash"]:
                    if self.confirm(f"Delete {len(targets)} item(s) FOR GOOD? This cannot be undone."):
                        self.bulk("purge", targets)
                        selected.clear()
                        self.load_page(state, state["page"])
                elif key == "E" and state["trash"]:
                    answer = self.prompt("Empty trash", "Type yes to delete everything in the trash for good", "")
                    if answer and answer.lower() == "yes":
                        result = self.client.json("POST", "/api/v1/trash/empty")
                        self.flash(f"Deleted {result.get('purged', 0)} item(s), freed {fmt_size(result.get('reclaimedBytes', 0))}.")
                        self.load_page(state, 0, reset=True)
                elif key == "?":
                    self.help_browse()
            except (VaultError, OSError, http.client.HTTPException) as error:
                self.flash(str(error), "err")

    def load_page(self, state: dict, page: int, reset: bool = False) -> bool:
        if reset:
            state.update(page=0, cursors=[None], seen=[])
            page = 0
        self.splash("Loading…", "Vault")
        try:
            already = set().union(*state["seen"][:page]) if page else set()
            items, nxt = fetch_items(self.client, state["q"], state["kind"], state["trash"], state["size"], state["cursors"][page])
        except (VaultError, OSError, http.client.HTTPException) as error:
            self.flash(str(error), "err")
            return False
        # Pinned items sort first on every page, so an older pinned item can repeat; show it once.
        items = [i for i in items if i["id"] not in already]
        state["items"], state["next"], state["page"] = items, nxt, page
        del state["seen"][page:]
        state["seen"].append({i["id"] for i in items})
        del state["cursors"][page + 1:]
        if nxt:
            state["cursors"].append(nxt)
        return True

    def bulk(self, action: str, items: list):
        result = self.client.json("POST", "/api/v1/items/bulk", {"action": action, "ids": [i["id"] for i in items]})
        done = {"trash": "Moved to trash", "restore": "Restored", "purge": "Deleted for good"}[action]
        self.flash(f"{done}: {result.get('affected', len(items))} item(s).")

    def help_browse(self):
        self.message("Help", [
            "Up/Down move, Home/End jump. Left/Right (or PgUp/PgDn) change page.",
            "Space selects the item and moves down; a selects the whole page; c clears the selection. The selection is kept while you change pages.",
            "/ searches. t cycles the type filter (text, code, url, html, image, file); T clears it.",
            "Enter opens an item (text is shown in full). d downloads the selected items, or the item under the cursor.",
            "x moves to the trash. Tab switches between the vault and the trash; there r restores, p deletes for good, E empties it.",
            "R reloads. q or Esc goes back.",
        ])

    def view_item(self, item: dict, in_trash: bool):
        scroll = 0
        text = item.get("text_payload")
        while True:
            cols, rows = self.term.size()
            height = rows - 4
            tags = ", ".join(t for t in (item.get("tags") or []) if isinstance(t, str)) or "none"
            head = [
                sty(item_name(item), "bold"),
                f"id {item['id']}   kind {item.get('kind', '')}   {fmt_size(item.get('size_bytes'))}",
                f"folder {item.get('virtual_path') or '/'}   tags {tags}",
                f"saved {item.get('created_at', '')}",
                "-" * min(cols - 2, 60),
            ]
            if text is not None:
                content = wrap_text(text, cols - 4)
            else:
                content = ["", "This is a file. Press d to download it."]
            room = max(height - len(head) - 1, 1)
            scroll = max(0, min(scroll, max(len(content) - room, 0)))
            body = [""] + ["  " + line for line in head] + ["  " + line for line in content[scroll:scroll + room]]
            more = f" · line {scroll + 1}-{min(scroll + room, len(content))} of {len(content)}" if text is not None else ""
            hints = [("↑↓", "scroll"), ("d", "download")] + ([("r", "restore")] if in_trash else [("x", "trash")]) + [("q", "back")]
            self.render("Item", " " + item["id"][:8] + more, body[:height], hints)
            key = self.read_key()
            try:
                if key in ("q", "esc", "enter"):
                    return None
                if key == "up":
                    scroll -= 1
                elif key == "down":
                    scroll += 1
                elif key == "pgup":
                    scroll -= room
                elif key == "pgdn":
                    scroll += room
                elif key == "home":
                    scroll = 0
                elif key == "end":
                    scroll = len(content)
                elif key == "d":
                    self.download_items([item])
                elif key == "x" and not in_trash and self.confirm("Move this item to the trash?"):
                    self.bulk("trash", [item])
                    return "changed"
                elif key == "r" and in_trash:
                    self.bulk("restore", [item])
                    return "changed"
            except (VaultError, OSError, http.client.HTTPException) as error:
                self.flash(str(error), "err")

    # ---- transfers

    def run_jobs(self, title: str, jobs: list, work) -> list:
        cancel = threading.Event()
        started = time.monotonic()

        def runner(job):
            if cancel.is_set():
                job.state = "cancelled"
                return
            job.state = "active"
            try:
                job.result = work(job, cancel)
                job.state = "done"
                job.done = max(job.done, job.total)
            except VaultError as error:
                job.state, job.error = ("cancelled", "cancelled") if cancel.is_set() else ("failed", str(error))
            except (OSError, http.client.HTTPException) as error:
                job.state, job.error = "failed", str(error)
            except Exception as error:      # a bug must show up on the job, not vanish in a thread
                job.state, job.error = "failed", f"{type(error).__name__}: {error}"

        pool = ThreadPoolExecutor(max_workers=max(1, min(self.workers, len(jobs))))
        for job in jobs:
            pool.submit(runner, job)
        scroll = 0
        final = ("done", "failed", "cancelled")
        finished = False
        while True:
            finished = all(j.state in final for j in jobs)
            scroll = self.draw_jobs(title, jobs, started, scroll, finished, cancel.is_set())
            key = self.term.key(None if finished else 0.15)
            if finished:
                if key in ("q", "esc", "enter"):
                    break
                if key == "up":
                    scroll -= 1
                elif key == "down":
                    scroll += 1
                elif key == "pgup":
                    scroll -= 10
                elif key == "pgdn":
                    scroll += 10
            elif key in ("esc", "q"):
                cancel.set()
        pool.shutdown(wait=True)
        return jobs

    def draw_jobs(self, title, jobs, started, scroll, finished, cancelling) -> int:
        cols, rows = self.term.size()
        height = rows - 4
        done = sum(1 for j in jobs if j.state == "done")
        failed = sum(1 for j in jobs if j.state == "failed")
        moved = sum(j.done for j in jobs)
        speed = moved / max(time.monotonic() - started, 0.01)
        sub = f" {done}/{len(jobs)} done" + (f" · {failed} failed" if failed else "") + f" · {fmt_size(moved)} · {fmt_size(speed)}/s"
        if cancelling and not finished:
            sub += " · cancelling…"
        icons = {"queued": ("·", "dim"), "active": (">", "cyan"), "done": ("✓", "green"), "failed": ("✗", "red"), "cancelled": ("-", "yellow")}
        if finished:
            start = max(0, min(scroll, max(len(jobs) - height, 0)))
        else:
            first = next((i for i, j in enumerate(jobs) if j.state in ("queued", "active")), 0)
            start = max(0, min(first - 1, max(len(jobs) - height, 0)))
        body = []
        bar = 22
        for job in jobs[start:start + height]:
            icon, color = icons[job.state]
            if job.state == "failed":
                tail = job.error
            elif job.state == "cancelled":
                tail = "cancelled"
            elif job.state == "queued":
                tail = "waiting"
            else:
                fraction = min(job.done / job.total, 1.0) if job.total else (1.0 if job.state == "done" else 0.0)
                filled = int(fraction * bar)
                tail = f"{'█' * filled}{'░' * (bar - filled)} {int(fraction * 100):>3}%  {fmt_size(job.done)}"
            room = cols - 6
            name_room = min(max(room // 3, 16), 40)
            body.append(" " + sty(icon, color) + " " + fit(job.name, name_room) + "  " + clip(tail, room - name_room - 2))
        if finished:
            hints = [("↑↓", "scroll"), ("Enter", "close")]
            self.status = (f"Finished: {done} ok, {failed} failed.", "err" if failed else "ok")
        else:
            hints = [("Esc", "cancel (partial transfers can be resumed)")]
        self.render(title, sub, body, hints)
        return start

    def download_items(self, items: list):
        default = str(downloads_dir(self.cfg))
        answer = self.prompt("Download", f"Save {len(items)} item(s) into this folder (a network path such as \\\\server\\share\\folder works)", default)
        if not answer:
            return
        directory = Path(clean_path(answer))
        try:
            os.makedirs(fs(directory), exist_ok=True)
        except OSError as error:
            self.flash(f"Cannot use that folder: {error}", "err")
            return
        jobs = [Job(item_name(i), int(i.get("size_bytes") or 0), item=i) for i in items]
        self.run_jobs("Downloading", jobs, lambda job, cancel: download(self.client, job.item, directory, job, cancel))
        self.persist(download_dir=str(directory))

    # ---- upload

    def upload_flow(self):
        selection = self.pick_files()
        if not selection:
            return
        base = self.prompt("Upload", "Vault folder to put them in (/ is the top level)", "/")
        if base is None:
            return
        tags = self.prompt("Upload", "Tags, comma separated (optional)", "")
        if tags is None:
            return
        keep = False
        if any(is_dir for _, is_dir in selection):
            self.render("Upload", "", ["", "  Folders are selected."], [])
            keep = self.confirm("Recreate their sub-folders inside the vault folder?")
        self.splash("Looking through the selected folders…", "Upload")
        files = expand_selection(selection, keep)
        if not files:
            self.flash("There are no files to upload in that selection.", "err")
            return
        total = sum(size for _, _, size in files)
        self.render("Upload", "", ["", f"  {len(files)} file(s), {fmt_size(total)}", f"  to {vault_folder(base, '')} on {self.server}"], [])
        if not self.confirm("Start the upload?", default=True):
            return
        tag_list = split_tags(tags)

        def work(job, cancel):
            return upload_one(self.client, job.path, vault_folder(base, job.relative), tag_list, False, job, cancel)

        jobs = [Job(path.name, size, path=path, relative=relative) for path, relative, size in files]
        self.run_jobs("Uploading", jobs, work)

    def scan_dir(self, path: str, hidden: bool) -> list:
        entries = []
        with os.scandir(fs(path)) as scan:
            for entry in scan:
                try:
                    is_dir = entry.is_dir()
                    info = entry.stat()
                except OSError:
                    continue
                if not hidden and is_hidden(entry.name, info):
                    continue
                entries.append((entry.name, os.path.join(path, entry.name), is_dir, 0 if is_dir else info.st_size, info.st_mtime))
        entries.sort(key=lambda e: (not e[2], e[0].casefold()))
        return entries

    def pick_files(self):
        """File browser. Returns [(path, is_dir)] or None. Folders are expanded at upload time."""
        here = start_folder(self.cfg)
        selected = {}            # path -> is_dir, kept while moving between folders
        pos = Pos()
        needle, hidden = "", False
        entries = []
        loaded = None

        while True:
            places = here is None
            if not places and loaded != (here, hidden):
                self.splash(f"Reading {here} …", "Upload")
                try:
                    entries = self.scan_dir(here, hidden)
                    loaded = (here, hidden)
                except OSError as error:
                    self.flash(f"Cannot open that folder: {error}", "err")
                    here = None if loaded is None else loaded[0]
                    continue
            if places:
                rows_ = [(label, path, True, 0, 0.0) for label, path in quick_places(self.cfg)]
                shown = rows_
            else:
                shown = [e for e in entries if needle.casefold() in e[0].casefold()]
                shown = [("..", str(Path(here).parent), True, 0, 0.0)] + shown
            cols, rows = self.term.size()
            height = rows - 4
            pos.fix(len(shown), height)
            pages = max(1, -(-len(shown) // height))
            page = pos.top // height + 1 if height else 1
            sub = " " + ("Places: pick where to start" if places else tail_clip(here, cols - 40)) \
                + (f" · filter '{needle}'" if needle else "") + (" · hidden shown" if hidden else "") \
                + f" · page {page}/{pages} · {len(selected)} selected"
            body = []
            for index in range(pos.top, min(len(shown), pos.top + height)):
                name, path, is_dir, size, mtime = shown[index]
                if places:
                    row = f"    {name}"
                    body.append(sty(fit(row, cols), "rev") if index == pos.cur else "    " + sty(name, "cyan"))
                    continue
                mark = "   " if name == ".." else ("[+]" if selected.get(path) and is_dir else "[x]" if path in selected else "[ ]")
                label = name + (os.sep if is_dir and name != ".." else "")
                tail = "" if name == ".." else f"{'' if is_dir else fmt_size(size):>10}  {fmt_mtime(mtime)}"
                room = max(cols - len(tail) - 6, 10)
                row = f"{mark} {fit(label, room)} {tail}"
                if index == pos.cur:
                    body.append(sty(fit(row, cols), "rev"))
                elif mark != "[ ]" and mark != "   ":
                    body.append(sty(mark, "green") + row[3:])
                elif is_dir:
                    body.append(mark + " " + sty(fit(label, room), "cyan") + " " + tail)
                else:
                    body.append(row)
            if not shown or (len(shown) == 1 and not places):
                body.append("")
                body.append("  " + ("No files match the filter." if needle else "This folder is empty."))
            hints = [("↑↓", "move"), ("Enter", "open/select"), ("Space", "select"), ("u", "upload"), ("g", "go to path"),
                     ("p", "places"), ("/", "filter"), ("a", "all files"), ("h", "hidden"), ("c", "clear"), ("q", "back")]
            self.render("Upload: choose files", sub, body, hints)
            key = self.read_key()
            current = shown[pos.cur] if shown else None
            if pos.handle(key, len(shown), height, pages=True):
                continue
            if key in ("q", "esc"):
                if selected and not self.confirm(f"Discard the {len(selected)} selected item(s)?"):
                    continue
                return None
            if key in ("u", "U"):
                if not selected:
                    self.flash("Select something first: move to it and press Space.", "warn")
                    continue
                if not places:
                    self.remember(here)
                return list(selected.items())
            if key == "p":
                here, needle, loaded, pos = None, "", None, Pos()
            elif key == "g":
                answer = self.prompt("Go to", "Type or paste a folder or file path, e.g. \\\\server\\share\\reports or D:\\data", "")
                target = clean_path(answer or "")
                if not target:
                    continue
                if os.path.isfile(fs(target)):
                    here, needle, loaded, pos = os.path.dirname(os.path.abspath(target)), "", None, Pos()
                    selected[os.path.abspath(target)] = False
                    self.flash("File selected.")
                elif os.path.isdir(fs(target)):
                    here, needle, loaded, pos = os.path.abspath(target), "", None, Pos()
                    self.remember(here)
                else:
                    self.flash(f"Not found or not reachable: {target}", "err")
            elif key == "/" and not places:
                text = self.prompt("Filter", "Show only names containing (empty shows all)", needle)
                if text is not None:
                    needle, pos = text, Pos()
            elif key == "h" and not places:
                hidden, pos = not hidden, Pos()
            elif key == "c":
                selected.clear()
            elif key == "a" and not places:
                files = [e[1] for e in shown if not e[2]]
                if files and all(f in selected for f in files):
                    for f in files:
                        selected.pop(f, None)
                else:
                    selected.update({f: False for f in files})
            elif key in ("bs", "left") and not places:
                parent = Path(here).parent
                here, needle, loaded, pos = (None if parent == Path(here) else str(parent)), "", None, Pos()
            elif current and key in ("enter", "right", " "):
                name, path, is_dir, _, _ = current
                if places:
                    if key == " ":
                        continue
                    if os.path.isdir(fs(path)):
                        here, needle, loaded, pos = path, "", None, Pos()
                    else:
                        self.flash(f"Cannot reach {path}", "err")
                elif name == "..":
                    if key != " ":
                        parent = Path(here).parent
                        here, needle, loaded, pos = (None if parent == Path(here) else str(parent)), "", None, Pos()
                elif key == " " or (key == "enter" and not is_dir):
                    if path in selected:
                        del selected[path]
                    else:
                        selected[path] = is_dir
                    if key == " ":
                        pos.cur += 1
                elif is_dir:
                    here, needle, loaded, pos = path, "", None, Pos()

    def remember(self, folder: str):
        recent = [p for p in (self.cfg.get("recent_paths") or []) if p != folder]
        self.persist(recent_paths=([folder] + recent)[:8])

    # ---- notes, storage, settings

    def note(self):
        text = ""
        while True:
            cols, rows = self.term.size()
            height = rows - 4
            lines = wrap_text(text, cols - 6) if text else [""]
            visible = lines[-(height - 1):]
            body = [""] + ["  " + line for line in visible[:-1]] + ["  " + visible[-1] + sty(" ", "rev")]
            self.render("New text note", f" {len(text)} characters", body, [("Ctrl+D", "save"), ("Esc", "cancel"), ("Enter", "new line")])
            keys = [self.read_key()]
            while True:
                more = self.term.key(0)
                if more is None:
                    break
                keys.append(more)
            for key in keys:
                if key in ("\x04", "\x13"):
                    if not text.strip():
                        self.flash("Nothing to save: the note is empty.", "warn")
                        continue
                    self.save_note(text)
                    return
                if key == "esc":
                    if not text.strip() or self.confirm("Discard this note?"):
                        return
                elif key == "bs":
                    text = text[:-1]
                elif key == "enter":
                    text += "\n"
                elif key == "tab":
                    text += "    "
                elif len(key) == 1 and (ord(key) >= 32):
                    text += key

    def save_note(self, text: str):
        folder = self.prompt("Save note", "Vault folder (/ is the top level)", "/")
        if folder is None:
            return
        tags = self.prompt("Save note", "Tags, comma separated (optional)", "")
        if tags is None:
            return
        item = self.client.json("POST", "/api/v1/items", {"payload": text, "type": "auto", "virtual_path": folder or "/", "tags": split_tags(tags)})
        self.flash(f"Saved {item['id'][:8]}: {item_name(item)}")

    def storage(self):
        self.splash("Asking the server…", "Storage")
        info = self.client.json("GET", "/api/v1/storage")
        lines = [
            f"Workspace          {info.get('workspace_name', '')}",
            f"Used by this workspace   {fmt_size(info.get('workspace_bytes'))}",
            f"Reserved by uploads      {fmt_size(info.get('reserved_upload_bytes'))}",
            f"Reclaimable (trash etc.) {fmt_size(info.get('reclaimable_bytes'))}",
            f"Disk free                {fmt_size(info.get('free_bytes'))} of {fmt_size(info.get('total_bytes'))}",
        ]
        if info.get("low_storage"):
            lines.append("The server is low on disk space.")
        self.message("Storage", lines, "warn" if info.get("low_storage") else "ok")

    def settings(self):
        pos = Pos()
        while True:
            masked = ("•" * 8 + self.key[-4:]) if len(self.key) > 8 else ("•" * len(self.key))
            rows = [
                ("Server address", self.server or "(not set)"),
                ("API key", masked or "(not set)"),
                ("Download folder", str(downloads_dir(self.cfg))),
                ("Upload start folder", str(self.cfg.get("start_dir") or "") or f"(this script's folder: {script_dir() or 'unknown'})"),
                ("Parallel transfers", str(self.workers)),
                ("Test the connection", ""),
                ("Forget saved settings", f"removes {CONFIG_FILE}"),
            ]
            body = [""]
            for index, (name, value) in enumerate(rows):
                row = f"  {name:<24} {value}"
                body.append(sty(fit(row, self.term.size()[0]), "rev") if index == pos.cur else "  " + name.ljust(24) + " " + sty(value, "dim"))
            body += ["", sty(f"  Saved in your user profile: {CONFIG_FILE}", "dim"),
                     sty("  The key is never stored next to this script.", "dim")]
            self.render("Settings", " Enter changes the highlighted setting", body, [("↑↓", "move"), ("Enter", "change"), ("q", "back")])
            key = self.read_key()
            if pos.handle(key, len(rows), len(rows)):
                continue
            if key in ("q", "esc"):
                return
            if key != "enter":
                continue
            if pos.cur in (0, 1):
                if pos.cur == 0:
                    value = self.prompt("Settings", "Vault address", self.server)
                else:
                    value = self.prompt("Settings", "New API key (typing is hidden)", "", secret=True)
                if value:
                    previous = (self.server, self.key)
                    if pos.cur == 0:
                        self.server = value.rstrip("/")
                    else:
                        self.key = value
                    if self.connect():
                        self.persist(server=self.server, key=self.key)
                        self.flash("Saved.")
                    else:
                        self.server, self.key = previous
                        self.connect()
            elif pos.cur == 2:
                value = self.prompt("Settings", "Folder downloads are saved into", str(downloads_dir(self.cfg)))
                if value:
                    self.persist(download_dir=clean_path(value))
            elif pos.cur == 3:
                value = self.prompt("Settings", "Folder the upload picker opens in (empty = the folder this script is in)", str(self.cfg.get("start_dir") or ""))
                if value is not None:
                    self.persist(start_dir=clean_path(value) or None)
            elif pos.cur == 4:
                value = self.prompt("Settings", "How many files to transfer at once (1-16)", str(self.workers))
                if value and value.isdigit():
                    self.workers = max(1, min(int(value), 16))
                    self.persist(jobs=self.workers)
            elif pos.cur == 5:
                self.splash("Checking…", "Settings")
                self.flash("Connected." if self.connect() else (self.status[0] if self.status else "Failed"),
                           "ok" if self.client and self.status is None else "err")
            elif pos.cur == 6:
                if self.confirm("Forget the saved server, key, and preferences?"):
                    try:
                        CONFIG_FILE.unlink()
                    except OSError:
                        pass
                    self.cfg = load_config()
                    self.flash("Saved settings removed. This session keeps working until you quit.")


# --------------------------------------------------------------------- main

def pause():
    """Keep a double-clicked console window open so the message can be read."""
    try:
        if sys.stdin and sys.stdin.isatty():
            input("\nPress Enter to close this window... ")
    except (EOFError, KeyboardInterrupt, OSError):
        pass


def cmd_ui(client, args):          # the real work happens in run_ui; this keeps `ui` a registered command
    raise AssertionError("handled by run_ui")


def run_ui(server: str, key: str):
    global UI_ACTIVE
    with Terminal() as term:
        UI_ACTIVE = True
        try:
            UI(term, server, key).run()
        finally:
            UI_ACTIVE = False


def build_parser():
    parser = argparse.ArgumentParser(prog="vault.py", description="Clipboard Vault from the terminal. Run without a command for the full-screen interface.",
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--server", default=None, help="e.g. https://vault.example.com")
    parser.add_argument("--key", default=None, help="workspace API key")
    parser.add_argument("--pause", action="store_true", help="wait for Enter before closing (for shortcuts that open their own window)")
    parser.add_argument("--version", action="version", version=f"vault.py {VERSION}")
    sub = parser.add_subparsers(dest="command", metavar="COMMAND")

    ui = sub.add_parser("ui", aliases=["tui"], help="full-screen interface (the default)")
    ui.set_defaults(run=cmd_ui)

    up = sub.add_parser("up", aliases=["upload"], help="upload files or folders")
    up.add_argument("paths", nargs="+")
    up.add_argument("-f", "--folder", default="/", help="vault folder, e.g. /reports/2026")
    up.add_argument("-t", "--tag", action="append", default=[], help="tag; repeat or comma-separate")
    up.add_argument("-j", "--jobs", type=int, default=4, help="parallel uploads (default 4)")
    up.add_argument("--resumable", action="store_true", help="use resumable uploads even for small files")
    up.add_argument("--keep-structure", action="store_true", help="recreate a folder's sub-folders inside the vault folder")
    up.set_defaults(run=cmd_up)

    ls = sub.add_parser("ls", aliases=["list"], help="list or search items")
    ls.add_argument("query", nargs="?")
    ls.add_argument("--type", choices=KINDS)
    ls.add_argument("--trash", action="store_true", help="list the trash instead")
    ls.add_argument("-n", "--limit", type=int, default=50, help="max items (0 = all, default 50)")
    ls.add_argument("--json", action="store_true", help="print raw JSON")
    ls.set_defaults(run=cmd_ls)

    get = sub.add_parser("get", aliases=["download"], help="download items")
    get.add_argument("ids", nargs="+")
    get.add_argument("-o", "--output", default=None, help="directory to save into (default: current folder)")
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
    return parser


def run(argv: list):
    parser = build_parser()
    args = parser.parse_args(argv)
    cfg = load_config()
    server = (args.server or os.environ.get("VAULT_SERVER") or cfg.get("server") or DEFAULT_SERVER or "").strip().rstrip("/")
    key = (args.key or os.environ.get("VAULT_API_KEY") or cfg.get("key") or DEFAULT_API_KEY or "").strip()
    command = getattr(args, "run", None)
    if command is None:
        if sys.stdin.isatty() and sys.stdout.isatty():
            return run_ui(server, key)
        parser.print_help()
        return
    if command is cmd_ui:
        return run_ui(server, key)
    if not server or not key:
        parser.error("no server or key yet. Run `python vault.py` once to set them up, or use --server/--key, "
                     "the VAULT_SERVER/VAULT_API_KEY environment variables, or DEFAULT_SERVER/DEFAULT_API_KEY in this file")
    try:
        command(Client(server, key), args)
    except KeyboardInterrupt:
        sys.exit("\nvault: interrupted. Run the same upload again to resume it.")


def main(argv=None):
    arguments = list(sys.argv[1:] if argv is None else argv)
    code = 0
    try:
        run(arguments)
    except VaultError as error:
        print(f"vault: {error}", file=sys.stderr)
        code = 1
    except KeyboardInterrupt:
        code = 130
    except SystemExit as stop:
        if isinstance(stop.code, str):
            print(stop.code, file=sys.stderr)
            code = 1
        else:
            code = stop.code or 0
    except Exception:      # last resort: show the problem instead of letting the window vanish
        traceback.print_exc()
        code = 1
    if arguments == [] and code not in (0, 130) or "--pause" in arguments:
        pause()
    elif arguments == [] and WIN and not (sys.stdin and sys.stdin.isatty() and sys.stdout.isatty()):
        pause()
    sys.exit(code)


if __name__ == "__main__":
    main()
