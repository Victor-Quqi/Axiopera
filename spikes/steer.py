"""Mid-turn steer under a long turn: several steers, a burst, and one after the turn ends.

usage: python steer.py claude|codex|grok
"""

import sys
import threading
import time
import uuid

from probe import (claude_stream, forget_sessions, claude_user, codex_app_server, grok_acp, log, remove_tree,
                   text_of, workdir, RpcError)

RUNS = 6
TASK = (
    f'Run this shell command {RUNS} times, strictly one tool call at a time, waiting for each to '
    'finish before starting the next: python -c "import time; time.sleep(12)"\n'
    "After each run write one short line `run N done`.\n"
    "Messages from me may arrive while you work. Whenever you see one, put its exact token "
    "(like TOKEN-XXXX) in your very next line of text, then continue the runs.\n"
    f"After all {RUNS} runs, end with `ALL DONE` followed by every token you received, in arrival order."
)
SCHEDULE = [(6, "TOKEN-A1"), (20, "TOKEN-B2"), (34, "TOKEN-C3"), (34.5, "TOKEN-D4"), (60, "TOKEN-E5")]
LATE = "TOKEN-F6"


class Agent:
    def __init__(self):
        self.text = ""
        self.turns_done = 0
        self.turn_done = threading.Event()

    def on_text(self, s):
        self.text += s

    def on_turn_end(self):
        self.turns_done += 1
        self.turn_done.set()


class Claude(Agent):
    def start(self, cwd, task):
        self.p = claude_stream(cwd, "--model", "haiku", "--permission-mode", "bypassPermissions",
                              "--no-session-persistence")
        threading.Thread(target=self.pump, daemon=True).start()
        self.p.send(claude_user(task))

    def pump(self):
        while (m := self.p.next(600)) not in (None, ...):
            if m.get("type") == "assistant":
                self.on_text(text_of(m["message"]["content"]) + "\n")
            elif m.get("type") == "result":
                log("claude result:", m.get("subtype"), "turns:", m.get("num_turns"))
                self.on_turn_end()

    def steer(self, text):
        self.p.send(claude_user(text))
        return "sent"

    def close(self):
        self.p.close()


class Codex(Agent):
    def start(self, cwd, task):
        self.rpc = codex_app_server(cwd)
        threading.Thread(target=self.pump, daemon=True).start()
        th = self.rpc.call("thread/start", {"cwd": str(cwd), "sandbox": "danger-full-access",
                                            "approvalPolicy": "never", "ephemeral": True})
        self.thread_id = th["thread"]["id"]
        turn = self.rpc.call("turn/start", {"threadId": self.thread_id, "effort": "low",
                                            "input": [{"type": "text", "text": task}]})
        self.turn_id = turn["turn"]["id"]

    def pump(self):
        while (m := self.rpc.next(600)) not in (None, ...):
            method, params = m.get("method"), m.get("params", {})
            if method == "item/completed" and params.get("item", {}).get("type") == "agentMessage":
                self.on_text(params["item"].get("text", "") + "\n")
            elif method == "turn/completed":
                log("codex turn/completed:", params.get("turn", {}).get("status"))
                self.on_turn_end()

    def steer(self, text):
        try:
            self.rpc.call("turn/steer", {"threadId": self.thread_id, "expectedTurnId": self.turn_id,
                                         "input": [{"type": "text", "text": text}]})
            return "ok"
        except RpcError as e:
            return f"error {e}"

    def close(self):
        self.rpc.close()


class Grok(Agent):
    def start(self, cwd, task):
        self.rpc = grok_acp(cwd)
        threading.Thread(target=self.pump, daemon=True).start()
        s = self.rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [], "_meta": {"yoloMode": True}})
        self.sid = s["sessionId"]
        self.prompt = self.rpc.call_async("session/prompt", {"sessionId": self.sid,
                                                             "prompt": [{"type": "text", "text": task}]})
        threading.Thread(target=self.wait_prompt, daemon=True).start()

    def wait_prompt(self):
        reply = self.prompt.get()
        log("grok prompt reply:", reply.get("result") or reply.get("error"))
        self.on_turn_end()

    def pump(self):
        while (m := self.rpc.next(600)) not in (None, ...):
            u = m.get("params", {}).get("update", {})
            if u.get("sessionUpdate") == "agent_message_chunk":
                self.on_text(u.get("content", {}).get("text", ""))

    def steer(self, text):
        try:
            r = self.rpc.call("_x.ai/interject", {"sessionId": self.sid, "text": text,
                                                  "interjectionId": str(uuid.uuid4())})
            return f"ok {r}"
        except RpcError as e:
            return f"error {e}"

    def close(self):
        self.rpc.close()


def main(kind):
    agent = {"claude": Claude, "codex": Codex, "grok": Grok}[kind]()
    cwd = workdir(f"steer-{kind}")
    sent, seen = {}, {}
    try:
        agent.start(cwd, TASK)
        t0 = time.monotonic()
        pending = list(SCHEDULE)
        while not agent.turn_done.is_set() and time.monotonic() - t0 < 420:
            now = time.monotonic() - t0
            while pending and pending[0][0] <= now:
                _, tok = pending.pop(0)
                sent[tok] = now
                log(f"steer {tok}:", agent.steer(f"Message from the user: {tok}"))
            for tok in sent:
                if tok not in seen and tok in agent.text:
                    seen[tok] = time.monotonic() - t0
                    log(f"seen {tok} after {seen[tok] - sent[tok]:.1f}s")
            time.sleep(0.2)
        log("turn done:", agent.turn_done.is_set(), "unsent:", [t for _, t in pending])

        # Race: steer right after the turn ended.
        agent.turn_done.clear()
        sent[LATE] = time.monotonic() - t0
        log(f"late steer {LATE}:", agent.steer(f"Message from the user: {LATE}"))
        agent.turn_done.wait(90)
        time.sleep(1)
        for tok in sent:
            if tok not in seen and tok in agent.text:
                seen[tok] = time.monotonic() - t0
        tail = agent.text[agent.text.rfind("ALL DONE"):][:300] if "ALL DONE" in agent.text else "(no ALL DONE)"
        log("final:", tail.replace("\n", " | "))
        log("summary:", {t: (round(seen[t] - sent[t], 1) if t in seen else None) for t in sent},
            "turns completed:", agent.turns_done)
    finally:
        agent.close()
        forget_sessions(cwd)
        remove_tree(cwd)


if __name__ == "__main__":
    main(sys.argv[1])
