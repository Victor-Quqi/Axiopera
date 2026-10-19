"""Does a Codex host outside the shared daemon, like the Codex app, get a doorbell?

Runs the host on a private `codex app-server`, as the Codex app and IDE extension do, and lets
it spawn Claude sub-agents through axiopera:
  idle  the host ends its turn; a reminder listing the news should start a new turn that runs
        `axiopera inbox`
  busy  the host takes the result itself; the queued reminder should be withdrawn, so no turn
        follows
Prints the spawn receipt, the reminder, how each case went and the daemon's log lines about the
host.

usage: python codex_doorbell.py
"""

import os
import subprocess
import tempfile
import time
from pathlib import Path

from probe import AXIOPERA, CODEX, Rpc, clean_env, forget_sessions, log, remove_tree, workdir

DOORBELL = "From your sub-tasks (sent by axiopera, not typed by the user):"


def turns(rpc, tid):
    return rpc.call("thread/read", {"threadId": tid, "includeTurns": True})["thread"]["turns"]


def wait_idle(rpc, tid, at_least, secs):
    deadline = time.monotonic() + secs
    while time.monotonic() < deadline:
        time.sleep(2)
        t = turns(rpc, tid)
        if len(t) >= at_least and t[-1].get("status") != "inProgress":
            return t
    raise TimeoutError(f"the host did not finish turn {at_least} in {secs}s")


def texts(turn, kind, field):
    out = []
    for item in turn.get("items", []):
        if item.get("type") != kind:
            continue
        value = item.get(field)
        if isinstance(value, list):
            out += [c.get("text", "") for c in value]
        elif value:
            out.append(value)
    return out


def ask(rpc, tid, text):
    rpc.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text": text}]})


def main():
    work = workdir("codex-doorbell")
    home = Path(tempfile.mkdtemp(prefix="axiopera-home-"))
    env = clean_env(AXIOPERA_HOME=str(home), PATH=f"{AXIOPERA.parent}{os.pathsep}{os.environ['PATH']}")
    host = tid = None
    spawn = "axiopera spawn claude --tier read --model haiku --name {0} \"Reply with exactly {1}\""
    try:
        host = Rpc([CODEX, "app-server"], work, env=env)
        host.call("initialize", {"clientInfo": {"name": "axiopera-spike", "version": "0"}})
        host.notify("initialized", {})
        tid = host.call("thread/start", {"cwd": str(work), "sandbox": "danger-full-access",
                                         "approvalPolicy": "never"})["thread"]["id"]
        log("host thread", tid)

        # Idle host: it ends its turn, then a reminder should start the next one.
        ask(host, tid, f"Run this command and show me what it printed, then end your turn: {spawn.format('alpha', 'ALPHA')}")
        t = wait_idle(host, tid, 1, 180)
        receipt = [i.get("aggregatedOutput") or "" for i in t[0].get("items", [])
                   if i.get("type") == "commandExecution" and "spawn" in str(i.get("command"))]
        log("receipt:", " / ".join(receipt)[:300] or "none")
        t = wait_idle(host, tid, 2, 240)
        reminder = [x for x in texts(t[1], "userMessage", "content") if DOORBELL in x]
        log("reminder:", reminder[0] if reminder else "none")
        rung = bool(reminder)
        got = any("ALPHA" in o for o in texts(t[1], "commandExecution", "aggregatedOutput"))
        log("idle:", "rung" if rung else "NOT rung", "|", "result collected" if got else "result NOT collected")

        # Busy host: the reminder is queued while it sleeps, then its own inbox withdraws it.
        ask(host, tid, "Run these commands one at a time, then end your turn. "
                       f"1. {spawn.format('bravo', 'BRAVO')} "
                       "2. python -c \"import time; time.sleep(45)\" "
                       "3. axiopera inbox")
        t = wait_idle(host, tid, 3, 300)
        got = any("BRAVO" in o for o in texts(t[2], "commandExecution", "aggregatedOutput"))
        time.sleep(25)
        extra = len(turns(host, tid)) - 3
        log("busy:", "result collected" if got else "result NOT collected", "|",
            "no reminder turn" if extra == 0 else f"{extra} extra turn(s)")

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
