"""Compare Grok native parent/child identities and replay their inbox calls.

Runs a real native subagent both directly and inside an Axiopera Grok task.
Only identity keys are read, kept in memory, and never printed. Each native child
also dispatches a real Axiopera task and collects its result after the parent polls.
Synthetic results in a separate data directory check both caller identities too.
Usage: python spikes/grok_native_identity.py [direct|nested|both]
"""

import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from probe import AXIOPERA, GROK, clean_env, forget_sessions, grok_acp, log, remove_tree, workdir


CAPTURE = r'''import json, os, re, subprocess, sys, urllib.request
p = subprocess.run([AXIOPERA, "guide", "--host", "grok"], capture_output=True, text=True, encoding="utf-8", check=True)
caller = re.search(r"watch --caller ([^`\s]+)", p.stdout).group(1)
payload = {"role": sys.argv[1], "caller": caller,
           "identity": {k: os.environ.get(k) for k in ("GROK_SESSION_ID", "AXIOPERA_TASK_ID")}}
request = urllib.request.Request(URL, data=json.dumps(payload).encode(), headers={"Content-Type": "application/json"})
with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=10) as response:
    assert response.status == 200
if sys.argv[1] == "parent":
    print("Identity captured.")
'''

LEAF_MARKER = "NATIVE_LIVE_RESULT_72941"
LIVE = r'''
if sys.argv[1] == "child":
    import subprocess, time
    def invoke(*args, timeout=45):
        p = subprocess.run([AXIOPERA, *args], capture_output=True, text=True,
                           encoding="utf-8", timeout=timeout)
        assert p.returncode in (0, 3, 4), "probe CLI failed: " + args[0]
        return p.stdout
    def post(payload):
        request = urllib.request.Request(URL, data=json.dumps(payload).encode(),
                                         headers={"Content-Type": "application/json"})
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=45) as response:
            assert response.status == 200
    def failed(kind, error, tb):
        try:
            post({"stage": "error", "kind": kind.__name__, "message": str(error)[:300]})
        finally:
            sys.__excepthook__(kind, error, tb)
    sys.excepthook = failed
    invoke("guide", "--host", AGENT)
    text = invoke("--json", "spawn", AGENT, "--tier", "read", "--timeout", "2m",
                  "Reply with exactly " + LEAF_MARKER + ". Do not call any tools.")
    task = json.loads(text.splitlines()[0])["data"]["task"]
    invoke("wait", task, "--timeout", "150s", timeout=160)
    deadline = time.monotonic() + 30
    while True:
        state = json.loads(invoke("--json", "status", task).splitlines()[0])["data"][0]["status"]
        if state not in ("queued", "running") or time.monotonic() >= deadline:
            break
        time.sleep(0.25)
    assert state == "idle", "leaf task ended " + state
    post({"stage": "before_inbox", "task": task})
    received = LEAF_MARKER in invoke("inbox")
    post({"stage": "after_inbox", "received": received})
    assert received, "native child did not receive its task result"
    print("Live inbox checked.")
'''


def receive_live(payload, samples, env, root, live):
    stage = payload.get("stage")
    if stage == "error":
        live["error"] = payload["kind"] + ": " + payload["message"]
        log(root.name, "helper error:", live["error"])
        return True
    if stage == "before_inbox":
        caller_env = dict(env)
        for key, value in samples["parent"]["identity"].items():
            caller_env.pop(key, None)
            if value:
                caller_env[key] = value
        live["live_parent_took_child_result"] = LEAF_MARKER in cli(caller_env, root, "inbox")
        return True
    if stage == "after_inbox":
        live["live_child_received_result"] = payload["received"]
        return True
    return False


def cli(env, cwd, *args):
    p = subprocess.run([str(AXIOPERA), *args], env=env, cwd=cwd,
                       capture_output=True, text=True, encoding="utf-8", timeout=40)
    if p.returncode not in (0, 3, 4):
        raise RuntimeError(f"axiopera {args[0]} failed (exit {p.returncode})")
    return p.stdout


def bounded_remove(path, root):
    path, root = Path(path).resolve(), Path(root).resolve()
    assert path.is_relative_to(root) and path != root
    remove_tree(path)


def inbox_check(root, env, samples):
    """Replay captured caller environments against a completed child-owned result."""
    home = root / "mailbox"
    task_dir = home / "tasks" / "receipt"
    (task_dir / "turns").mkdir(parents=True)
    now = int(time.time() * 1000)
    task = {"id": "receipt", "parent": samples["child"]["caller"], "agent": "grok",
            "tier": "read", "cwd": str(root), "status": "idle", "turns": 1,
            "delivered": 0, "changed_files": [], "stalled": False, "detached": False,
            "created_ms": now, "updated_ms": now}
    (task_dir / "task.json").write_text(json.dumps(task), encoding="utf-8")
    event = {"seq": 1, "ts": now, "task": "receipt", "kind": "turn_ended", "turn": 1}
    (task_dir / "events.jsonl").write_text(json.dumps(event) + "\n", encoding="utf-8")
    marker = "NATIVE_CHILD_RESULT_72941"
    (task_dir / "turns" / "1.md").write_text(marker, encoding="utf-8")
    # Include the owning task so the daemon can normalize its own native session.
    owner = samples["parent"]["identity"].get("AXIOPERA_TASK_ID")
    parent = samples["parent"]["caller"]
    if owner:
        kind, var = ("codex", "CODEX_THREAD_ID") if "CODEX_THREAD_ID" in samples["parent"]["identity"] else ("grok", "GROK_SESSION_ID")
        owner_task = dict(task, id=owner, parent="user", agent=kind, turns=0,
                          session=samples["parent"]["identity"][var])
        owner_dir = home / "tasks" / owner
        owner_dir.mkdir()
        (owner_dir / "task.json").write_text(json.dumps(owner_task), encoding="utf-8")
        parent = "task:" + owner
    parent_dir = home / "tasks" / "parent_receipt"
    (parent_dir / "turns").mkdir(parents=True)
    (parent_dir / "task.json").write_text(json.dumps(dict(task, id="parent_receipt", parent=parent)), encoding="utf-8")
    (parent_dir / "events.jsonl").write_text(json.dumps(dict(event, task="parent_receipt", seq=2)) + "\n", encoding="utf-8")
    parent_marker = "PARENT_RESULT_72941"
    (parent_dir / "turns" / "1.md").write_text(parent_marker, encoding="utf-8")
    base = dict(env, AXIOPERA_HOME=str(home))
    try:
        took = {}
        for role in ("parent", "child"):
            caller_env = dict(base)
            for key, value in samples[role]["identity"].items():
                caller_env.pop(key, None)
                if value:
                    caller_env[key] = value
            output = cli(caller_env, root, "inbox")
            took[role] = marker in output
            assert (parent_marker in output) == (role == "parent"), "parent routing changed"
        return {"parent_took_child_result": took["parent"],
                "child_received_result_after_parent": took["child"]}
    finally:
        cli(base, root, "daemon", "stop", "--force")


def run_case(root, nested):
    root.mkdir()
    samples, live = {}, {}

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
            log(root.name, "captured", role)
            self.send_response(200)
            self.end_headers()

        def log_message(self, *_):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), CaptureHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_port}/"
    (root / "capture.py").write_text(
        f"AXIOPERA = {str(AXIOPERA)!r}\nURL = {url!r}\nAGENT = 'grok'\nLEAF_MARKER = {LEAF_MARKER!r}\n"
        + CAPTURE + LIVE, encoding="utf-8")
    env = clean_env(AXIOPERA_HOME=str(root / "axiopera"),
                    PATH=f"{AXIOPERA.parent}{os.pathsep}{os.environ['PATH']}")
    env.pop("AXIOPERA_TASK_ID", None)
    for kind in ("MCPS", "HOOKS", "SKILLS", "RULES", "AGENTS"):
        env[f"GROK_CLAUDE_{kind}_ENABLED"] = "0"
    prompt = (
        "Test native subagent identity. In this working directory, run `python capture.py parent` "
        "with your terminal tool. Then use your native spawn_subagent tool to start exactly one "
        "general-purpose child in this same directory. Its entire task is: run `python capture.py child` "
        "with the terminal tool, wait until the command exits and prints 'Live inbox checked.', then report completion. "
        "Wait for that child to finish, then reply DONE. If the helper fails, report the error and stop; do not edit it. "
        "The helper records identity and dispatches a one-message Axiopera probe; do not print environment variables or "
        "read agent configuration. Use native subagent tools for delegation."
    )
    rpc = None
    try:
        if nested:
            (root / "task.txt").write_text(prompt, encoding="utf-8")
            cli(env, root, "guide", "--host", "codex")
            spawned = cli(env, root, "--json", "spawn", "grok", "--tier", "work", "--timeout", "5m", "-f", "task.txt")
            task_id = json.loads(spawned.splitlines()[0])["data"]["task"]
            deadline = time.monotonic() + 330
            last = None
            while time.monotonic() < deadline:
                assert "error" not in live, live.get("error")
                task_path = root / "axiopera" / "tasks" / task_id / "task.json"
                state = json.loads(task_path.read_text(encoding="utf-8"))["status"]
                if state != last:
                    log(root.name, "task", state)
                    last = state
                if state in ("idle", "failed", "cancelled", "interrupted"):
                    break
                time.sleep(1)
            else:
                raise TimeoutError("nested Grok run did not finish")
            if state != "idle":
                raise RuntimeError(f"nested Grok task ended {state}")
        else:
            rpc = grok_acp(root, env=env)
            sid = rpc.call("session/new", {"cwd": str(root), "mcpServers": [],
                                           "_meta": {"autoMode": True, "yoloMode": False}})["sessionId"]
            reply = rpc.call_async("session/prompt", {"sessionId": sid,
                                                      "prompt": [{"type": "text", "text": prompt}]})
            deadline = time.monotonic() + 300
            while reply.empty() and time.monotonic() < deadline:
                assert "error" not in live, live.get("error")
                rpc.next(0.5)
            if reply.empty():
                raise TimeoutError("direct Grok run did not finish")
            if "error" in reply.get():
                raise RuntimeError("direct Grok prompt failed")
        assert set(samples) == {"parent", "child"}, "both agents must execute the helper"
        deadline = time.monotonic() + 180
        while "live_child_received_result" not in live and "error" not in live and time.monotonic() < deadline:
            time.sleep(0.5)
        assert "error" not in live, live.get("error")
        assert "live_child_received_result" in live, "native child command did not finish"
        a, b = samples["parent"], samples["child"]
        assert a["identity"]["GROK_SESSION_ID"] and b["identity"]["GROK_SESSION_ID"]
        report = {
            "native_session_ids_differ": a["identity"]["GROK_SESSION_ID"] != b["identity"]["GROK_SESSION_ID"],
            "parent_has_axiopera_task": bool(a["identity"]["AXIOPERA_TASK_ID"]),
            "child_has_axiopera_task": bool(b["identity"]["AXIOPERA_TASK_ID"]),
            "effective_axiopera_callers_differ": a["caller"] != b["caller"],
            **inbox_check(root, env, samples),
            **live,
        }
        assert report["native_session_ids_differ"] and report["effective_axiopera_callers_differ"]
        assert not report["parent_took_child_result"] and report["child_received_result_after_parent"]
        assert not report["live_parent_took_child_result"] and report["live_child_received_result"]
        log(root.name, json.dumps(report))
        return report
    finally:
        if rpc:
            rpc.close()
        cli(env, root, "daemon", "stop", "--force")
        server.shutdown()
        server.server_close()
        time.sleep(2)
        # Only sessions belonging to this unique test directory are removed.
        forget_sessions(root)


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "both"
    if mode not in ("direct", "nested", "both"):
        raise SystemExit("expected direct, nested, or both")
    root = workdir("grok-native-identity").resolve()
    log(subprocess.run([GROK, "--version"], capture_output=True, text=True).stdout.strip())
    try:
        cases = ("direct", "nested") if mode == "both" else (mode,)
        results = {case: run_case(root / case, case == "nested") for case in cases}
        print(json.dumps(results, indent=2), flush=True)
    finally:
        bounded_remove(root, root.parent)


if __name__ == "__main__":
    main()
