"""Verify read -> full -> read in one native session through Axiopera.

Checks session identity, remembered context, a real file write at full, and refusal
to change that file at read. Uses an isolated daemon and deletes probe sessions.
Usage: python spikes/tier_resume.py claude|codex|grok
"""

import json
import os
import subprocess
import sys
import time
import uuid

from probe import AXIOPERA, clean_env, codex_app_server, forget_sessions, log, remove_tree, workdir


def main(agent):
    work = workdir(f"tier-resume-{agent}")
    home = work / "axiopera-home"
    env = clean_env(AXIOPERA_HOME=str(home),
                    PATH=f"{AXIOPERA.parent}{os.pathsep}{os.environ['PATH']}")
    env.pop("AXIOPERA_TASK_ID", None)
    session = None

    def call(*args):
        proc = subprocess.run([str(AXIOPERA), *args, "--json"], cwd=work, env=env,
                              capture_output=True, text=True, encoding="utf-8", timeout=30)
        if proc.returncode:
            raise RuntimeError(f"{args[0]}: {proc.stdout.strip()} {proc.stderr.strip()}")
        return [json.loads(line) for line in proc.stdout.splitlines() if line.strip()]

    def settled(turn):
        nonlocal session
        deadline = time.monotonic() + 240
        while time.monotonic() < deadline:
            task = call("status", "probe")[0]["data"][0]
            session = task.get("session") or session
            if task["status"] in ("failed", "cancelled", "interrupted"):
                raise RuntimeError(f"turn {turn}: {task['status']}")
            if task["turns"] >= turn and task["status"] in ("idle", "waiting"):
                # Status is persisted immediately before the actor releases its channel.
                time.sleep(0.2)
                return task
            time.sleep(0.5)
        raise TimeoutError(f"turn {turn}")

    try:
        marker = f"MARK-{uuid.uuid4().hex}"
        extra = ["--model", "haiku"] if agent == "claude" else []
        if agent == "codex":
            extra = ["--effort", "low"]
        call("spawn", agent, "--tier", "read", "--name", "probe", *extra,
             f"Remember this marker for later: {marker}. Reply with it. Do not use tools.")
        first = settled(1)
        original_session = first["session"]
        assert first["tier"] == "read" and original_session
        log(agent, "read: session created")

        call("send", "probe", "--tier", "full",
             "Write the marker from my first message into marker.txt in the current directory, "
             "using your file editing tool. Then reply DONE.")
        second = settled(2)
        assert second["session"] == original_session and second["tier"] == "full"
        assert (work / "marker.txt").read_text(encoding="utf-8").strip() == marker
        log(agent, "full: same session, context retained, file written")

        call("send", "probe", "--tier", "read",
             "Try once to replace marker.txt with CHANGED using your file editing tool. "
             "If permissions prevent it, report that and stop.")
        third = settled(3)
        assert third["session"] == original_session and third["tier"] == "read"
        assert (work / "marker.txt").read_text(encoding="utf-8").strip() == marker
        log(agent, "read: same session, file unchanged")
    finally:
        try:
            if home.exists():
                call("daemon", "stop", "--force")
                time.sleep(2)
        finally:
            try:
                if agent == "codex" and session:
                    rpc = codex_app_server(work)
                    try:
                        rpc.call("thread/delete", {"threadId": session})
                    finally:
                        rpc.close()
                forget_sessions(work)
            finally:
                remove_tree(work)


if __name__ == "__main__":
    main(sys.argv[1])
