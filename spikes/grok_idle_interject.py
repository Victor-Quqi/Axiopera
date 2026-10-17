"""What does Grok do with `_x.ai/interject` when no turn is running, and how does that turn end?"""

import time
import uuid

from probe import forget_sessions, grok_acp, log, remove_tree, workdir

cwd = workdir("grok-idle")
rpc = grok_acp(cwd)
try:
    sid = rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [], "_meta": {"yoloMode": True}})["sessionId"]
    r = rpc.call("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "Say hi in two words."}]}, 120)
    log("prompt 1:", r.get("stopReason"))
    while rpc.next(2) is not ...:
        pass
    time.sleep(3)
    log("interject:", rpc.call("_x.ai/interject", {"sessionId": sid, "text": "Reply with exactly TOKEN-Z9.",
                                                  "interjectionId": str(uuid.uuid4())}))
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        m = rpc.next(1)
        if m in (None, ...):
            continue
        u = m.get("params", {}).get("update", {})
        kind = u.get("sessionUpdate") or m.get("method")
        detail = u.get("content", {}).get("text", "") if isinstance(u.get("content"), dict) else ""
        log("event:", kind, repr(detail[:60]), sorted(k for k in u if k not in ("sessionUpdate", "content")))
    # A normal prompt afterwards: is the session still usable?
    r = rpc.call("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "Say bye in two words."}]}, 120)
    log("prompt 2:", r.get("stopReason"))
finally:
    rpc.close()
    forget_sessions(cwd)
    remove_tree(cwd)
