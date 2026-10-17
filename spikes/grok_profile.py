"""Which `_meta.agentProfile` shapes take built-in tools away from a Grok ACP session?
Each variant opens a session and asks the model to list its tool names.

usage: python grok_profile.py
"""

import json

from probe import forget_sessions, grok_acp, log, remove_tree, text_of, workdir

ASK = "List the exact names of all tools you can call, comma-separated, nothing else."
GONE = ["write", "search_replace"]

# `agentProfile` is a whole agent definition: without `name` and `description` it fails to parse
# and is silently ignored.
NAMED = {"name": "probe", "description": "probe"}
VARIANTS = {
    "none": {},
    "unnamed disallowedTools": {"agentProfile": {"disallowedTools": GONE}},
    "named disallowedTools": {"agentProfile": {**NAMED, "disallowedTools": GONE}},
}


def run(cwd, meta):
    rpc = grok_acp(cwd)
    try:
        sid = rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [],
                                       "_meta": {"yoloMode": False, "autoMode": False, **meta}})["sessionId"]
        reply = rpc.call_async("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": ASK}]})
        text = ""
        while reply.empty():
            m = rpc.next(1)
            if m in (None, ...):
                continue
            u = m.get("params", {}).get("update", {})
            if u.get("sessionUpdate") == "agent_message_chunk":
                text += text_of(u.get("content", {}))
        return text
    finally:
        rpc.close()


if __name__ == "__main__":
    cwd = workdir("grok-profile")
    try:
        for name, meta in VARIANTS.items():
            tools = run(cwd, meta)
            names = {t.strip(" `.\n") for t in tools.split(",")}
            missing = [t for t in GONE if t not in names]
            log(f"{name}: missing {missing or 'none'} | {json.dumps(tools)[:300]}")
    finally:
        forget_sessions(cwd)
        remove_tree(cwd)
