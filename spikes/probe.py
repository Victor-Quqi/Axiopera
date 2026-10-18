"""Shared helpers for the protocol probes.

Probes talk to the real agent CLIs, so they cost tokens. They never print
environment variables or config files; transcripts are summarized, not dumped.
"""

import json
import os
import platform
import queue
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
from pathlib import Path

def agent_exe(name):
    configured = os.environ.get(f"AXIOPERA_PROBE_{name.upper()}")
    if configured:
        return str(Path(configured).expanduser())
    if name == "grok":
        home = Path(os.environ.get("GROK_HOME") or Path.home() / ".grok")
        native = home / "bin" / ("grok.exe" if os.name == "nt" else "grok")
        if native.is_file():
            return str(native)
    found = shutil.which(name)
    if os.name == "nt" and found and Path(found).suffix.lower() in (".cmd", ".ps1"):
        # Own the native process so cancellation can kill its complete process tree.
        modules = Path(found).parent / "node_modules"
        arm = platform.machine().lower() in ("arm64", "aarch64")
        arch, triple = ("arm64", "aarch64") if arm else ("x64", "x86_64")
        native = {
            "claude": modules / "@anthropic-ai/claude-code/bin/claude.exe",
            "codex": modules / f"@openai/codex/node_modules/@openai/codex-win32-{arch}/vendor/{triple}-pc-windows-msvc/bin/codex.exe",
        }.get(name)
        if native and native.is_file():
            return str(native)
    if found:
        return found
    return name


CLAUDE, CODEX, GROK = (agent_exe(c) for c in ("claude", "codex", "grok"))
AXIOPERA = Path(__file__).resolve().parent.parent / "target/debug" / ("axiopera.exe" if os.name == "nt" else "axiopera")
ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b.)?|\x1b[()][0-9A-Za-z]|\x1b[=>]")

# Host session identity leaks into every descendant; a sub-agent must not inherit ours.
IDENTITY_VARS = {
    "CLAUDECODE", "CLAUDE_CODE_SESSION_ID", "CLAUDE_PID", "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_THREAD_ID", "CODEX_SESSION_ID", "CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "GROK_SESSION_ID", "GROK_AGENT",
    "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN",
}

T0 = time.monotonic()
sys.stdout.reconfigure(encoding="utf-8", errors="replace")


def log(*args):
    print(f"[{time.monotonic() - T0:7.1f}s]", *args, flush=True)


def clean_env(**extra):
    env = {k: v for k, v in os.environ.items() if k.upper() not in IDENTITY_VARS}
    env.update(extra)
    return env


def workdir(name):
    d = Path(tempfile.gettempdir()) / "axiopera-spike" / f"{name}-{os.getpid()}"
    d.mkdir(parents=True, exist_ok=True)
    return d


def remove_tree(path):
    """Delete a tree, including read-only files such as git objects on Windows."""
    def writable(func, p, _exc):
        os.chmod(p, stat.S_IWRITE)
        func(p)

    if Path(path).exists():
        shutil.rmtree(path, onexc=writable)


def forget_sessions(cwd):
    """Delete the Claude and Grok sessions the probes created under cwd."""
    cwd = str(cwd)
    shutil.rmtree(Path.home() / ".claude/projects" / re.sub(r"[^A-Za-z0-9]", "-", cwd), ignore_errors=True)
    grok_dir = Path.home() / ".grok/sessions" / urllib.parse.quote(cwd, safe="")
    if grok_dir.is_dir():
        for s in grok_dir.iterdir():
            if s.is_dir():
                subprocess.run([GROK, "sessions", "delete", s.name], capture_output=True, env=clean_env())
        shutil.rmtree(grok_dir, ignore_errors=True)


def kill_tree(pid):
    if os.name == "nt":
        subprocess.run(["taskkill", "/T", "/F", "/PID", str(pid)], capture_output=True)
    else:
        subprocess.run(["pkill", "-KILL", "-P", str(pid)], capture_output=True)
        subprocess.run(["kill", "-KILL", str(pid)], capture_output=True)


class Pty:
    """An interactive CLI in a pseudo-terminal; `screen` collects its output without escapes.
    Answers cursor-position queries, which some TUIs wait for. Windows needs pywinpty."""

    def __init__(self, argv, cwd, env):
        self.screen = ""
        if os.name == "nt":
            from winpty import PtyProcess

            p = PtyProcess.spawn(argv, cwd=str(cwd), env=env, dimensions=(50, 200))
            self.pid, self.read, self.write = p.pid, lambda: p.read(65536), p.write
        else:
            import fcntl
            import pty
            import struct
            import termios

            pid, fd = pty.fork()
            if pid == 0:
                os.chdir(cwd)
                os.execvpe(argv[0], argv, dict(env, TERM="xterm-256color"))
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 200, 0, 0))
            self.pid = pid
            self.read = lambda: os.read(fd, 65536).decode("utf-8", "replace")
            self.write = lambda s: os.write(fd, s.encode())
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            try:
                data = self.read()
            except (EOFError, OSError):
                return
            if not data:
                return
            if "\x1b[6n" in data:
                self.write("\x1b[1;1R")
            self.screen += ANSI.sub("", data)

    def type(self, text):
        """Type a line and submit it."""
        self.write(text)
        time.sleep(1)
        self.write("\r")

    def close(self):
        try:
            self.write("\x03")
            time.sleep(1)
            self.write("\x03")
            time.sleep(2)
        except (EOFError, OSError):
            pass
        kill_tree(self.pid)


class Jsonl:
    """A child process speaking newline-delimited JSON on stdio."""

    def __init__(self, argv, cwd, env=None):
        self.proc = subprocess.Popen(
            argv, cwd=cwd, env=env or clean_env(),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
        )
        self.inbox = queue.Queue()
        self.stderr_tail = []
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stdout(self):
        for raw in self.proc.stdout:
            raw = raw.strip()
            if not raw:
                continue
            try:
                msg = json.loads(raw)
            except ValueError:
                msg = {"_raw": raw[:200].decode(errors="replace")}
            self.dispatch(msg)
        self.inbox.put(None)

    def _read_stderr(self):
        for raw in self.proc.stderr:
            self.stderr_tail = (self.stderr_tail + [raw.decode(errors="replace").rstrip()])[-20:]

    def dispatch(self, msg):
        self.inbox.put(msg)

    def send(self, obj):
        self.proc.stdin.write((json.dumps(obj) + "\n").encode())
        self.proc.stdin.flush()

    def next(self, timeout):
        try:
            return self.inbox.get(timeout=timeout)
        except queue.Empty:
            return ...

    def close(self):
        if self.proc.poll() is None:
            kill_tree(self.proc.pid)
        self.proc.wait(10)


class Rpc(Jsonl):
    """JSON-RPC 2.0 over stdio. Server requests go to on_request; notifications to inbox."""

    def __init__(self, argv, cwd, env=None, on_request=None):
        self._next_id = 0
        self._pending = {}
        self._lock = threading.Lock()
        self.on_request = on_request or (lambda rpc, msg: None)
        super().__init__(argv, cwd, env)

    def dispatch(self, msg):
        if "id" in msg and "method" not in msg:
            slot = self._pending.pop(msg["id"], None)
            if slot:
                slot.put(msg)
            return
        if "id" in msg:
            threading.Thread(target=self._serve, args=(msg,), daemon=True).start()
            return
        self.inbox.put(msg)

    def _serve(self, msg):
        result = self.on_request(self, msg)
        if result is not None:
            self.respond(msg["id"], result)

    def respond(self, id, result):
        self.send({"jsonrpc": "2.0", "id": id, "result": result})

    def call_async(self, method, params):
        with self._lock:
            self._next_id += 1
            id = self._next_id
        slot = queue.Queue()
        self._pending[id] = slot
        self.send({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        return slot

    def call(self, method, params, timeout=60):
        reply = self.call_async(method, params).get(timeout=timeout)
        if "error" in reply:
            raise RpcError(method, reply["error"])
        return reply["result"]

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})


class RpcError(Exception):
    def __init__(self, method, error):
        super().__init__(f"{method}: {error.get('code')} {error.get('message')}")
        self.error = error


def codex_app_server(cwd, on_request=None, config=()):
    argv = [CODEX, "app-server"]
    for kv in config:
        argv += ["-c", kv]
    rpc = Rpc(argv, cwd, on_request=on_request)
    rpc.call("initialize", {"clientInfo": {"name": "axiopera-spike", "version": "0"},
                            "capabilities": {"experimentalApi": True}})
    rpc.notify("initialized", {})
    return rpc


def grok_acp(cwd, on_request=None, env=None):
    argv = [GROK, "agent", "--no-leader", "stdio"]
    rpc = Rpc(argv, cwd, env=env, on_request=on_request)
    rpc.call("initialize", {"protocolVersion": 1, "clientCapabilities": {
        "fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False}})
    return rpc


def claude_stream(cwd, *args):
    argv = [CLAUDE, "-p", "--input-format", "stream-json", "--output-format", "stream-json",
            "--verbose", *args]
    return Jsonl(argv, cwd)


def claude_user(text, **extra):
    return {"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}, **extra}


def text_of(obj):
    """Concatenate every 'text' string found anywhere in a message."""
    out = []

    def walk(o):
        if isinstance(o, dict):
            for k, v in o.items():
                if k == "text" and isinstance(v, str):
                    out.append(v)
                else:
                    walk(v)
        elif isinstance(o, list):
            for v in o:
                walk(v)

    walk(obj)
    return "\n".join(out)
