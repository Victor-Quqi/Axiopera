"""Does axiopera push sub-task results into an interactive Codex host session?

Runs the Codex TUI in a pseudo-terminal, so it attaches to the user's shared Codex daemon, and
spawns Claude sub-agents as that session:
  idle  the host has ended its turn; the result should arrive as a new turn
  busy  the host is mid-turn; the result should be steered into that turn
  approval  a turn started from another connection, closed right after (as axiopera does),
            needs approval for a command; the request should still reach the TUI
Prints how each result arrived and the daemon's log lines about it. Watches the host through
`codex app-server proxy`, the same way axiopera reaches it.

usage: python codex_host_push.py   (Windows needs pywinpty)
"""

import base64
import json
import os
import subprocess
import tempfile
import threading
import time
from pathlib import Path

from probe import AXIOPERA, CODEX, Pty, clean_env, forget_sessions, kill_tree, log, remove_tree, workdir


class Daemon:
    """JSON-RPC over WebSocket over the bytes `codex app-server proxy` relays to the Codex daemon."""

    def __init__(self):
        self.p = subprocess.Popen([CODEX, "app-server", "proxy"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, env=clean_env())
        self.buf, self.closed, self.cv, self.n = b"", False, threading.Condition(), 0
        threading.Thread(target=self.pump, daemon=True).start()
        key = base64.b64encode(os.urandom(16)).decode()
        self.write(f"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                   f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode())
        self.need(lambda b: b"\r\n\r\n" in b)
        head, self.buf = self.buf.split(b"\r\n\r\n", 1)
        if not head.startswith(b"HTTP/1.1 101"):
            raise RuntimeError(f"no WebSocket upgrade: {head[:80]!r}")
        self.call("initialize", {"clientInfo": {"name": "axiopera-spike", "version": "0"}})
        self.send({"method": "initialized", "params": {}})

    def pump(self):
        while True:
            chunk = self.p.stdout.read1(65536)
            with self.cv:
                self.closed = not chunk
                self.buf += chunk
                self.cv.notify_all()
                if not chunk:
                    return

    def need(self, ok):
        with self.cv:
            while not ok(self.buf):
                if self.closed:
                    raise EOFError("the proxy exited; is the Codex daemon running?")
                self.cv.wait(1)

    def write(self, data):
        self.p.stdin.write(data)
        self.p.stdin.flush()

    def frame(self, opcode, data):
        n = len(data)
        head = bytearray([0x80 | opcode])
        if n < 126:
            head.append(0x80 | n)
        elif n < 65536:
            head += bytes([0x80 | 126]) + n.to_bytes(2, "big")
        else:
            head += bytes([0x80 | 127]) + n.to_bytes(8, "big")
        mask = os.urandom(4)
        self.write(bytes(head) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def send(self, msg):
        self.frame(0x1, json.dumps(msg).encode())

    def recv(self):
        msg = b""
        while True:
            self.need(lambda b: len(b) >= 2)
            fin, op, n = self.buf[0] & 0x80, self.buf[0] & 0x0F, self.buf[1] & 0x7F
            off = {126: 4, 127: 10}.get(n, 2)
            self.need(lambda b: len(b) >= off)
            n = int.from_bytes(self.buf[2:off], "big") if off > 2 else n
            self.need(lambda b: len(b) >= off + n)
            with self.cv:
                payload, self.buf = self.buf[off:off + n], self.buf[off + n:]
            if op == 0x9:
                self.frame(0xA, payload)
            elif op == 0x8:
                raise EOFError("the Codex daemon closed the connection")
            elif op != 0xA:
                msg += payload
                if fin:
                    return json.loads(msg)

    def call(self, method, params):
        self.n += 1
        self.send({"id": self.n, "method": method, "params": params})
        while True:
            m = self.recv()
            if m.get("id") == self.n and "method" not in m:
                if "error" in m:
                    raise RuntimeError(f"{method}: {m['error']}")
                return m["result"]

    def turns(self, tid):
        return self.call("thread/read", {"threadId": tid, "includeTurns": True})["thread"]["turns"]

    def wait_idle(self, tid, at_least, secs):
        deadline = time.monotonic() + secs
        while time.monotonic() < deadline:
            time.sleep(2)
            turns = self.turns(tid)
            if len(turns) >= at_least and turns[-1].get("status") != "inProgress":
                return turns
        raise TimeoutError(f"the host did not finish turn {at_least} in {secs}s")

    def close(self):
        self.p.stdin.close()
        kill_tree(self.p.pid)


def user_texts(turn):
    items = turn.get("items", [])
    return [c.get("text", "") for i in items if i.get("type") == "userMessage" for c in i.get("content", [])]


def main():
    work = workdir("codex-host-push")
    home = Path(tempfile.mkdtemp(prefix="axiopera-home-"))
    tui = daemon = tid = None
    rollouts = []

    def axiopera(*args):
        env = clean_env(AXIOPERA_HOME=str(home), **({"CODEX_THREAD_ID": tid} if tid else {}))
        p = subprocess.run([str(AXIOPERA), *args], cwd=work, env=env, capture_output=True, text=True,
                           encoding="utf-8", errors="replace")
        return (p.stdout + p.stderr).strip()

    try:
        # No -c overrides: with them the TUI runs its own app-server instead of the shared one.
        tui = Pty([CODEX, "Reply with just: ready. Do not run any commands."], work, clean_env())
        trusted = False
        deadline = time.monotonic() + 90
        while not tid and time.monotonic() < deadline:
            time.sleep(1)
            screen = tui.screen.lower()
            if not trusted and "trust" in screen and ("folder" in screen or "directory" in screen):
                tui.write("\r")
                trusted = True
            if daemon is None:
                try:
                    daemon = Daemon()
                except (EOFError, RuntimeError):
                    continue
            found = daemon.call("thread/list", {"cwd": str(work), "limit": 5})["data"]
            if found:
                tid = found[0]["id"]
        if not tid:
            log("the TUI started no thread; screen tail:\n" + tui.screen[-1500:])
            return
        daemon.wait_idle(tid, 1, 120)
        log("host thread", tid)

        # Idle host: the result should start a new turn.
        log("spawn alpha:", axiopera("spawn", "claude", "--tier", "read", "--model", "haiku", "--name", "alpha",
                                     "Reply with exactly ALPHA").splitlines()[-1])
        turns = daemon.wait_idle(tid, 2, 240)
        pushed = [t for t in user_texts(turns[-1]) if t.startswith("[axiopera:") and "ALPHA" in t]
        log("idle:", "new turn" if len(turns) == 2 and pushed else "NOT pushed", "|", " / ".join(pushed)[:200])

        # Busy host: the result should be steered into the running turn.
        daemon.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text":
                    "Write the numbers from 1 to 400 in English words, one per line. Do not run any commands."}]})
        time.sleep(3)
        log("spawn bravo:", axiopera("spawn", "claude", "--tier", "read", "--model", "haiku", "--name", "bravo",
                                     "Reply with exactly BRAVO").splitlines()[-1])
        turns = daemon.wait_idle(tid, 3, 300)
        steered = [t for t in user_texts(turns[-1]) if "BRAVO" in t]
        log("busy:", "steered" if len(turns) == 3 and steered else f"NOT steered ({len(turns)} turns)")

        # A turn started from a connection that is gone needs approval.
        other = Daemon()
        other.call("turn/start", {"threadId": tid, "approvalPolicy": "on-request", "approvalsReviewer": "user",
                                  "sandboxPolicy": {"type": "readOnly"},
                                  "input": [{"type": "text", "text":
                   "Run exactly this shell command once: `mkdir approved`. Do nothing else."}]})
        other.close()
        mark = len(tui.screen)
        asked = False
        deadline = time.monotonic() + 120
        while not asked and time.monotonic() < deadline:
            time.sleep(1)
            asked = "proceed" in tui.screen[mark:].lower()
        if asked:
            tui.write("y")
        daemon.wait_idle(tid, 4, 120)
        log("approval:", "asked in the TUI" if asked else "NOT asked in the TUI",
            "| command ran" if (work / "approved").exists() else "| command did not run")
        if not asked:
            log("screen tail:\n" + tui.screen[-1500:])

        lines = (home / "daemon" / "daemon.log").read_text(encoding="utf-8").splitlines()
        log("daemon log:\n" + "\n".join(l for l in lines if "codex:" in l))
    finally:
        if daemon:
            # The host's, and any approval reviewer's.
            try:
                rollouts = [t.get("path") for t in daemon.call("thread/list", {"cwd": str(work), "limit": 20})["data"]]
            except (EOFError, RuntimeError):
                pass
            daemon.close()
        if tui:
            tui.close()
        print(axiopera("daemon", "stop", "--force"))
        forget_sessions(work)
        remove_tree(home)
        for rollout in filter(None, rollouts):
            Path(rollout).unlink(missing_ok=True)
        # The shared daemon keeps the thread loaded, its MCP servers in `work`, for a minute before
        # daemon 0.163 and for 30 minutes since.
        for _ in range(30):
            try:
                remove_tree(work)
                break
            except PermissionError:
                time.sleep(5)
        else:
            log(f"{work} is still in use; remove it once the daemon unloads the thread")


if __name__ == "__main__":
    main()
