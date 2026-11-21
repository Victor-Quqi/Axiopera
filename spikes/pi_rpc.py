"""Run the real Pi adapter against a local model. Usage: python spikes/pi_rpc.py /path/to/pi"""

import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from probe import AXIOPERA


def exercise(root, pi):
    cwd, data, agent = (root / name for name in ("project", "axiopera", "agent"))
    for directory in (cwd, data, agent):
        directory.mkdir()
    entered, release, abort_entered, abort_release = (threading.Event() for _ in range(4))
    requests = []

    class Model(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            messages = request["messages"]
            last_user = next(m["content"] for m in reversed(messages) if m["role"] == "user")
            user_text = json.dumps(last_user)
            tool_call = None
            if "PI-HOLD-ONE" in user_text and not entered.is_set():
                entered.set()
                release.wait(60)
                tool_call = {"index": 0, "id": "call_probe", "type": "function", "function": {
                    "name": "write", "arguments": json.dumps({"path": "probe.txt", "content": "file-marker"})}}
                answer = ""
            elif "PI-HOLD-CANCEL" in user_text:
                abort_entered.set()
                abort_release.wait(60)
                answer = "cancelled-http"
            elif "PI-RESUMED" in user_text:
                answer = "RESUMED-PASS"
            elif "PI-STEER" in user_text:
                answer = "STEER-PASS"
            else:
                answer = "UNEXPECTED-INPUT"
            try:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                first = {"role": "assistant", "tool_calls": [tool_call]} if tool_call else {"role": "assistant", "content": answer}
                for delta, reason in ((first, None), ({}, "tool_calls" if tool_call else "stop")):
                    chunk = {"id": "chat-probe", "object": "chat.completion.chunk", "created": 1, "model": "probe-model",
                             "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]}
                    self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
                self.wfile.write(b"data: [DONE]\n\n")
            except OSError:
                pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    # Pi accepts a synthetic provider in this disposable agent directory.
    (agent / "models.json").write_text(json.dumps({"providers": {"fixture": {
        "baseUrl": f"http://127.0.0.1:{server.server_port}/v1", "api": "openai-completions",
        "apiKey": "fixture", "models": [{"id": "probe-model", "name": "Probe model", "reasoning": False,
                                         "contextWindow": 128000, "maxTokens": 2048}]
    }}}), encoding="utf-8")
    (data / "config.toml").write_text(f"[agents.pi]\npath = {json.dumps(str(pi))}\nmodel = \"fixture/probe-model\"\n", encoding="utf-8")
    env = {k: os.environ[k] for k in ("PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT", "PROCESSOR_ARCHITECTURE") if k in os.environ}
    env.update({k: str(root) for k in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "TMP", "TEMP")})
    env.update(PI_CODING_AGENT_DIR=str(agent), PI_OFFLINE="1", AXIOPERA_HOME=str(data))

    def cli(*args, check=True):
        result = subprocess.run([AXIOPERA, *args], cwd=cwd, env=env, capture_output=True, text=True, encoding="utf-8", timeout=40)
        if check and result.returncode:
            raise RuntimeError(f"{args[0]} failed: {result.stderr[:400]} {result.stdout[:400]}")
        return result

    def task(status):
        deadline = time.monotonic() + 55
        while time.monotonic() < deadline:
            row = cli("--json", "status", "probe").stdout.splitlines()[0]
            info = json.loads(row)["data"][0]
            if info["status"] == status:
                return info, data / "tasks" / info["id"]
            if info["status"] == "failed":
                raise AssertionError(cli("result", "probe", "--force", check=False).stdout)
            time.sleep(0.1)
        raise TimeoutError(f"task did not reach {status}")

    try:
        assert "fixture/probe-model" in cli("models", "pi").stdout
        assert not requests, "listing models sent an inference request"
        for tier in ("read", "work"):
            assert cli("spawn", "pi", "--tier", tier, "unsupported", check=False).returncode != 0
        cli("spawn", "pi", "--tier", "full", "--name", "probe", "PI-HOLD-ONE")
        assert entered.wait(45), cli("status", "probe", check=False).stdout
        cli("send", "probe", "PI-STEER")
        release.set()
        info, task_dir = task("idle")
        session = info["session"]
        assert (cwd / "probe.txt").read_text() == "file-marker"
        assert any(Path(p).name == "probe.txt" for p in info["changed_files"])
        assert "STEER-PASS" in cli("result", "probe", "--force").stdout
        assert info["turns"] == 1 and info["actual_model"] == "fixture/probe-model"
        events = [json.loads(x) for x in (task_dir / "events.jsonl").read_text(encoding="utf-8").splitlines()]
        assert any(e["kind"] == "input_read" for e in events)
        assert any(m["role"] == "tool" for m in requests[1]["messages"])
        assert "PI-STEER" in json.dumps(requests[1]["messages"])
        cli("send", "probe", "PI-HOLD-CANCEL")
        assert abort_entered.wait(30)
        cli("send", "probe", "PI-QUEUED-CANCEL")
        cli("cancel", "probe")
        task("cancelled")
        abort_release.set()
        cli("send", "probe", "PI-RESUMED")
        info, _ = task("idle")
        assert session == info["session"]
        assert "RESUMED-PASS" in cli("result", "probe", "--force").stdout
        history = json.dumps(requests[-1]["messages"])
        assert "STEER-PASS" in history
        print("Pi full: models, unsupported tiers, write, busy steer receipt, result, cancel and cold resume passed")
    finally:
        release.set()
        abort_release.set()
        try:
            cli("daemon", "stop", "--force", check=False)
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="axiopera-pi-rpc-") as scratch:
        exercise(Path(scratch), Path(sys.argv[1]).resolve(strict=True))
