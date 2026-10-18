"""After a turn ends while the agent still has a background command running: what does the
protocol show, and does the agent start a turn by itself when the command completes?

With `busy`, the agent keeps working in the foreground instead: does the completion reach the
model within the running turn, or only once it ends?

usage: python background.py claude|grok [auto] [busy]   (auto: Grok in autoMode instead of yolo)
"""

import sys
import time

from probe import claude_stream, claude_user, forget_sessions, grok_acp, log, remove_tree, workdir

TASK = ('Start this shell command in the background (do not wait for it): python -c "import time; time.sleep(20); print(7)" '
        "then end your turn right away with one short sentence. When it finishes, report its output.")
BUSY = ('Start this shell command in the background (do not wait for it): python -c "import time; time.sleep(15); print(7)" '
        'Then, without checking on it, run this foreground command five times, one call at a time: '
        'python -c "import time; time.sleep(6)" '
        "After each call, say in one short line whether the background command has finished and what it printed, "
        "as far as you know at that moment. Then end your turn.")
TASK = BUSY if "busy" in sys.argv else TASK
WINDOW = 120 if "busy" in sys.argv else 75


def show(kind, detail=""):
    log(kind, detail[:160].replace("\n", " "))


def claude(cwd):
    p = claude_stream(cwd, "--model", "haiku", "--permission-mode", "bypassPermissions", "--no-session-persistence")
    try:
        p.send(claude_user(TASK))
        end = time.monotonic() + WINDOW
        while time.monotonic() < end:
            m = p.next(1)
            if m in (None, ...):
                if m is None:
                    show("EOF")
                    break
                continue
            t, st = m.get("type"), m.get("subtype")
            if t == "assistant":
                for b in m["message"]["content"]:
                    show("assistant." + b["type"], b.get("text") or str(b.get("input")))
            elif t == "result":
                show("RESULT", f"{st} {m.get('result', '')}")
            elif t == "system":
                show(f"system.{st}", str({k: v for k, v in m.items() if k not in ("type", "subtype", "session_id", "uuid", "tools", "mcp_servers", "slash_commands", "agents", "skills", "plugins")}))
            else:
                show(t or "?", str(m)[:160])
    finally:
        p.close()


def grok(cwd):
    rpc = grok_acp(cwd)
    try:
        meta = {"yoloMode": False, "autoMode": True} if "auto" in sys.argv else {"yoloMode": True}
        sid = rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [], "_meta": meta})["sessionId"]
        reply = rpc.call_async("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": TASK}]})
        end = time.monotonic() + WINDOW
        while time.monotonic() < end:
            if not reply.empty():
                show("PROMPT REPLY", str(reply.get().get("result", {}).get("stopReason")))
            m = rpc.next(1)
            if m in (None, ...):
                continue
            u = m.get("params", {}).get("update", {})
            kind = u.get("sessionUpdate") or m.get("method")
            if kind in ("agent_thought_chunk", "available_commands_update"):
                continue
            detail = ""
            if isinstance(u.get("content"), dict):
                detail = u["content"].get("text", "")
            elif u:
                detail = str({k: v for k, v in u.items() if k not in ("sessionUpdate", "content", "rawInput", "rawOutput")})
            else:
                detail = str(m.get("params"))
            show(kind, detail)
    finally:
        rpc.close()


if __name__ == "__main__":
    cwd = workdir(f"bg-{sys.argv[1]}")
    try:
        {"claude": claude, "grok": grok}[sys.argv[1]](cwd)
    finally:
        forget_sessions(cwd)
        remove_tree(cwd)
