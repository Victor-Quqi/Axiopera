"""Does Codex hand back the `clientUserMessageId` of a user message, so that a push is found again
without a marker in its text?

Sends messages into one thread of a private `codex app-server`, by `turn/start` and by `turn/steer`,
each with a `clientUserMessageId` of the shape axiopera gives its pushes, and reads them back as
axiopera does, with `thread/turns/list`:
  start    a message that starts a turn
  steer    a message steered into a running turn, read back while it runs and after it ends
  reload   the same thread read by a new app-server, before and after `thread/resume`
Prints for each message whether its `clientId` and its text came back as sent. Deletes the thread.

usage: python codex_client_message_id.py [model]
"""

import sys
import time
import uuid

from probe import CODEX, Rpc, log, remove_tree, workdir

# What a push carries: axiopera's lines, a sub-agent's text in tags, and characters a store might
# normalize: a tab, trailing spaces, CRLF, an escaped closing tag and trailing blank lines.
DATA = ("[t73iv codex] turn 1 finished; model m, effort low\n<result>\n中文结果 🦀\ttab  trailing  \r\n"
        "crlf line\n<\\/result> escaped\n</result>\n\n")


def server(cwd):
    rpc = Rpc([CODEX, "app-server"], cwd)
    info = rpc.call("initialize", {"clientInfo": {"name": "axiopera-spike", "version": "0"},
                                   "capabilities": {"experimentalApi": True}})
    rpc.notify("initialized", {})
    return rpc, info.get("userAgent", "")


def until(rpc, method, secs):
    """The next notification of `method`, or None."""
    deadline = time.monotonic() + secs
    while time.monotonic() < deadline:
        m = rpc.next(deadline - time.monotonic())
        if m in (None, ...):
            return None
        if m.get("method") == method:
            return m.get("params", {})
    return None


def user_messages(rpc, tid):
    """Every user message of the thread as `thread/turns/list` gives it, newest turn first."""
    out, cursor = [], None
    while True:
        page = rpc.call("thread/turns/list", {"threadId": tid, "limit": 20, "sortDirection": "desc",
                                              "itemsView": "full", "cursor": cursor})
        for turn in page["data"]:
            for item in turn.get("items") or []:
                if item.get("type") == "userMessage":
                    text = "".join(c.get("text", "") for c in item.get("content", []))
                    out.append((turn["id"], turn.get("status"), item.get("clientId"), text))
        cursor = page.get("nextCursor")
        if not cursor:
            return out


def report(label, found, sent):
    """Of each message sent, whether it came back by its ID, and with its text unchanged."""
    by_id = {client: (turn, status, text) for turn, status, client, text in found if client}
    log(f"{label}: {len(found)} user message(s)")
    for client, (name, text) in sent.items():
        if client not in by_id:
            same = [t for _, _, _, t in found if t == text]
            log(f"  {name}: NOT found by clientId; same text in {len(same)} message(s)")
            continue
        turn, status, back = by_id[client]
        if back == text:
            log(f"  {name}: found by clientId in turn …{turn[-6:]} ({status}), text identical ({len(text)} chars)")
        else:
            at = next((i for i, (a, b) in enumerate(zip(back, text)) if a != b), min(len(back), len(text)))
            log(f"  {name}: found by clientId in turn …{turn[-6:]} ({status}), text DIFFERS at {at}: "
                f"sent {text[at:at + 24]!r} ({len(text)}), got {back[at:at + 24]!r} ({len(back)})")


def main(model):
    work = workdir("codex-client-message-id")
    rpc = tid = None
    sent = {}

    def send(name, text):
        """Notes a message to send, under an ID of the shape axiopera gives its pushes."""
        client = f"axiopera:{uuid.uuid4().hex[:12]}"
        sent[client] = (name, text)
        return {"clientUserMessageId": client, "input": [{"type": "text", "text": text, "text_elements": []}]}

    try:
        rpc, agent = server(work)
        log("app-server:", agent)
        tid = rpc.call("thread/start", {"cwd": str(work), "sandbox": "read-only", "approvalPolicy": "never",
                                        "model": model})["thread"]["id"]
        log("thread", tid)

        # A message that starts a turn.
        first = send("start", "Reply with just: ok. Run no commands. The rest of this message is test data.\n" + DATA)
        rpc.call("turn/start", {"threadId": tid, "effort": "low", **first})
        log("start turn:", (until(rpc, "turn/completed", 180) or {}).get("turn", {}).get("status"))
        report("after turn/start", user_messages(rpc, tid), sent)

        # A message steered into a running turn.
        long = send("long", "Write the numbers from 1 to 250 in English words, one per line. Run no commands.")
        turn = rpc.call("turn/start", {"threadId": tid, "effort": "low", **long})["turn"]["id"]
        until(rpc, "item/agentMessage/delta", 60)
        steer = send("steer", "Stop counting. Reply with just: steered.\n" + DATA)
        try:
            rpc.call("turn/steer", {"threadId": tid, "expectedTurnId": turn, **steer})
            log("steer: accepted")
        except Exception as e:
            log("steer: refused:", e)
        report("while the steered turn runs", user_messages(rpc, tid), sent)
        log("steered turn:", (until(rpc, "turn/completed", 300) or {}).get("turn", {}).get("status"))
        report("after the steered turn", user_messages(rpc, tid), sent)

        # The thread as another process reads it from its rollout.
        rpc.close()
        rpc, _ = server(work)
        try:
            report("new app-server, thread not loaded", user_messages(rpc, tid), sent)
        except Exception as e:
            log("new app-server, thread not loaded: thread/turns/list failed:", e)
        rpc.call("thread/resume", {"threadId": tid})
        report("new app-server, after thread/resume", user_messages(rpc, tid), sent)
    finally:
        if tid:
            if rpc is None or rpc.proc.poll() is not None:
                rpc, _ = server(work)
            try:
                rpc.call("thread/delete", {"threadId": tid})
                log("thread deleted")
            except Exception as e:
                log("thread/delete failed:", e, "| thread", tid)
        if rpc:
            rpc.close()
        remove_tree(work)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "gpt-5.6-luna")
