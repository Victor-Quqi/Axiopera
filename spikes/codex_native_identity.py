"""Probe native Codex child identity, directly and inside an Axiopera task.

Capture only the two routing identity keys in memory; never print their values.
Check synthetic inboxes and a real task dispatched by the native child.
Usage: python spikes/codex_native_identity.py [direct|nested|both]
"""

import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from grok_native_identity import LEAF_MARKER, LIVE, bounded_remove, cli, inbox_check, receive_live
from probe import AXIOPERA, CODEX, Rpc, RpcError, clean_env, log, workdir


CAPTURE = r'''import json, os, sys, urllib.request
identity = {k: os.environ.get(k) for k in ("CODEX_THREAD_ID", "AXIOPERA_TASK_ID")}
assert identity["CODEX_THREAD_ID"], "missing native thread identity"
task = identity["AXIOPERA_TASK_ID"]
native = "codex:" + identity["CODEX_THREAD_ID"]
caller = "task:" + task + "/" + native if task else native
payload = {"role": sys.argv[1], "caller": caller, "identity": identity}
request = urllib.request.Request(URL, data=json.dumps(payload).encode(), headers={"Content-Type": "application/json"})
with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=10) as response:
    assert response.status == 200
if sys.argv[1] == "parent":
    print("Identity captured.")
'''


def app_server(root, env):
    rpc = Rpc([CODEX, "app-server"], root, env=env)
    rpc.call("initialize", {"clientInfo": {"name": "axiopera-spike", "version": "0"},
                            "capabilities": {"experimentalApi": True}})
    rpc.notify("initialized", {})
    return rpc


def delete_threads(root, env, known):
    rpc = app_server(root, env)
    try:
        threads = rpc.call("thread/list", {"cwd": str(root), "limit": 100})["data"]
        for thread in threads:
            if Path(thread["cwd"]).resolve() == root.resolve():
                known.add(thread["id"])
        for tid in known:
            try:
                rpc.call("thread/delete", {"threadId": tid})
            except RpcError as error:
                # A native child can already be removed when its parent is deleted.
                if "no rollout found for thread id" not in str(error):
                    raise
        remaining = rpc.call("thread/list", {"cwd": str(root), "limit": 100})["data"]
        assert not any(Path(t["cwd"]).resolve() == root.resolve() for t in remaining)
        log(root.name, "test thread cleanup complete")
    finally:
        rpc.close()


def run_case(root, nested):
    root.mkdir()
    samples, known, live = {}, set(), {}

    class CaptureHandler(BaseHTTPRequestHandler):
        def do_POST(self):
            payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if receive_live(payload, samples, env, root, live):
                self.send_response(200)
                self.end_headers()
                return
            role = payload["role"]
            if role not in ("parent", "child") or role in samples:
                self.send_error(409)
                return
            samples[role] = payload
            known.add(payload["identity"]["CODEX_THREAD_ID"])
            log(root.name, "captured", role)
            self.send_response(200)
            self.end_headers()

        def log_message(self, *_):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), CaptureHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    (root / "capture.py").write_text(
        f"URL = {'http://127.0.0.1:' + str(server.server_port) + '/'!r}\n"
        f"AXIOPERA = {str(AXIOPERA)!r}\nAGENT = 'codex'\nLEAF_MARKER = {LEAF_MARKER!r}\n"
        + CAPTURE + LIVE,
        encoding="utf-8")
    env = clean_env(AXIOPERA_HOME=str(root / "axiopera"),
                    PATH=f"{AXIOPERA.parent}{os.pathsep}{os.environ['PATH']}")
    env.pop("AXIOPERA_TASK_ID", None)
    prompt = (
        "Test native subagent identity. Run `python capture.py parent` in this working directory. "
        "Then use your native spawn_agent tool to start exactly one child in this same directory. "
        "The child's entire task is: run `python capture.py child`, wait until it exits and prints "
        "'Live inbox checked.', then report completion. "
        "Wait for the child to finish, then reply DONE. This explicitly authorizes native delegation. "
        "If the helper fails, report the error and stop; do not edit it. "
        "The helper captures identity and dispatches a one-message Axiopera probe; do not print environment variables, read agent "
        "configuration, or alter identity variables. Use native subagent tools for delegation."
    )
    rpc = None
    try:
        if nested:
            (root / "task.txt").write_text(prompt, encoding="utf-8")
            cli(env, root, "guide", "--host", "codex")
            spawned = cli(env, root, "--json", "spawn", "codex", "--tier", "full", "--timeout", "6m", "-f", "task.txt")
            task_id = json.loads(spawned.splitlines()[0])["data"]["task"]
            deadline, last = time.monotonic() + 390, None
            while time.monotonic() < deadline:
                assert "error" not in live, live.get("error")
                task_path = root / "axiopera" / "tasks" / task_id / "task.json"
                task = json.loads(task_path.read_text(encoding="utf-8"))
                if task.get("session"):
                    known.add(task["session"])
                state = task.get("status", "pending")
                if state != last:
                    log(root.name, "task", state)
                    last = state
                if state in ("idle", "failed", "cancelled", "interrupted"):
                    break
                time.sleep(1)
            else:
                raise TimeoutError("nested Codex run did not finish")
            if state != "idle":
                raise RuntimeError(f"nested Codex task ended {state}")
        else:
            rpc = app_server(root, env)
            thread = rpc.call("thread/start", {"cwd": str(root), "sandbox": "danger-full-access",
                                               "approvalPolicy": "never"})["thread"]
            sid = thread["id"]
            known.add(sid)
            rpc.call("turn/start", {"threadId": sid, "input": [{"type": "text", "text": prompt}]})
            deadline = time.monotonic() + 360
            while time.monotonic() < deadline:
                assert "error" not in live, live.get("error")
                message = rpc.next(0.5)
                if message in (None, ...):
                    continue
                params = message.get("params", {})
                if message.get("method") == "thread/started":
                    known.add(params["thread"]["id"])
                if message.get("method") == "turn/completed" and params.get("threadId") == sid:
                    if params.get("turn", {}).get("status") != "completed":
                        raise RuntimeError("direct Codex turn failed")
                    break
            else:
                raise TimeoutError("direct Codex run did not finish")
        assert set(samples) == {"parent", "child"}, "both agents must execute the helper"
        a, b = samples["parent"], samples["child"]
        report = {
            "native_thread_ids_differ": a["identity"]["CODEX_THREAD_ID"] != b["identity"]["CODEX_THREAD_ID"],
            "parent_has_axiopera_task": bool(a["identity"]["AXIOPERA_TASK_ID"]),
            "child_has_axiopera_task": bool(b["identity"]["AXIOPERA_TASK_ID"]),
            "effective_axiopera_callers_differ": a["caller"] != b["caller"],
            **inbox_check(root, env, samples),
            **live,
        }
        assert report["native_thread_ids_differ"] and report["effective_axiopera_callers_differ"]
        assert not report["parent_took_child_result"] and report["child_received_result_after_parent"]
        assert not report["live_parent_took_child_result"] and report["live_child_received_result"]
        log(root.name, json.dumps(report))
        return report
    finally:
        try:
            if rpc:
                rpc.close()
            cli(env, root, "daemon", "stop", "--force")
        finally:
            server.shutdown()
            server.server_close()
            time.sleep(2)
            for path in (root / "axiopera" / "tasks").glob("*/task.json"):
                if sid := json.loads(path.read_text(encoding="utf-8")).get("session"):
                    known.add(sid)
            delete_threads(root, env, known)


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "both"
    if mode not in ("direct", "nested", "both"):
        raise SystemExit("expected direct, nested, or both")
    root = workdir("codex-native-identity").resolve()
    log(subprocess.run([CODEX, "--version"], capture_output=True, text=True,
                       encoding="utf-8", errors="replace").stdout.strip())
    try:
        cases = ("direct", "nested") if mode == "both" else (mode,)
        results = {case: run_case(root / case, case == "nested") for case in cases}
        print(json.dumps(results, indent=2), flush=True)
    finally:
        bounded_remove(root, root.parent)


if __name__ == "__main__":
    main()
