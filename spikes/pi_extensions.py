"""Real Pi, local model. Run after cargo build:

    python spikes/pi_extensions.py /path/to/pi host
    python spikes/pi_extensions.py /path/to/pi subagents

host exercises local Git package install/update/remove, idle and busy delivery,
session isolation, and reload deduplication. Windows needs pywinpty. subagents
downloads two pinned npm packages into temporary homes and checks child identity,
RPC completion, and both turns recorded by the daemon. Neither command needs a model key.
"""

import argparse
import contextlib
import json
import os
import queue
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from probe import AXIOPERA, Pty

ROOT = Path(__file__).resolve().parents[1]
MODEL = "fixture/probe"
NOTICE = "Run axiopera inbox to read your task updates."


class Fixture:
    def __init__(self, root, pi):
        self.root, self.pi = root, pi
        self.cwd, self.agent = root / "project", root / "agent"
        for path in (self.cwd, self.agent, root / "temp", root / "home"):
            path.mkdir(parents=True)
        keys = ("PATH", "PATHEXT", "ComSpec", "SystemRoot", "WINDIR", "PROCESSOR_ARCHITECTURE",
                "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ALLUSERSPROFILE", "LANG")
        self.env = {k: os.environ[k] for k in keys if k in os.environ}
        self.env.update(
            PATH=os.pathsep.join((str(pi.parent), str(AXIOPERA.parent), self.env["PATH"])),
            HOME=str(root / "home"), USERPROFILE=str(root / "home"), APPDATA=str(root / "appdata"),
            LOCALAPPDATA=str(root / "local"), XDG_CONFIG_HOME=str(root / "config"), XDG_DATA_HOME=str(root / "data"),
            XDG_CACHE_HOME=str(root / "cache"), TEMP=str(root / "temp"), TMP=str(root / "temp"),
            TMPDIR=str(root / "temp"), PI_CODING_AGENT_DIR=str(self.agent), AXIOPERA_HOME=str(root / "ax"),
            NPM_CONFIG_CACHE=str(root / "npm-cache"), NPM_CONFIG_USERCONFIG=str(root / "npmrc"),
            NPM_CONFIG_GLOBALCONFIG=str(root / "global-npmrc"), NPM_CONFIG_AUDIT="false",
            NPM_CONFIG_FUND="false", GIT_CONFIG_NOSYSTEM="1", TERM="xterm-256color",
        )

    def run(self, argv, **kwargs):
        return subprocess.run(argv, cwd=kwargs.pop("cwd", self.cwd), env=self.env, capture_output=True,
                              text=True, encoding="utf-8", errors="replace", timeout=180, check=True, **kwargs).stdout

    @contextlib.contextmanager
    def model(self, respond):
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                value = respond(request)
                if isinstance(value, tuple):
                    name, args = value
                    delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": uuid.uuid4().hex,
                             "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}]}
                    reason = "tool_calls"
                else:
                    delta, reason = {"role": "assistant", "content": value}, "stop"
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for item, finish in ((delta, None), ({}, reason)):
                    packet = {"id": "fixture", "object": "chat.completion.chunk", "created": 1, "model": "probe",
                              "choices": [{"index": 0, "delta": item, "finish_reason": finish}]}
                    self.wfile.write(("data: " + json.dumps(packet) + "\n\n").encode())
                self.wfile.write(b"data: [DONE]\n\n")

        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            settings = {"providers": {"fixture": {"api": "openai-completions", "apiKey": "local-fixture",
                        "baseUrl": f"http://127.0.0.1:{server.server_port}/v1",
                        "models": [{"id": "probe", "input": ["text"], "reasoning": False,
                                    "contextWindow": 100000, "maxTokens": 4000}]}}}
            (self.agent / "models.json").write_text(json.dumps(settings), encoding="utf-8")
            threading.Thread(target=server.serve_forever, daemon=True).start()
            try:
                yield
            finally:
                server.shutdown()


def wait_until(test, why, timeout=35):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if test():
            return
        time.sleep(0.1)
    raise AssertionError(why)


def user_text(request):
    return "\n".join(json.dumps(m["content"]) for m in request["messages"] if m["role"] == "user")


def host(f):
    origin = f.root / "origin"
    origin.mkdir()
    source = "git:github.com/fixture/axiopera"
    f.env.update(GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0=f"url.{origin.as_uri()}.insteadOf",
                 GIT_CONFIG_VALUE_0="https://github.com/fixture/axiopera", GIT_ALLOW_PROTOCOL="file",
                 GIT_AUTHOR_NAME="Probe", GIT_AUTHOR_EMAIL="probe@localhost",
                 GIT_COMMITTER_NAME="Probe", GIT_COMMITTER_EMAIL="probe@localhost")
    f.run(["git", "init", "-q", "-b", "main"], cwd=origin)
    shutil.copytree(ROOT / "plugins/pi", origin / "plugins/pi")

    def publish(version):
        package = json.loads((ROOT / "package.json").read_text(encoding="utf-8"))
        package["version"] = version
        (origin / "package.json").write_text(json.dumps(package), encoding="utf-8")
        f.run(["git", "add", "."], cwd=origin)
        f.run(["git", "commit", "-qm", version], cwd=origin)

    publish("0.1.0")
    f.run([f.pi, "install", source])
    publish("0.2.0")
    f.run([f.pi, "update", source])
    installed, = [p.parent.parent.parent for p in f.agent.rglob("plugins/pi/index.ts")]
    assert json.loads((installed / "package.json").read_text(encoding="utf-8"))["version"] == "0.2.0"
    f.env["PI_OFFLINE"] = "1"
    caller = "pi:" + str(uuid.uuid4())
    calls = []
    entered, release, steered = (threading.Event() for _ in range(3))

    def respond(request):
        text = user_text(request)
        if "HOST-PROBE-CHILD" in text:
            calls.append("child")
            return "child result"
        calls.append("host")
        if "HOST-PROBE-BUSY" in text and not entered.is_set():
            entered.set()
            release.wait(45)
            return "bash", {"command": "echo host-tool-complete"}
        if "HOST-PROBE-BUSY" in text and text.count(NOTICE) >= 2:
            steered.set()
        return "host done"

    def dispatch(name, owner=caller):
        f.run([AXIOPERA, "--caller", owner, "spawn", "pi", "--model", MODEL, "--tier", "full",
               "--name", name, "HOST-PROBE-CHILD " + name])

    with f.model(respond):
        tui = Pty([str(f.pi), "--session-id", caller[3:], "--model", MODEL, "--approve"], f.cwd, f.env)
        try:
            wait_until(lambda: bool(tui.screen.strip()), "Pi did not start")
            assert not (f.root / "ax/daemon/info.json").exists()
            dispatch("foreign", "pi:" + str(uuid.uuid4()))
            wait_until(lambda: "child" in calls, "foreign child did not run")
            time.sleep(3)
            assert "host" not in calls
            dispatch("own")
            wait_until(lambda: "host" in calls, "idle owner did not wake")
            tui.type("/reload")
            time.sleep(3)
            assert calls.count("host") == 1
            tui.type("HOST-PROBE-BUSY")
            assert entered.wait(15)
            dispatch("busy")
            wait_until(lambda: calls.count("child") == 3, "third child did not run")
            time.sleep(2)
            release.set()
            assert steered.wait(20), "busy host did not receive the notice after the tool"
            f.run([f.pi, "remove", source])
            assert not installed.exists() and "fixture/axiopera" not in f.run([f.pi, "list"])
            tui.type("/reload")
            time.sleep(2)
            before = calls.count("host")
            dispatch("removed")
            wait_until(lambda: calls.count("child") == 4, "fourth child did not run")
            time.sleep(3)
            assert calls.count("host") == before
        finally:
            release.set()
            tui.close()
            f.run([AXIOPERA, "daemon", "stop", "--force"])
    print("Pi TUI: Git install/update/remove, owner filtering, idle wake, busy steer and reload dedup passed")


PACKAGES = {"nico": "pi-subagents@0.89.2", "tintin": "@tintinweb/pi-subagents@0.26.2"}


def subagents(f, name):
    package = "npm:" + PACKAGES[name]
    f.run([f.pi, "install", package])
    f.env.update(PI_OFFLINE="1", AXIOPERA_TASK_ID="probe-task")
    session = str(uuid.uuid4())
    release = threading.Event()
    identity = f.cwd / "identity.json"
    code = ("import os,json;open('identity.json','w').write(json.dumps("
            "[os.getenv('PI_SESSION_ID'),os.getenv('AXIOPERA_TASK_ID')]))")
    command = shlex.quote(Path(sys.executable).as_posix()) + " -c " + shlex.quote(code)
    parent_sent = False
    replies = 0

    def respond(request):
        nonlocal parent_sent, replies
        text = user_text(request)
        if "PLUGIN-PROBE-CHILD" in text and "PLUGIN-PROBE-PARENT" not in text:
            release.wait(40)
            if not any(m["role"] == "tool" for m in request["messages"]):
                return "bash", {"command": command}
            return "child completed"
        if parent_sent:
            replies += 1
            return "parent waiting" if replies == 1 else "parent finished"
        tools = {tool["function"]["name"] for tool in request["tools"]}
        if "subagents_enable" in tools and "subagent" not in tools:
            return "subagents_enable", {}
        parent_sent = True
        if name == "nico":
            return "subagent", {"agent": "delegate", "task": "PLUGIN-PROBE-CHILD", "async": True}
        return "Agent", {"prompt": "PLUGIN-PROBE-CHILD", "subagent_type": "general-purpose",
                         "description": "Check child identity", "model": MODEL, "run_in_background": True}

    with f.model(respond), subprocess.Popen(
        [f.pi, "--mode", "rpc", "--model", MODEL, "--session-id", session],
        cwd=f.cwd, env=f.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, encoding="utf-8",
    ) as pi:
        events = queue.Queue()

        def read():
            for line in pi.stdout:
                events.put(json.loads(line))

        threading.Thread(target=read, daemon=True).start()
        pi.stdin.write(json.dumps({"type": "prompt", "message": "PLUGIN-PROBE-PARENT"}) + "\n")
        pi.stdin.flush()
        launched = completed = None
        settled = 0
        try:
            while settled < 2:
                event = events.get(timeout=55)
                if event["type"] == "tool_execution_end" and event["toolName"] in ("subagent", "Agent"):
                    details = event["result"]["details"]
                    launched = details.get("asyncId") if name == "nico" else details.get("agentId")
                elif event["type"] == "message_end":
                    message = event["message"]
                    if message.get("customType") == "subagent-notify":
                        completed = message["details"]["runs"][0]["runId"]
                    if message.get("customType") == "subagent-notification":
                        completed = message["details"]["id"]
                elif event["type"] == "agent_settled":
                    settled += 1
                    if settled == 1:
                        assert launched and not identity.exists()
                        release.set()
            child_id, task_id = json.loads(identity.read_text(encoding="utf-8"))
            assert completed == launched and child_id and child_id != session and task_id == "probe-task"
        finally:
            release.set()
            pi.stdin.close()
            try:
                pi.wait(timeout=10)
            except subprocess.TimeoutExpired:
                pi.kill()
    identity.unlink()
    parent_sent, replies = False, 0
    release.clear()
    del f.env["AXIOPERA_TASK_ID"]

    def task():
        row = f.run([AXIOPERA, "--json", "status", "plugin"]).splitlines()[0]
        return json.loads(row)["data"][0]

    with f.model(respond):
        try:
            f.run([AXIOPERA, "spawn", "pi", "--tier", "full", "--model", MODEL,
                   "--name", "plugin", "PLUGIN-PROBE-PARENT"])
            wait_until(lambda: task()["turns"] >= 1, "daemon did not record the first turn")
            assert task()["status"] == "running" and not identity.exists()
            release.set()
            wait_until(lambda: task()["status"] == "idle", "daemon did not release the parent")
            final = task()
            child_id, task_id = json.loads(identity.read_text(encoding="utf-8"))
            assert child_id and child_id != final["session"] and task_id == final["id"]
            assert final["turns"] == 2
            assert "parent finished" in f.run([AXIOPERA, "result", "plugin", "--force"])
        finally:
            release.set()
            f.run([AXIOPERA, "daemon", "stop", "--force"])
    print(f"{package}: distinct child, terminal RPC notification, both managed turns and release passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pi", type=Path)
    parser.add_argument("mode", choices=("host", "subagents"))
    args = parser.parse_args()
    pi = args.pi.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="axiopera-pi-extensions-") as tmp:
        root = Path(tmp)
        if args.mode == "host":
            host(Fixture(root / "host", pi))
        else:
            for plugin in PACKAGES:
                subagents(Fixture(root / plugin, pi), plugin)
