"""Does Grok's monitor tool bring `axiopera watch` lines to a busy and an idle host?

Runs the host on `grok agent stdio` in yolo mode and lets it spawn Claude sub-agents:
  busy  the host starts the watch command `axiopera guide` gives it in a persistent monitor,
        spawns a sub-agent and keeps running commands; the line should arrive between two of
        them, and the host should run `axiopera inbox` within the same prompt
  idle  the host spawns another sub-agent and ends its prompt; the line should start a new turn
        that runs `axiopera inbox`
Prints the commands the host runs, the monitor's lines, turn boundaries and how each case went.

usage: python grok_monitor.py
"""

import json
import os
import subprocess
import tempfile
import time
from pathlib import Path

from probe import AXIOPERA, clean_env, forget_sessions, grok_acp, log, remove_tree, workdir

SPAWN = "axiopera spawn claude --tier read --model haiku --name {0} \"Reply with exactly {1}\""
SHOWN = ("monitor_event", "task_completed", "turn_completed")


class Host:
    """A Grok session, with the commands it ran and what they printed."""

    def __init__(self, rpc, sid):
        self.rpc, self.sid = rpc, sid
        self.calls = {}

    def pump(self, secs, done=lambda: False):
        """Handle updates for up to `secs`, or until `done` and the updates that came before."""
        deadline = time.monotonic() + secs
        while time.monotonic() < deadline:
            finished = done()
            m = self.rpc.next(0 if finished else 0.5)
            if m in (None, ...):
                if finished:
                    break
                continue
            u = m.get("params", {}).get("update", {})
            kind = u.get("sessionUpdate") or m.get("method")
            if kind == "tool_call":
                command = (u.get("rawInput") or {}).get("command") or u.get("title")
                self.calls[u["toolCallId"]] = [str(command), ""]
                log("tool:", str(command)[:200])
            elif kind == "tool_call_update" and u.get("toolCallId") in self.calls:
                out = u.get("content") or u.get("rawOutput") or ""
                self.calls[u["toolCallId"]][1] += json.dumps(out, ensure_ascii=False)
            elif kind == "monitor_event":
                log("monitor:", u.get("event_text"))
            elif kind in SHOWN:
                log(kind)

    def prompt(self, text, secs):
        reply = self.rpc.call_async("session/prompt", {"sessionId": self.sid,
                                                       "prompt": [{"type": "text", "text": text}]})
        self.pump(secs, lambda: not reply.empty())
        if reply.empty():
            raise TimeoutError(f"the prompt did not end in {secs}s")
        log("prompt ended:", reply.get().get("result", {}).get("stopReason"))

    def took(self, word, before=()):
        """The inbox calls that printed `word`, leaving out the calls in `before`."""
        return [c for k, c in self.calls.items()
                if k not in before and "axiopera inbox" in c[0] and word in c[1]]


def main():
    work = workdir("grok-monitor")
    home = Path(tempfile.mkdtemp(prefix="axiopera-home-"))
    env = clean_env(AXIOPERA_HOME=str(home), PATH=f"{AXIOPERA.parent}{os.pathsep}{os.environ['PATH']}")
    rpc = None
    try:
        rpc = grok_acp(work, env=env)
        sid = rpc.call("session/new", {"cwd": str(work), "mcpServers": [],
                                       "_meta": {"yoloMode": True}})["sessionId"]
        host = Host(rpc, sid)
        log("host session", sid)

        # Busy host: the line arrives after one of its commands.
        host.prompt("Do these one at a time. 1. Run `axiopera guide`, then start the watch command "
                    "it gives for Grok with your monitor tool, persistent; for each line it prints, "
                    "run `axiopera inbox`. "
                    f"2. Run {SPAWN.format('alpha', 'ALPHA')} "
                    "3. Run python -c \"import time; time.sleep(15)\" up to four times, one call at a "
                    "time, and follow any instructions that arrive between calls. Then end your turn.", 300)
        log("busy:", "collected in the same prompt" if host.took("ALPHA") else "NOT collected")
        host.pump(15)

        # Idle host: the line starts a turn.
        host.prompt(f"Run {SPAWN.format('bravo', 'BRAVO')} and end your turn.", 120)
        ended = set(host.calls)
        if host.took("BRAVO"):
            log("idle: not tested, the host collected the result before its prompt ended")
        else:
            host.pump(120, lambda: host.took("BRAVO", ended))
            log("idle:", "collected in a turn of its own" if host.took("BRAVO", ended) else "NOT collected")
        host.pump(20)
    finally:
        if rpc:
            rpc.close()
        subprocess.run([str(AXIOPERA), "daemon", "stop", "--force"], env=env, capture_output=True)
        time.sleep(4)
        forget_sessions(work)
        remove_tree(home)
        remove_tree(work)


if __name__ == "__main__":
    main()
