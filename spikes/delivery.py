"""Does news from real sub-agents reach the host as designed?

Runs axiopera as a host session with no push, which takes news by `wait` and `inbox`:
  approval  a Codex sub-agent at tier full asks approval for a forced delete after a Claude
            sub-agent has finished; `inbox` should give the request ahead of the earlier result,
            and the host may approve only after that
  long      a Claude sub-agent asks a question and sends a sibling a note, each longer than one
            reply shows whole; `inbox` should give their ends and a file with all of it
  self      Claude and Grok sub-agents end a turn with a command running in the background; when
            it completes, the turn they start by themselves should show in `status` and `log`
Prints what the host got and how each case went.

usage: python delivery.py [approval|long|self]...   (default: all)
"""

import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from probe import AXIOPERA, clean_env, codex_app_server, forget_sessions, log, remove_tree, workdir

BACKGROUND = ('Start this shell command in the background (do not wait for it): '
              'python -c "import time; time.sleep(20); print(7)" '
              "then end your turn right away with one short sentence. When it finishes, report its output.")


class Host:
    def __init__(self, env, cwd):
        self.env, self.cwd = env, cwd

    def run(self, *args, timeout=600):
        p = subprocess.run([str(AXIOPERA), *args], env=self.env, cwd=self.cwd, capture_output=True,
                           text=True, encoding="utf-8", timeout=timeout)
        return p.returncode, (p.stdout + p.stderr).strip()

    def wait(self, *tasks, secs=300):
        return self.run("wait", "--timeout", f"{secs}s", *tasks, timeout=secs + 30)[1]

    def inbox_all(self):
        """Everything `inbox` has, reply by reply."""
        replies = []
        while True:
            code, out = self.run("inbox")
            if code != 0:
                return replies
            replies.append(out)
            if "run `axiopera inbox` again" not in out:
                return replies

    def wait_until(self, task, pattern, secs):
        deadline = time.monotonic() + secs
        while time.monotonic() < deadline:
            out = self.run("log", task)[1]
            if re.search(pattern, out):
                return out
            time.sleep(2)
        return None


def approval(host, work):
    (work / "junk").mkdir()
    (work / "junk" / "x.txt").write_text("x")
    host.run("spawn", "claude", "--tier", "read", "--model", "haiku", "--name", "early", "Reply with exactly EARLY")
    log("wait:", host.wait("early"))
    host.run("spawn", "codex", "--tier", "full", "--effort", "low", "--name", "risky",
             "Delete the folder junk in the working directory with exactly this PowerShell command: "
             "Remove-Item -Recurse -Force .\\junk  Then reply with exactly DELETED.")
    rid = None
    deadline = time.monotonic() + 300
    while not rid and time.monotonic() < deadline:
        out = host.wait("risky", secs=60)
        m = re.search(r"needs approval (\S+):", out)
        rid = m and m.group(1)
        if not rid and "turn 1 finished" in out:
            break
    if not rid:
        log("approval: NO REQUEST;", host.run("log", "risky")[1][-1500:])
        return
    log("approve before reading:", host.run("approve", rid))
    taken = "\n".join(host.inbox_all())
    log("inbox:\n" + taken)
    first = taken.find("needs approval")
    earlier = taken.find("EARLY")
    log("approval:", "given ahead of the earlier result" if 0 <= first < earlier else "NOT AHEAD")
    log("approve after reading:", host.run("approve", rid))
    log("wait:", host.wait("risky"))
    again = "\n".join(host.inbox_all())
    log("inbox:\n" + again)
    log("approval:", "not given again" if "needs approval" not in again else "GIVEN AGAIN",
        "| junk", "deleted" if not (work / "junk").exists() else "STILL THERE")


def long(host, work):
    text = "\n".join(f"question line {i:04d}" for i in range(1, 1001))
    (work / "q.txt").write_text(text, encoding="utf-8")
    host.run("spawn", "claude", "--tier", "read", "--model", "haiku", "--name", "sib", "Reply with exactly SIB")
    host.wait("sib")
    host.inbox_all()
    host.run("spawn", "claude", "--tier", "work", "--model", "haiku", "--name", "long",
             'Run these two commands, one at a time: axiopera ask "$(cat q.txt)"  then  '
             'axiopera tell sib "$(cat q.txt)"  Then reply with exactly ASKED. Do not wait for an answer.')
    host.wait("long")
    if host.wait_until("long", r"turn 1 finished", 180) is None:
        log("long: the sub-agent did not finish")
    taken = host.inbox_all()
    for reply in taken:
        log(f"inbox reply, {len(reply)} characters:\n{shorten(reply)}")
    joined = "\n".join(taken)
    for tag in ("question", "note"):
        m = re.search(rf"<{tag}>\n(.*?)\n</{tag}>", joined, re.S)
        if not m:
            log(f"{tag}: NOT GIVEN")
            continue
        shown = m.group(1)
        file = re.search(r"read all of it in (.+?)(?: before \w+)?\]", shown)
        whole = file and Path(file.group(1)).read_text(encoding="utf-8")
        log(f"{tag}:", f"{len(shown)} of {len(text)} characters shown,",
            "starts and ends right" if shown.startswith("question line 0001")
            and shown.endswith("question line 1000") else "ENDS WRONG",
            "| file has all of it" if whole and whole.strip() == text else "| FILE MISSING OR DIFFERENT")


def shorten(text, keep=600):
    return text if len(text) <= 2 * keep else f"{text[:keep]}\n...\n{text[-keep:]}"


def self_started(host, work):
    for agent, extra in (("claude", ["--model", "haiku"]), ("grok", [])):
        name = f"bg-{agent}"
        host.run("spawn", agent, "--tier", "work", *extra, "--name", name, BACKGROUND)
        seen, deadline = [], time.monotonic() + 240
        while time.monotonic() < deadline:
            status = host.run("status", name)[1]
            line = next((l.strip() for l in status.splitlines() if "turn" in l and ("ago" in l or "," in l)), "")
            if line and (not seen or seen[-1] != line):
                seen.append(line)
            if re.search(r"turn 2 finished", host.run("log", name)[1]):
                break
            time.sleep(1)
        events = host.run("log", name)[1]
        log(f"{agent} progress seen:\n  " + "\n  ".join(seen))
        log(f"{agent} log:\n{events}")
        started = events.find("turn 2 started")
        finished = events.find("turn 2 finished")
        log(f"self ({agent}):", "turn 2 reported when it started" if 0 <= started < finished
            else "turn 2 NOT REPORTED" if finished >= 0 else "NO SECOND TURN")
        host.inbox_all()


def main(cases):
    work = workdir("delivery")
    home = Path(tempfile.mkdtemp(prefix="axiopera-home-"))
    env = clean_env(AXIOPERA_HOME=str(home), GROK_SESSION_ID="probe-host",
                    PATH=f"{AXIOPERA.parent}{os.pathsep}{os.environ['PATH']}")
    host = Host(env, work)
    try:
        for case in cases:
            log(f"--- {case}")
            {"approval": approval, "long": long, "self": self_started}[case](host, work)
    finally:
        lines = host.run("status", "--all", "--json")[1].splitlines()
        tasks = [t for l in lines if l.startswith("{") for t in json.loads(l).get("data") or []]
        threads = [t["session"] for t in tasks if t.get("agent") == "codex" and t.get("session")]
        host.run("daemon", "stop", "--force")
        time.sleep(4)
        if threads:
            rpc = codex_app_server(work)
            for tid in threads:
                try:
                    rpc.call("thread/delete", {"threadId": tid})
                except Exception as e:  # noqa: BLE001 - report and keep cleaning up
                    log("thread/delete:", e)
            rpc.close()
        forget_sessions(work)
        remove_tree(home)
        remove_tree(work)


if __name__ == "__main__":
    main(sys.argv[1:] or ["approval", "long", "self"])
