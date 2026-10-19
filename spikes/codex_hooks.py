"""Does the Codex plugin's hook tell a busy host about its sub-tasks?

Runs the host on a private `codex app-server`, as the Codex app does, with the plugin's MCP
server and hook given as thread config, so nothing is installed or trusted for real:
  tool  the host keeps running commands; a PostToolUse reminder listing the news should make it
        run `axiopera inbox` within the same turn, with no turn after it
  end   the host writes without tools; the doorbell should start one turn after it that runs
        `axiopera inbox`
Prints the hook's reminders, how each case went and the daemon's log lines about the host.

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
                if run.get("entries"):
                    log(m["method"], run.get("eventName"), run.get("status"), str(run["entries"])[:400])
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
        wait_idle(host, tid, 1, 300)
        time.sleep(10)
        t = turns(host, tid)
        takes = outputs(t[0], "axiopera inbox")
        got = any("ALPHA" in (i.get("aggregatedOutput") or "") for i in takes)
        log("tool:", "collected in the same turn" if got else f"NOT collected ({len(takes)} inbox call(s))",
            "|", f"{len(t) - 1} turn(s) after it")

        # Busy without tools: the doorbell starts the next turn.
        n = len(t) + 1
        host.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text":
                  f"Run this command: {spawn.format('bravo', 'BRAVO')} Then, without running any other "
                  "command, write a 400-word story about a lighthouse, and end your turn."}]})
        wait_idle(host, tid, n, 300)
        try:
            t = wait_idle(host, tid, n + 1, 120)
        except TimeoutError:
            log("end: no turn after it")
            t = turns(host, tid)
        else:
            rung = [c.get("text", "") for i in t[n].get("items", []) if i.get("type") == "userMessage"
                    for c in i.get("content", [])]
            log("doorbell:", " / ".join(rung))
            takes = outputs(t[n], "axiopera inbox")
            got = any("BRAVO" in (i.get("aggregatedOutput") or "") for i in takes)
            log("end:", "collected in the turn after" if got else f"NOT collected ({len(takes)} inbox call(s))")

        time.sleep(25)
        extra = len(turns(host, tid)) - len(t)
        log("after:", "no further turn" if extra == 0 else f"{extra} extra turn(s)")

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
