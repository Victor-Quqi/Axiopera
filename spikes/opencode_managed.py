"""Run managed OpenCode v2 against a local streaming model in a disposable profile.

python spikes/opencode_managed.py /path/to/opencode [--axiopera /path/to/axiopera]
"""

import argparse
import json
import os
import shutil
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from probe import remove_tree

MODEL = "fixture/probe"
READ = {"read", "glob", "grep", "axiopera_ask", "axiopera_tell", "axiopera_answers", "axiopera_list_directory"}


def isolated_profile(root, opencode, binary):
    cwd = root / "project"
    cwd.mkdir()
    home = root / "axiopera-home"
    home.mkdir()
    (home / "config.toml").write_text(f"[agents.opencode]\npath = {json.dumps(str(opencode))}\n", encoding="utf-8")
    exe = Path(shutil.copy2(binary, root / binary.name))
    keys = ("PATH", "PATHEXT", "ComSpec", "SystemRoot", "WINDIR", "ProgramFiles", "PROCESSOR_ARCHITECTURE", "LANG")
    env = {k: os.environ[k] for k in keys if k in os.environ}
    for key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
                "XDG_CACHE_HOME", "XDG_STATE_HOME", "TEMP", "TMP", "TMPDIR"):
        p = root / key
        p.mkdir()
        env[key] = str(p)
    env.update(AXIOPERA_HOME=str(home), OPENCODE_DISABLE_MODELS_FETCH="1", OPENCODE_DISABLE_AUTOUPDATE="1")
    return cwd, exe, env


def send_completion(handler, model, delta, finish):
    handler.send_response(200)
    handler.send_header("Content-Type", "text/event-stream")
    handler.end_headers()
    for part, reason in [({"role": "assistant", **delta}, None), ({}, finish)]:
        chunk = {"id": "chatcmpl_probe", "object": "chat.completion.chunk", "created": 1, "model": model,
                 "choices": [{"index": 0, "delta": part, "finish_reason": reason}]}
        handler.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
    handler.wfile.write(b"data: [DONE]\n\n")


def cleanup_profile(path):
    # Shutdown can acknowledge before Windows unloads Bun's DLL and the daemon finishes writing.
    for attempt in range(60):
        try:
            remove_tree(path)
            return
        except OSError:
            if attempt == 59:
                raise
            time.sleep(.25)


def run(root, opencode, binary):
    cwd, exe, env = isolated_profile(root, opencode, binary)
    (cwd / "fixture.txt").write_text("native-read-marker", encoding="utf-8")
    entered, release = threading.Event(), threading.Event()
    sampled = []

    class Model(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            tools = {t["function"]["name"] for t in body.get("tools", [])}
            messages = body["messages"]
            document = json.dumps(messages)
            tool = None
            if tools:
                sampled.append((tools, document))
                if "read-request" in document and not any(m["role"] == "tool" for m in messages):
                    tool = ("read", {"path": str(cwd / "fixture.txt")})
                elif "write-request" in document and not any(m["role"] == "tool" for m in messages):
                    tool = ("write", {"path": str(cwd / "written.txt"), "content": "native-write-marker"})
                elif "shell-request" in document and not any(m["role"] == "tool" for m in messages):
                    tool = ("shell", {"command": "echo shell-marker"})
                elif "pause-request" in document and "steering-marker" not in document:
                    entered.set()
                    release.wait(20)
            if tool:
                name, args = tool
                delta = {"tool_calls": [{"index": 0, "id": "call_fixture", "type": "function",
                                        "function": {"name": name, "arguments": json.dumps(args)}}]}
            else:
                delta = {"content": "fixture-finished"}
            send_completion(self, body["model"], delta, "tool_calls" if tool else "stop")

    def cli(*args):
        result = subprocess.run([str(exe), "--json", "--caller", "manual:opencode-probe", *args],
                                env=env, cwd=cwd, capture_output=True, text=True, encoding="utf-8", timeout=50)
        if result.returncode:
            raise RuntimeError(result.stderr.strip() or result.stdout.strip())
        return json.loads(result.stdout).get("data")

    def spawn(tier, prompt):
        return cli("spawn", "opencode", "--model", MODEL, "--tier", tier, "--cwd", str(cwd), prompt)["task"]

    def until(predicate):
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            if value := predicate():
                return value
            time.sleep(0.15)
        raise TimeoutError("OpenCode probe did not reach the expected event")

    def finished(task, turn=1):
        until(lambda: any(e["kind"] == "turn_ended" and e["turn"] == turn and e["ok"] for e in cli("log", task)))

    def check_tool(task, name, target):
        events = cli("log", task)
        calls = [e["id"] for e in events if e["kind"] == "tool" and e["name"] == name]
        assert len(calls) == 1 and any(e["kind"] == "tool_updated" and e["id"] == calls[0]
                                      and e["detail"] == target for e in events), (name, target)

    with ThreadingHTTPServer(("127.0.0.1", 0), Model) as server:
        provider = {"npm": "@ai-sdk/openai-compatible", "name": "Fixture", "models": {"probe": {"name": "Probe"}},
                    "options": {"apiKey": "fixture", "baseURL": f"http://127.0.0.1:{server.server_port}/v1"}}
        env["OPENCODE_CONFIG_CONTENT"] = json.dumps({"model": MODEL, "provider": {"fixture": provider},
                                                      "permission": {"write": "ask"}})
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            assert MODEL in {m["id"] for m in cli("models", "opencode", "--cwd", str(cwd))["models"]}
            read = spawn("read", "read-request")
            finished(read)
            assert sampled and all(names == READ for names, _ in sampled)
            assert any("native-read-marker" in body for _, body in sampled)
            check_tool(read, "read", str(cwd / "fixture.txt"))
            for command in ("deny", "approve"):
                sampled.clear()
                task = spawn("full", "write-request")
                rid = until(lambda: next((e["id"] for e in cli("log", task) if e["kind"] == "approval_request"), None))
                check_tool(task, "write", str(cwd / "written.txt"))
                cli("inbox", task)
                cli(command, rid)
                finished(task)
                assert (cwd / "written.txt").exists() == (command == "approve")
            assert (cwd / "written.txt").read_text(encoding="utf-8") == "native-write-marker"
            shell = spawn("full", "shell-request")
            finished(shell)
            check_tool(shell, "shell", "echo shell-marker")
            task = spawn("read", "pause-request")
            assert entered.wait(30)
            cli("send", task, "steering-marker")
            release.set()
            finished(task)
            events = cli("log", task)
            assert any(e["kind"] == "input_read" for e in events)
            assert any("steering-marker" in body for _, body in sampled)
            assert cli("status", task)[0]["turns"] == 1
            cli("send", task, "resume-marker")
            finished(task, 2)
            print("OpenCode catalog, read tool restrictions, native file I/O, one-shot approvals, steering and resume passed.")
        finally:
            release.set()
            try:
                cli("daemon", "stop", "--force")
            finally:
                server.shutdown()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("opencode", type=Path)
    default = Path(__file__).resolve().parents[1] / "target/debug" / ("axiopera.exe" if os.name == "nt" else "axiopera")
    parser.add_argument("--axiopera", type=Path, default=default)
    args = parser.parse_args()
    tmp = Path(tempfile.mkdtemp(prefix="axiopera-opencode-managed-"))
    try:
        run(tmp, args.opencode.resolve(strict=True), args.axiopera.resolve(strict=True))
    finally:
        cleanup_profile(tmp)
