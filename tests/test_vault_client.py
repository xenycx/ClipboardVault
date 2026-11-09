"""Behavior tests for static/vault.py: the CLI core and the full-screen interface.

A small fake vault server speaks the handful of /api/v1 routes the client uses, and a scripted
terminal replaces the console, so every screen is driven by key presses and read back as text.
"""

from __future__ import annotations

import importlib.util
import json
import os
import re
import subprocess
import sys
import threading
import uuid
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

import pytest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "static" / "vault.py"
KEY = "cv_live_" + "k" * 64
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07")


# ------------------------------------------------------------------ fake server

class FakeVault:
    def __init__(self):
        now = datetime.now(timezone.utc)
        self.items = []
        for number in range(1, 26):
            self.items.append(self.make(
                f"report-{number:02}.txt", kind="file", size=1000 * number,
                created=now - timedelta(hours=number), content=f"report body {number}".encode() * 10,
            ))
        self.items.append(self.make("", kind="text", text="Remember the milk\nand the eggs", created=now - timedelta(minutes=5)))
        self.items.sort(key=lambda i: i["created_at"], reverse=True)
        self.uploads = []
        self.contents = {}
        self.calls = []

    def make(self, name, kind, size=0, created=None, content=b"", text=None):
        item_id = uuid.uuid4().hex[:8] + "-0000-4000-8000-" + uuid.uuid4().hex[:12]
        item = {
            "id": item_id, "kind": kind, "original_filename": name or None, "text_payload": text,
            "size_bytes": len(text or "") if text is not None else size, "pinned": False, "tags": ["t"],
            "virtual_path": "/", "created_at": created.strftime("%Y-%m-%dT%H:%M:%S.%fZ"),
            "deleted_at": None,
        }
        self.__dict__.setdefault("blobs", {})[item_id] = content
        return item


def make_handler(vault: FakeVault):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def reply(self, status, payload=None, raw=None, headers=None):
            body = raw if raw is not None else json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Content-Type", "application/json" if raw is None else "application/octet-stream")
            for key, value in (headers or {}).items():
                self.send_header(key, value)
            self.end_headers()
            self.wfile.write(body)

        def authorized(self):
            if self.headers.get("Authorization") != f"Bearer {KEY}":
                self.reply(401, {"error": {"message": "bad key"}})
                return False
            return True

        def body(self):
            return self.rfile.read(int(self.headers.get("Content-Length") or 0))

        def do_GET(self):
            if not self.authorized():
                return
            url = urlsplit(self.path)
            query = {k: v[0] for k, v in parse_qs(url.query).items()}
            vault.calls.append(("GET", self.path))
            if url.path == "/api/v1/items":
                trash = query.get("trash") == "true"
                pool = [i for i in vault.items if bool(i["deleted_at"]) == trash]
                if query.get("q"):
                    word = query["q"].lower()
                    pool = [i for i in pool if word in (i["original_filename"] or i["text_payload"] or "").lower()]
                if query.get("type"):
                    pool = [i for i in pool if i["kind"] == query["type"]]
                if query.get("before"):
                    pool = [i for i in pool if i["created_at"] < query["before"]]
                limit = int(query.get("limit", 50))
                page = pool[:limit]
                more = len(pool) > limit
                return self.reply(200, {"items": page, "next_cursor": page[-1]["created_at"] if more else None})
            if url.path == "/api/v1/storage":
                return self.reply(200, {"workspace_name": "Main", "workspace_bytes": 2048, "reserved_upload_bytes": 0,
                                        "reclaimable_bytes": 0, "free_bytes": 10 ** 9, "total_bytes": 2 * 10 ** 9, "low_storage": False})
            match = re.fullmatch(r"/api/v1/items/([^/]+)/content", url.path)
            if match and match.group(1) in vault.blobs:
                return self.reply(200, raw=vault.blobs[match.group(1)])
            self.reply(404, {"error": {"message": "not found"}})

        def do_POST(self):
            if not self.authorized():
                return
            data = self.body()
            vault.calls.append(("POST", self.path))
            if self.path == "/api/v1/items/bulk":
                request = json.loads(data)
                hit = [i for i in vault.items if i["id"] in request["ids"]]
                for item in hit:
                    item["deleted_at"] = "now" if request["action"] == "trash" else None
                return self.reply(200, {"affected": len(hit)})
            if self.path == "/api/v1/items":
                kind = self.headers.get("Content-Type", "")
                if kind.startswith("multipart/form-data"):
                    text = data.decode("utf-8", "replace")
                    name = re.search(r'filename="([^"]*)"', text).group(1)
                    folder = re.search(r'name="virtual_path"\r\n\r\n([^\r]*)', text).group(1)
                    tags = json.loads(re.search(r'name="tags"\r\n\r\n([^\r]*)', text).group(1))
                    payload = text.split("\r\n\r\n", 3)[-1].rsplit("\r\n--", 1)[0]
                    vault.uploads.append({"name": name, "folder": folder, "tags": tags, "payload": payload})
                    return self.reply(201, {"id": uuid.uuid4().hex + "-x", "original_filename": name})
                request = json.loads(data)
                vault.uploads.append({"text": request["payload"], "folder": request["virtual_path"], "tags": request["tags"]})
                return self.reply(201, {"id": uuid.uuid4().hex, "text_payload": request["payload"], "kind": "text"})
            self.reply(404, {"error": {"message": "not found"}})

    return Handler


@pytest.fixture()
def vault_server():
    vault = FakeVault()
    server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(vault))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    vault.url = f"http://127.0.0.1:{server.server_address[1]}"
    yield vault
    server.shutdown()
    server.server_close()


@pytest.fixture()
def vault_module(tmp_path, monkeypatch):
    monkeypatch.setenv("VAULT_CONFIG", str(tmp_path / "config" / "config.json"))
    monkeypatch.setenv("VAULT_STATE", str(tmp_path / "resume.json"))
    monkeypatch.delenv("VAULT_SERVER", raising=False)
    monkeypatch.delenv("VAULT_API_KEY", raising=False)
    spec = importlib.util.spec_from_file_location("vault_under_test", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    sys.modules["vault_under_test"] = module
    spec.loader.exec_module(module)
    yield module
    sys.modules.pop("vault_under_test", None)


# ------------------------------------------------------------- scripted terminal

class FakeTerminal:
    def __init__(self, keys, size=(100, 30)):
        self.script = list(keys)
        self.dims = size
        self.frames = []
        self.last = None

    def size(self):
        return self.dims

    def draw(self, lines):
        frame = (self.dims, tuple(lines))
        if frame == self.last:
            return
        self.last = frame
        self.frames.append("\n".join(ANSI.sub("", line) for line in lines))

    def key(self, timeout=None):
        if timeout is not None:          # polling: never consume the script
            return None
        if not self.script:
            raise AssertionError("the script ran out of keys; the last screen was:\n" + self.frames[-1])
        return self.script.pop(0)

    def screen(self):
        return self.frames[-1]

    def seen(self, text):
        return any(text in frame for frame in self.frames)


def type_text(text):
    return list(text)


def make_ui(module, vault, keys, tmp_path, **config):
    module.save_config(download_dir=str(tmp_path / "downloads"), **config)
    term = FakeTerminal(keys)
    ui = module.UI(term, vault.url, KEY)
    module.UI_ACTIVE = True
    assert ui.connect()
    return ui, term


# ------------------------------------------------------------------------ tests

def test_text_helpers_respect_terminal_width(vault_module):
    v = vault_module
    assert v.clip("hello", 10) == "hello"
    assert v.clip("hello world", 6) == "hello…"
    assert v.text_width(v.clip("日本語のファイル名です", 9)) <= 9          # wide characters take two columns
    assert v.tail_clip(r"\\server\share\very\long\folder\name", 16).startswith("…")
    assert v.tail_clip(r"\\server\share\very\long\folder\name", 16).endswith("folder\\name")
    assert v.clean_path('  "\\\\server\\share\\My Files"  ') == "\\\\server\\share\\My Files"   # Explorer's "Copy as path"
    assert v.safe_name("CON.txt") == "_CON.txt"
    assert v.safe_name('a:b*c?.txt') == "a_b_c_.txt"
    assert v.vault_folder("/reports/", "2026\\q1") == "/reports/2026/q1"


def test_unc_paths_gain_the_long_path_prefix_only_on_windows(vault_module):
    v = vault_module
    if v.WIN:
        long_unc = "\\\\server\\share\\" + "d\\" * 150 + "file.txt"
        assert v.fs(long_unc).startswith("\\\\?\\UNC\\server\\share\\")
        assert v.fs("C:\\short.txt") == "C:\\short.txt"
    else:
        assert v.fs("/tmp/x") == "/tmp/x"


def test_settings_are_saved_in_the_profile_not_beside_the_script(vault_module, tmp_path):
    v = vault_module
    v.save_config(server="https://v.example.com", key="secret-key")
    assert v.CONFIG_FILE.parent == tmp_path / "config"
    assert json.loads(v.CONFIG_FILE.read_text())["server"] == "https://v.example.com"
    if os.name != "nt":
        assert (v.CONFIG_FILE.stat().st_mode & 0o777) == 0o600
    v.save_config(key=None)
    assert "key" not in v.load_config()


def test_browse_pages_search_and_download(vault_module, vault_server, tmp_path):
    keys = (["b", "right"]                       # open the vault, next page
            + ["left", "/"] + type_text("report-03") + ["enter"]   # back, search
            + [" ", "d", "enter", "enter"]       # select it, download to the default folder, close the screen
            + ["q", "q"])                        # leave browse, quit the menu
    ui, term = make_ui(vault_module, vault_server, keys, tmp_path)
    # a page holds ten rows on this terminal, so shrink it to force a second page
    term.dims = (100, 12)
    ui.menu()
    assert term.seen("page 2")
    assert term.seen('search "report-03"')
    assert term.seen("[x]")
    downloaded = list((tmp_path / "downloads").iterdir())
    assert [p.name for p in downloaded] == ["report-03.txt"]
    assert downloaded[0].read_bytes() == b"report body 3" * 10
    assert term.seen("Finished: 1 ok, 0 failed")


def test_pages_do_not_repeat_items_and_cover_everything(vault_module, vault_server, tmp_path):
    ui, term = make_ui(vault_module, vault_server, [], tmp_path)
    state = {"q": "", "kind": "", "trash": False, "size": 3, "page": 0, "cursors": [None], "items": [], "next": None, "seen": []}
    seen = []
    page = 0
    while True:
        assert ui.load_page(state, page)
        seen += [i["id"] for i in state["items"]]
        if not state["next"]:
            break
        page += 1
    assert len(seen) == len(set(seen)) == len(vault_server.items)
    assert ui.load_page(state, 1) and ui.load_page(state, 0)            # going back works through the cursor stack
    assert [i["id"] for i in state["items"]] == seen[:3]


def test_text_items_open_in_full_and_can_be_trashed_and_restored(vault_module, vault_server, tmp_path):
    keys = ["b", "enter", "down", "x", "y", "q", "q"]       # newest item is the note; open, scroll, trash it, leave
    ui, term = make_ui(vault_module, vault_server, keys, tmp_path)
    ui.menu()
    assert term.seen("Remember the milk")
    assert term.seen("and the eggs")
    note = next(i for i in vault_server.items if i["text_payload"])
    assert note["deleted_at"]
    ui2, term2 = make_ui(vault_module, vault_server, ["t", " ", "r", "q", "q"], tmp_path)
    ui2.menu()
    assert term2.seen("Restored: 1 item(s)")
    assert not note["deleted_at"]


def test_upload_files_from_the_picker_with_folder_and_tags(vault_module, vault_server, tmp_path):
    source = tmp_path / "work"
    (source / "sub").mkdir(parents=True)
    (source / "a.txt").write_text("alpha")
    (source / "b.txt").write_text("bravo")
    (source / "sub" / "c.txt").write_text("charlie")
    (source / ".hidden").write_text("no")
    keys = (["u", "a", "u", "enter"]                         # all files here, upload, accept "/"
            + ["x", ",", "y", "enter"] + ["y"] + ["enter"]   # tags "x,y", start, close the progress screen
            + ["q"])
    ui, term = make_ui(vault_module, vault_server, keys, tmp_path, last_dir=str(source))
    ui.menu()
    names = sorted(u["name"] for u in vault_server.uploads)
    assert names == ["a.txt", "b.txt"]                       # hidden files and sub-folders are not taken by "a"
    assert all(u["tags"] == ["x", "y"] and u["folder"] == "/" for u in vault_server.uploads)
    assert {u["payload"] for u in vault_server.uploads} == {"alpha", "bravo"}
    assert term.seen("Finished: 2 ok, 0 failed")


def test_upload_a_folder_keeping_its_structure(vault_module, vault_server, tmp_path):
    source = tmp_path / "work"
    (source / "photos" / "2026").mkdir(parents=True)
    (source / "photos" / "top.txt").write_text("1")
    (source / "photos" / "2026" / "deep.txt").write_text("2")
    # cursor starts on "..", move to "photos", select the folder, upload
    keys = ["u", "down", " ", "u", "enter", "enter", "y", "y", "enter", "q"]
    ui, term = make_ui(vault_module, vault_server, keys, tmp_path, last_dir=str(source))
    ui.menu()
    folders = {u["name"]: u["folder"] for u in vault_server.uploads}
    assert folders == {"top.txt": "/photos", "deep.txt": "/photos/2026"}


def test_go_to_path_accepts_quoted_paths_and_selects_a_file(vault_module, vault_server, tmp_path):
    target = tmp_path / "elsewhere"
    target.mkdir()
    (target / "one.txt").write_text("1")
    quoted = '"' + str(target) + '"'
    keys = ["u", "g"] + type_text(quoted) + ["enter", "a", "u", "enter", "enter", "y", "enter", "q"]
    ui, term = make_ui(vault_module, vault_server, keys, tmp_path, last_dir=str(tmp_path))
    ui.menu()
    assert [u["name"] for u in vault_server.uploads] == ["one.txt"]
    assert str(target) in vault_module.load_config()["recent_paths"]


def test_a_new_text_note_is_saved(vault_module, vault_server, tmp_path):
    keys = ["n"] + type_text("line one") + ["enter"] + type_text("line two") + ["\x04", "enter", "enter", "q"]
    ui, term = make_ui(vault_module, vault_server, keys, tmp_path)
    ui.menu()
    assert vault_server.uploads == [{"text": "line one\nline two", "folder": "/", "tags": []}]


def test_first_run_setup_asks_for_server_and_key_and_saves_them(vault_module, vault_server, tmp_path):
    keys = ["enter"] + type_text(vault_server.url) + ["enter"] + type_text(KEY) + ["enter", "q"]
    term = FakeTerminal(keys)
    ui = vault_module.UI(term, "", "")
    vault_module.UI_ACTIVE = True
    ui.run()
    saved = vault_module.load_config()
    assert saved["server"] == vault_server.url and saved["key"] == KEY
    assert not any(KEY in frame for frame in term.frames), "the key is masked while typing"


def test_a_wrong_key_is_reported_and_not_saved(vault_module, vault_server, tmp_path):
    keys = ["enter"] + type_text(vault_server.url) + ["enter"] + type_text("wrong") + ["enter", "enter", "esc"]
    term = FakeTerminal(keys)
    ui = vault_module.UI(term, "", "")
    vault_module.UI_ACTIVE = True
    ui.run()
    assert term.seen("HTTP 401")
    assert "key" not in vault_module.load_config()


def test_cancelling_a_transfer_stops_it(vault_module, vault_server, tmp_path):
    ui, term = make_ui(vault_module, vault_server, [], tmp_path)
    job = vault_module.Job("x", 100)
    cancel = threading.Event()
    cancel.set()
    progress = vault_module.Progress("x", 100, job, cancel)
    with pytest.raises(vault_module.VaultError, match="cancelled"):
        progress.update(10)
    assert job.done == 10


def test_cli_commands_still_work(vault_module, vault_server, tmp_path, capsys):
    v = vault_module
    client = v.Client(vault_server.url, KEY)
    first = next(i for i in vault_server.items if i["original_filename"] == "report-01.txt")
    args = v.build_parser().parse_args(["get", first["id"][:8], "-o", str(tmp_path / "out")])
    args.run(client, args)
    assert (tmp_path / "out" / "report-01.txt").exists()
    ls = v.build_parser().parse_args(["ls", "report-02"])
    ls.run(client, ls)
    assert "report-02.txt" in capsys.readouterr().out
    file = tmp_path / "up.txt"
    file.write_text("hi")
    up = v.build_parser().parse_args(["up", str(file), "-f", "/cli", "-t", "a,b"])
    up.run(client, up)
    assert vault_server.uploads[-1]["folder"] == "/cli" and vault_server.uploads[-1]["tags"] == ["a", "b"]


def test_without_a_terminal_and_without_arguments_it_prints_help(tmp_path):
    env = {**os.environ, "VAULT_CONFIG": str(tmp_path / "c.json"), "PYTHONIOENCODING": "utf-8"}
    done = subprocess.run([sys.executable, "-I", str(SCRIPT)], capture_output=True, text=True, env=env, stdin=subprocess.DEVNULL, timeout=30)
    assert done.returncode == 0
    assert "usage: vault.py" in done.stdout


def test_errors_exit_non_zero_with_a_readable_message(tmp_path):
    env = {**os.environ, "VAULT_CONFIG": str(tmp_path / "c.json")}
    done = subprocess.run([sys.executable, "-I", str(SCRIPT), "ls"], capture_output=True, text=True, env=env, stdin=subprocess.DEVNULL, timeout=30)
    assert done.returncode == 2
    assert "python vault.py` once to set them up" in done.stderr


@pytest.mark.skipif(os.name == "nt", reason="needs a POSIX pseudo-terminal")
def test_real_terminal_smoke_test(vault_server, tmp_path):
    import pty
    import select
    import time

    env = {**os.environ, "VAULT_CONFIG": str(tmp_path / "c.json"), "VAULT_SERVER": vault_server.url,
           "VAULT_API_KEY": KEY, "TERM": "xterm", "COLUMNS": "100", "LINES": "30"}
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(sys.executable, [sys.executable, "-I", str(SCRIPT)], env)
    output = b""

    def pump(seconds):
        nonlocal output
        end = time.time() + seconds
        while time.time() < end:
            if select.select([fd], [], [], 0.1)[0]:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    return
                if not chunk:
                    return
                output += chunk

    pump(1.5)
    assert b"Browse the vault" in output
    os.write(fd, b"b")
    pump(1.0)
    assert b"report-01.txt" in output
    os.write(fd, b"\x1b[B\x1b[B")                  # two arrow-key presses must not be mistaken for Escape
    pump(0.5)
    assert b"report-01.txt" in output
    os.write(fd, b"q")
    pump(0.4)
    os.write(fd, b"q")
    pump(1.0)
    _, status = os.waitpid(pid, 0)
    assert os.WEXITSTATUS(status) == 0
    assert b"\x1b[?1049h" in output and b"\x1b[?1049l" in output, "enters and leaves the alternate screen"
