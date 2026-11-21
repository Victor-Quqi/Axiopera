"""Real Pi, local model. Run after cargo build:

    python spikes/pi_extensions.py /path/to/pi host
    python spikes/pi_extensions.py /path/to/pi subagents

host checks Git install, update and removal, the receipts Pi sees, automatic
inbox delivery, and foreground wait after removal. subagents installs two pinned
npm packages in temporary homes and checks native child inboxes, RPC completion,
and turns recorded by the daemon. Windows needs pywinpty.
"""

import argparse
import contextlib
import json
import os
import queue
import re
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
    receipts, inbox = {}, []
    initial_context = []
    release_child, entered, release_busy, defer = (threading.Event() for _ in range(4))
    commands = {
        "HOST-SPAWN": ('[[ $AXIOPERA_PI_NOTIFY == "pi:$PI_SESSION_ID/"?* && -z $AXIOPERA_PI_WATCH ]] && '
                       f"axiopera spawn pi --model {MODEL} --tier full --name own HOST-PROBE-CHILD-own"),
        "HOST-SEND": "axiopera send own HOST-PROBE-CHILD-resumed",
        "HOST-BARE": ('[[ -z $AXIOPERA_PI_NOTIFY && -z $AXIOPERA_PI_WATCH ]] && '
                      f"axiopera spawn pi --model {MODEL} --tier full --name removed HOST-PROBE-CHILD-removed"),
        "HOST-TAKE": "echo ready > host-waiting && axiopera wait removed",
    }

    def respond(request):
        messages = request["messages"]
        last = messages[-1]
        last_user = next(str(m["content"]) for m in reversed(messages) if m["role"] == "user")
        if "HOST-UNRELATED" in last_user:
            initial_context.append(str(messages))
            return "ordinary turn finished"
        child = re.search(r"HOST-PROBE-CHILD-([a-z]+)", last_user)
        if child:
            name = child[1]
            calls.append(name)
            if name in ("own", "resumed", "answered", "removed", "blocked"):
                release_child.wait(45)
            if name == "resumed" and last["role"] == "user":
                return "bash", {"command": "axiopera ask Which-marker"}
            return f"child-result-{name}"
        calls.append("host")
        if last["role"] == "tool":
            tool = next(m for m in reversed(messages) if m.get("tool_calls"))["tool_calls"][0]
            command = json.loads(tool["function"]["arguments"])["command"]
            if command == commands["HOST-TAKE"]:
                if "axiopera inbox" in str(last["content"]):
                    return "bash", {"command": "axiopera inbox removed"}
                return "wait returned without a collection instruction"
            if command in commands.values():
                receipts[command] = str(last["content"])
                prompt = next(prompt for prompt, action in commands.items() if action == command)
                return "host finished " + prompt
            if "axiopera inbox" in command:
                inbox.append(str(last["content"]))
                return "host collected"
        if last["role"] == "user" and NOTICE in last_user:
            if defer.is_set():
                return "host deferred collection"
            return "bash", {"command": "axiopera inbox"}
        if last["role"] == "user":
            for prompt, command in commands.items():
                if prompt in last_user:
                    return "bash", {"command": command}
        if "HOST-PROBE-BUSY" in last_user and not entered.is_set():
            entered.set()
            release_busy.wait(45)
            return "bash", {"command": "echo host-tool-complete"}
        return "host done"

    def dispatch(name, owner=caller):
        receipt = f.run([AXIOPERA, "--caller", owner, "spawn", "pi", "--model", MODEL, "--tier", "full",
                         "--name", name, "HOST-PROBE-CHILD-" + name])
        assert "in the foreground" in receipt and "Pi will notify" not in receipt, receipt

    with f.model(respond):
        tui = Pty([str(f.pi), "--session-id", caller[3:], "--model", MODEL, "--approve"], f.cwd, f.env)
        try:
            wait_until(lambda: bool(tui.screen.strip()), "Pi did not start")
            tui.type("HOST-UNRELATED")
            wait_until(lambda: "ordinary turn finished" in tui.screen, "ordinary turn did not finish")
            assert len(initial_context) == 1
            assert not any(cmd in initial_context[0] for cmd in ("axiopera inbox", "axiopera wait"))
            assert not (f.root / "ax/daemon/info.json").exists()

            def receipt(prompt, automatic):
                tui.type(prompt)
                command = commands[prompt]
                wait_until(lambda: command in receipts, f"{prompt} did not return a receipt to the host")
                output = receipts[command]
                assert "axiopera inbox" not in output, output
                if automatic:
                    assert "Pi will notify this session" in output and "axiopera wait" not in output, output
                else:
                    assert "in the foreground" in output and "Pi will notify" not in output, output
                wait_until(lambda: "host finished " + prompt in tui.screen, "host did not finish its turn")
                print(f"{prompt}: {output.strip()}", flush=True)

            def collected(name):
                wait_until(lambda: any("child-result-" + name in output for output in inbox),
                           f"host did not take the {name} result")

            for prompt, name in (("HOST-SPAWN", "own"), ("HOST-SEND", "resumed")):
                release_child.clear()
                receipt(prompt, automatic=True)
                wait_until(lambda: name in calls, f"{name} child did not run")
                release_child.set()
                collected(name)

            question = re.search(r"axiopera reply (q[\w-]+)", "\n".join(inbox))
            assert question, inbox
            commands["HOST-REPLY"] = f"axiopera reply {question[1]} HOST-PROBE-CHILD-answered"
            release_child.clear()
            receipt("HOST-REPLY", automatic=True)
            release_child.set()
            collected("answered")

            before = calls.count("host")
            dispatch("foreign", "pi:" + str(uuid.uuid4()))
            wait_until(lambda: "foreign" in calls, "foreign child did not run")
            time.sleep(3)
            assert calls.count("host") == before
            defer.set()
            dispatch("unread")
            wait_until(lambda: "host deferred collection" in tui.screen, "host did not see unread news")
            defer.clear()
            before = calls.count("host")
            tui.type("/reload")
            time.sleep(3)
            assert calls.count("host") == before
            tui.type("HOST-PROBE-BUSY")
            assert entered.wait(15)
            dispatch("busy")
            wait_until(lambda: "busy" in calls, "busy child did not run")
            time.sleep(2)
            release_busy.set()
            collected("busy")
            collected("unread")
            f.run([f.pi, "remove", source])
            assert not installed.exists() and "fixture/axiopera" not in f.run([f.pi, "list"])
            tui.type("/reload")
            time.sleep(2)
            release_child.clear()
            receipt("HOST-BARE", automatic=False)
            wait_until(lambda: "removed" in calls, "uninstalled case child did not run")
            before = calls.count("host")
            release_child.set()
            time.sleep(3)
            assert calls.count("host") == before
            tui.type("HOST-TAKE")
            collected("removed")
            (f.cwd / "host-waiting").unlink()

            commands["HOST-BLOCK"] = "axiopera send removed HOST-PROBE-CHILD-blocked"
            release_child.clear()
            receipt("HOST-BLOCK", automatic=False)
            wait_until(lambda: "blocked" in calls, "bare resumed child did not run")
            before = len(inbox)
            tui.type("HOST-TAKE")
            wait_until(lambda: (f.cwd / "host-waiting").exists(), "foreground command did not start")
            time.sleep(.5)
            assert len(inbox) == before
            release_child.set()
            collected("blocked")
        finally:
            release_busy.set()
            release_child.set()
            tui.close()
            f.run([AXIOPERA, "daemon", "stop", "--force"])
    print("Pi TUI: ordinary turn, cold spawn, resumed send/reply, idle/busy inbox, reload and bare wait passed")


PACKAGES = {"nico": "pi-subagents@0.89.2", "tintin": "@tintinweb/pi-subagents@0.26.2"}


def subagent_receipts(f):
    f.run([f.pi, "install", str(ROOT)])
    session = str(uuid.uuid4())
    child_output, child_inbox, parent_output, inbox = [], [], [], []
    release = threading.Event()
    child_task, parent_task = "from-delegate", "from-tui"

    def spawn(name, prompt):
        return f"axiopera spawn pi --model {MODEL} --tier full --name {name} {prompt}"

    def respond(request):
        messages = request["messages"]
        last = messages[-1]
        user = next(str(m["content"]) for m in reversed(messages) if m["role"] == "user")
        if "DELEGATE-LEAF" in user:
            return "private child result"
        if "TUI-LEAF" in user:
            release.wait(40)
            return "public parent result"
        if "DELEGATE-SHELL" in user:
            if last["role"] == "tool":
                if not child_output:
                    child_output.append(str(last["content"]))
                    return "bash", {"command": f"axiopera wait {child_task} && axiopera inbox {child_task}"}
                child_inbox.append(str(last["content"]))
                return "delegate collected its result"
            return "bash", {"command": spawn(child_task, "DELEGATE-LEAF")}
        if NOTICE in user:
            if last["role"] == "tool":
                inbox.append(str(last["content"]))
                return "parent received own result"
            return "bash", {"command": "axiopera inbox"}
        if "PARENT-DISPATCH" in user:
            if last["role"] == "tool":
                parent_output.append(str(last["content"]))
                return "parent dispatched"
            return "bash", {"command": spawn(parent_task, "TUI-LEAF")}
        previous = [call["function"]["name"] for m in messages for call in m.get("tool_calls", [])]
        if "subagent" in previous:
            return "parent finished delegation"
        tools = {tool["function"]["name"] for tool in request["tools"]}
        if "subagent" not in tools:
            return "subagents_enable", {}
        return "subagent", {"agent": "delegate", "task": "DELEGATE-SHELL", "async": False}

    with f.model(respond):
        tui = Pty([str(f.pi), "--session-id", session, "--model", MODEL, "--approve"], f.cwd, f.env)
        try:
            wait_until(lambda: "pi-subagents" in tui.screen, "community plugin did not load")
            # Pi can display the editor while extension startup is still in progress.
            # An Enter rejected during startup has no pending prompt.
            for attempt in range(4):
                offset = len(tui.screen)
                tui.type("PARENT-DELEGATE")
                time.sleep(.5)
                if "Startup is still in progress" not in tui.screen[offset:]:
                    break
                time.sleep(1)
            wait_until(lambda: "parent finished delegation" in tui.screen, "native delegate did not return")
            assert child_output and "in the foreground" in child_output[-1] and "Pi will notify" not in child_output[-1]
            assert child_inbox and "private child result" in child_inbox[-1]
            tui.type("PARENT-DISPATCH")
            wait_until(lambda: "parent dispatched" in tui.screen, "parent did not dispatch after its delegate")
            assert parent_output and "Pi will notify this session" in parent_output[-1] and "axiopera wait" not in parent_output[-1]
            release.set()
            wait_until(lambda: any("public parent result" in row for row in inbox), "original TUI did not take its result")
            assert all("private child result" not in row for row in inbox)
            rows = json.loads(f.run([AXIOPERA, "--json", "status", "--all"]).splitlines()[0])["data"]
            owners = {row["name"]: row["parent"] for row in rows}
            assert owners[parent_task] == "pi:" + session
            assert owners[child_task].startswith("pi:") and owners[child_task] != owners[parent_task]
        finally:
            release.set()
            tui.close()
            f.run([AXIOPERA, "daemon", "stop", "--force"])
    print("pi-subagents TUI: native delegate collected its result with foreground wait; parent received its own notification")


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
    if name == "nico":
        subagent_receipts(f)


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
