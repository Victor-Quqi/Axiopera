"""Codex as the main agent, sandboxed (workspace-write + on-request), calls a fake `axiopera spawn`
that refuses to run inside the sandbox. Questions:

- Does the model retry with escalation, and what does the approval request look like?
- After `acceptForSession`, are later spawns with different arguments approved without asking?
"""

import json
import os
import sys
import tempfile
from pathlib import Path

from probe import codex_app_server, log, remove_tree, workdir

FAKE = r'''
import os, sys, tempfile
probe = os.path.join(os.environ["LOCALAPPDATA"], "axiopera-spike-sandbox-probe.tmp")
try:
    open(probe, "w").close()
    os.remove(probe)
    sandboxed = False
except OSError:
    sandboxed = True
with open(os.path.join(tempfile.gettempdir(), "axiopera-spike-calls.log"), "a") as f:
    f.write(f"{sandboxed} {sys.argv[1:]}\n")
if sandboxed and sys.argv[1:2] == ["spawn"]:
    print("axiopera: spawn must run outside the sandbox. Re-run this exact command with escalated permissions.")
    sys.exit(2)
print(f"axiopera: started task for {sys.argv[1:]}")
'''

TURNS = [
    "Run the shell command `axiopera spawn --task alpha` and tell me its output.",
    "Run the shell command `axiopera spawn --task beta --backend grok` and tell me its output.",
    "Run the shell command `axiopera spawn --task alpha` again and tell me its output.",
]


def on_request(rpc, msg):
    p = msg["params"]
    if msg["method"] == "item/commandExecution/requestApproval":
        log("approval request:", json.dumps({k: p.get(k) for k in
            ("command", "reason", "proposedExecpolicyAmendment", "availableDecisions")}, ensure_ascii=False)[:500])
        return {"decision": "acceptForSession"}
    log("other request:", msg["method"], json.dumps(p)[:300])
    return None


def main():
    cwd = workdir("codex-escalate")
    bin_dir = cwd / "bin"
    bin_dir.mkdir()
    (bin_dir / "fake_axiopera.py").write_text(FAKE)
    (bin_dir / "axiopera.cmd").write_text(f'@python "{bin_dir / "fake_axiopera.py"}" %*\r\n')
    calls = Path(tempfile.gettempdir()) / "axiopera-spike-calls.log"
    calls.unlink(missing_ok=True)
    os.environ["PATH"] = f"{bin_dir};{os.environ['PATH']}"
    rpc = codex_app_server(cwd, on_request=on_request)
    thread_id = None
    try:
        thread_id = rpc.call("thread/start", {"cwd": str(cwd), "sandbox": "workspace-write",
                                              "approvalPolicy": "on-request", "approvalsReviewer": "user",
                                              "ephemeral": True})["thread"]["id"]
        for text in TURNS:
            log("turn:", text)
            rpc.call("turn/start", {"threadId": thread_id, "effort": "low", "input": [{"type": "text", "text": text}]})
            while (m := rpc.next(300)) not in (None, ...):
                params = m.get("params", {})
                item = params.get("item", {})
                if m.get("method") == "item/completed" and item.get("type") == "commandExecution":
                    log("  command:", item.get("command"), "exit", item.get("exitCode"), "status", item.get("status"))
                if m.get("method") == "turn/completed":
                    break
        log("fake axiopera calls (sandboxed, args):")
        for line in calls.read_text().splitlines() if calls.exists() else []:
            log("  ", line)
    finally:
        rpc.close()
        calls.unlink(missing_ok=True)
        remove_tree(cwd)


if __name__ == "__main__":
    main()
