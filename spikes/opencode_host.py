"""Test native OpenCode TUI background-shell wakeup and Axiopera inbox delivery.

Run `cargo build --locked`, then `python spikes/opencode_host.py /path/to/opencode`.
It covers an idle TUI and a TUI running another foreground tool. Each run uses an
isolated profile, a private OpenCode server, and a local model. Windows needs pywinpty.
"""

import argparse
import contextlib
import json
import os
import shutil
import sqlite3
import subprocess
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from probe import AXIOPERA, Pty

MODEL = "fixture/probe"


def run(root, opencode, axiopera, busy):
    cwd, home = root / "project", root / "axiopera"
    cwd.mkdir()
    home.mkdir()
    (home / "config.toml").write_text(f"[agents.opencode]\npath = {json.dumps(str(opencode))}\n", encoding="utf-8")
    keys = ("PATH", "PATHEXT", "ComSpec", "SystemRoot", "WINDIR", "ProgramFiles", "ProgramFiles(x86)",
            "ProgramW6432", "PROCESSOR_ARCHITECTURE", "LANG")
    env = {k: os.environ[k] for k in keys if k in os.environ}
    env["PATH"] = os.pathsep.join((str(axiopera.parent), str(opencode.parent), env["PATH"]))
    for key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
                "XDG_CACHE_HOME", "XDG_STATE_HOME", "TEMP", "TMP", "TMPDIR"):
        directory = root / key
        directory.mkdir()
        env[key] = str(directory)
    env.update(AXIOPERA_HOME=str(home), OPENCODE_DISABLE_AUTOUPDATE="1", OPENCODE_DISABLE_MODELS_FETCH="1",
               TERM="xterm-256color")
    child, release, got = (threading.Event() for _ in range(3))
    errors = []
    if busy:
        (cwd / "busy.py").write_text(
            "from pathlib import Path\nimport time\nPath('busy-started').touch()\n"
            "for _ in range(600):\n    if Path('release-tool').exists(): break\n    time.sleep(.1)\n"
            "print('busy-finished')\n", encoding="utf-8")

    def respond(req):
        messages = req["messages"]
        users = "\n".join(str(m["content"]) for m in messages if m["role"] == "user")
        tools = [t["function"]["name"] for t in req.get("tools", [])]
        if not tools:
            return "Fixture title"
        if "RUN-OC-HOST" not in users:
            if "RUN-OC-CHILD" in users:
                child.set()
                assert release.wait(90), "child timed out waiting for the parent"
                return "child-return-9127"
            return "Fixture"
        assert "shell" in tools, tools
        outputs = [str(m["content"]) for m in messages if m["role"] == "tool"]
        if not outputs:
            return {"command": f"axiopera --json spawn opencode --tier full --model {MODEL} --name sample RUN-OC-CHILD"}
        if len(outputs) == 1:
            assert json.loads(outputs[0])["data"]["task"], outputs[0]
            return {"command": "axiopera wait sample", "background": True}
        assert "moved to the background" in outputs[1], outputs[1]
        if '<shell id="' not in users:
            if busy:
                assert len(outputs) == 2, "background notice missing at the next tool boundary"
                return {"command": ("python" if os.name == "nt" else "python3") + " busy.py"}
            return "Parent waiting"
        assert 'state="completed"' in users and 'command="axiopera wait sample"' in users, users[-600:]
        if busy:
            assert "busy-finished" in outputs[2], outputs[2]
        if len(outputs) == (3 if busy else 2):
            return {"command": "axiopera inbox sample"}
        assert "child-return-9127" in outputs[-1], outputs[-1]
        got.set()
        return "Parent received result"

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            try:
                value = respond(request)
            except Exception as exc:
                errors.append(str(exc))
                value = "fixture error"
            if isinstance(value, dict):
                delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": uuid.uuid4().hex, "type": "function",
                         "function": {"name": "shell", "arguments": json.dumps(value)}}]}
                finish = "tool_calls"
            else:
                delta, finish = {"role": "assistant", "content": value}, "stop"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for content, reason in ((delta, None), ({}, finish)):
                data = {"id": "fixture", "object": "chat.completion.chunk", "created": 1, "model": "probe",
                        "choices": [{"index": 0, "delta": content, "finish_reason": reason}]}
                self.wfile.write(("data: " + json.dumps(data) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        env["OPENCODE_CONFIG_CONTENT"] = json.dumps({"model": MODEL, "provider": {"fixture": {
            "npm": "@ai-sdk/openai-compatible", "name": "Fixture", "models": {"probe": {"name": "Fixture"}},
            "options": {"baseURL": f"http://127.0.0.1:{server.server_port}/v1", "apiKey": "local-fixture"}}}})
        tui = Pty([str(opencode), "--standalone", "--auto", "--prompt", "RUN-OC-HOST"], cwd, env)
        try:
            def until(test, why):
                deadline = time.monotonic() + 50
                while not test():
                    if errors or time.monotonic() > deadline:
                        raise AssertionError((why, errors, tui.screen.strip()[-240:]))
                    time.sleep(.1)

            database = Path(env["XDG_DATA_HOME"]) / "opencode/opencode.db"

            def exists(query):
                if not database.is_file():
                    return False
                with contextlib.closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)) as db:
                    return db.execute(query).fetchone() is not None

            until(child.is_set, "managed child must reach the model")
            if busy:
                until((cwd / "busy-started").is_file, "host must enter the foreground tool")
            else:
                until(lambda: "Parent waiting" in tui.screen and
                      exists("select 1 from session_message where type='idle'"),
                      "host must finish its turn while the child still runs")
            release.set()
            if busy:
                until(lambda: exists("select 1 from session_inbox where type='synthetic' and delivery='steer' "
                                     "and payload like '%axiopera wait sample%'"),
                      "native shell completion must be queued while the foreground tool runs")
                (cwd / "release-tool").touch()
            until(lambda: got.is_set() and "Parent received result" in tui.screen,
                  "wait completion must start the next turn and retrieve the child result")
            print("OpenCode TUI", "busy delivery" if busy else "idle wake", "and inbox: passed", flush=True)
        finally:
            release.set()
            if busy:
                (cwd / "release-tool").touch()
            tui.close()
            if (home / "daemon/info.json").exists():
                subprocess.run([str(axiopera), "daemon", "stop", "--force"], cwd=cwd, env=env,
                               capture_output=True, timeout=20)
            server.shutdown()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("opencode", type=Path)
    parser.add_argument("--axiopera", type=Path, default=AXIOPERA)
    parser.add_argument("--case", choices=("idle", "busy", "both"), default="both")
    args = parser.parse_args()
    path = Path(tempfile.mkdtemp(prefix="axiopera-opencode-host-"))
    try:
        for case in (("idle", "busy") if args.case == "both" else (args.case,)):
            folder = path / case
            folder.mkdir()
            run(folder, args.opencode.resolve(strict=True), args.axiopera.resolve(strict=True), case == "busy")
    finally:
        for attempt in range(20):
            try:
                shutil.rmtree(path)
                break
            except OSError:
                if attempt == 19:
                    raise
                time.sleep(.25)
