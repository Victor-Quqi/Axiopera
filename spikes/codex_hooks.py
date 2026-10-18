"""Do the Codex plugin's hooks tell a busy host about its sub-tasks?

Runs the host on a private `codex app-server`, as the Codex app does, with the plugin's MCP
server and hooks given as thread config, so nothing is installed or trusted for real:
  tool  the host keeps running commands; a PostToolUse reminder should make it run `wait`
        within the same turn
  stop  the host writes without tools; the Stop hook should continue its turn so it runs `wait`
Each case should take one turn, with no reminder turn after it. Prints the daemon's log lines
about the host.

usage: python codex_hooks.py
"""

import json
import os
import subprocess
import tempfile
import time
from pathlib import Path

from probe import AXIOPERA, CODEX, Rpc, clean_env, forget_sessions, log, remove_tree, workdir

PLUGIN = Path(__file__).resolve().parent.parent / "plugins/axiopera"


def turns(rpc, tid):
    return rpc.call("thread/read", {"threadId": tid, "includeTurns": True})["thread"]["turns"]


def wait_idle(rpc, tid, at_least, secs):
    deadline = time.monotonic() + secs
    while time.monotonic() < deadline:
        time.sleep(2)
        while (m := rpc.next(0)) not in (None, ...):
            if "hook" in str(m.get("method", "")).lower():
                run = m.get("params", {}).get("run", {})
                log(m["method"], run.get("eventName"), run.get("status"), str(run.get("entries") or "")[:200])
        t = turns(rpc, tid)
        if len(t) >= at_least and t[-1].get("status") != "inProgress":
            return t
    raise TimeoutError(f"the host did not finish turn {at_least} in {secs}s")


def outputs(turn, needle):
    return [i for i in turn.get("items", [])
            if i.get("type") == "commandExecution" and needle in str(i.get("command"))]


def main():
    work = workdir("codex-hooks")
    home = Path(tempfile.mkdtemp(prefix="axiopera-home-"))
    env = clean_env(AXIOPERA_HOME=str(home), PATH=f"{AXIOPERA.parent}{os.pathsep}{os.environ['PATH']}")
    servers = json.loads((PLUGIN / "codex/mcp.json").read_text(encoding="utf-8"))["mcpServers"]
    servers["axiopera-hooks"]["command"] = str(AXIOPERA)
    hooks = json.loads((PLUGIN / "codex/hooks.json").read_text(encoding="utf-8"))["hooks"]
    spawn = "axiopera spawn claude --tier read --model haiku --name {0} \"Reply with exactly {1}\""
    host = tid = None
    try:
        host = Rpc([CODEX, "app-server"], work, env=env)
        host.call("initialize", {"clientInfo": {"name": "axiopera-spike", "version": "0"}})
        host.notify("initialized", {})
        tid = host.call("thread/start", {
            "cwd": str(work), "sandbox": "danger-full-access", "approvalPolicy": "never",
            "config": {"bypass_hook_trust": True, "mcp_servers": servers, "hooks": hooks},
        })["thread"]["id"]
        log("host thread", tid)

        # Busy with tools: the reminder comes with a tool result.
        host.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text":
                  f"Run these commands one at a time. 1. {spawn.format('alpha', 'ALPHA')} "
                  "2. Then run python -c \"import time; time.sleep(15)\" up to four times, one call at a time, "
                  "and follow any instructions that arrive between calls. Then end your turn."}]})
        t = wait_idle(host, tid, 1, 300)
        waits = outputs(t[0], "axiopera wait")
        got = any("ALPHA" in (i.get("aggregatedOutput") or "") for i in waits)
        log("tool:", "collected in the same turn" if got else f"NOT collected ({len(waits)} wait call(s))")

        # Busy without tools: the Stop hook continues the turn.
        host.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text":
                  f"Run this command: {spawn.format('bravo', 'BRAVO')} Then, without running any other "
                  "command, write a 400-word story about a lighthouse, and end your turn."}]})
        t = wait_idle(host, tid, 2, 300)
        waits = outputs(t[1], "axiopera wait")
        got = any("BRAVO" in (i.get("aggregatedOutput") or "") for i in waits)
        log("stop:", "collected in the same turn" if got else f"NOT collected ({len(waits)} wait call(s))")

        time.sleep(25)
        extra = len(turns(host, tid)) - 2
        log("after:", "no reminder turn" if extra == 0 else f"{extra} extra turn(s)")

        lines = (home / "daemon" / "daemon.log").read_text(encoding="utf-8").splitlines()
        log("daemon log:\n" + "\n".join(l for l in lines if "codex:" in l))
    finally:
        if host:
            if tid:
                try:
                    host.call("thread/delete", {"threadId": tid})
                except Exception as e:  # noqa: BLE001 - report and keep cleaning up
                    log("thread/delete:", e)
            host.close()
        subprocess.run([str(AXIOPERA), "daemon", "stop", "--force"], env=env, capture_output=True)
        forget_sessions(work)
        remove_tree(home)
        remove_tree(work)


if __name__ == "__main__":
    main()
