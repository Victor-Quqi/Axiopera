"""Two questions about native sessions, per agent:

1. After resume, do the reported session ID and the env var the host injects into its shell stay the same?
2. While a long-lived holder (what the daemon would keep) has the session open and idle, another
   process resumes it and adds a turn (what a TUI takeover does). Does either side fail, and whose
   history does a later resume load?

usage: python resume.py claude|codex|grok
"""

import json
import subprocess
import sys
import uuid

from probe import (CLAUDE, CODEX, GROK, claude_stream, claude_user, clean_env, codex_app_server,
                   forget_sessions, grok_acp, log, remove_tree, text_of, workdir)

ENV_VAR = {"claude": "CLAUDE_CODE_SESSION_ID", "codex": "CODEX_THREAD_ID", "grok": "GROK_SESSION_ID"}
LIST = "Without running any tools, list every MARK- token that appears in this conversation, in order."


def env_turn(var, file, mark):
    cmd = f"""python -c "import os; open('{file}','w').write(os.environ.get('{var}','(unset)'))\""""
    return f"Run this shell command exactly once: {cmd}\nThen reply with just: {mark}"


def run(argv, cwd):
    p = subprocess.run(argv, cwd=cwd, env=clean_env(), capture_output=True, timeout=300)
    out = p.stdout.decode(errors="replace")
    if p.returncode:
        log("exit", p.returncode, "stderr tail:", p.stderr.decode(errors="replace").strip()[-400:])
    return out


def marks(text):
    return [w.strip(".,`*:;") for w in text.split() if "MARK-" in w]


class Claude:
    def holder_start(self, cwd, prompt):
        self.sid = str(uuid.uuid4())
        self.p = claude_stream(cwd, "--model", "haiku", "--permission-mode", "bypassPermissions",
                               "--session-id", self.sid)
        return self.holder_turn(prompt)

    def holder_turn(self, prompt):
        self.p.send(claude_user(prompt))
        text = ""
        while (m := self.p.next(300)) not in (None, ...):
            if m.get("type") == "assistant":
                text += text_of(m["message"]["content"]) + "\n"
            if m.get("type") == "result":
                return m.get("session_id"), text
        return None, text + "(holder exited)"

    def external(self, cwd, prompt):
        out = run([CLAUDE, "-p", prompt, "--model", "haiku", "--permission-mode", "bypassPermissions",
                   "--output-format", "json", "--resume", self.sid], cwd)
        try:
            r = json.loads(out)
            return r.get("session_id"), r.get("result", "")
        except ValueError:
            return None, out[-300:]

    def close(self):
        self.p.close()


class Codex:
    def holder_start(self, cwd, prompt):
        self.rpc = codex_app_server(cwd)
        th = self.rpc.call("thread/start", {"cwd": str(cwd), "sandbox": "danger-full-access",
                                            "approvalPolicy": "never"})
        self.sid = th["thread"]["id"]
        return self.holder_turn(prompt)

    def holder_turn(self, prompt):
        self.rpc.call("turn/start", {"threadId": self.sid, "effort": "low",
                                     "input": [{"type": "text", "text": prompt}]})
        text = ""
        while (m := self.rpc.next(300)) not in (None, ...):
            params = m.get("params", {})
            if m.get("method") == "item/completed" and params.get("item", {}).get("type") == "agentMessage":
                text += params["item"].get("text", "") + "\n"
            if m.get("method") == "turn/completed":
                return params.get("threadId"), text
        return None, text + "(holder exited)"

    def external(self, cwd, prompt):
        out = run([CODEX, "exec", "--json", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox",
                   "-c", 'model_reasoning_effort="low"', "resume", self.sid, prompt], cwd)
        sid, text = None, ""
        for line in out.splitlines():
            try:
                e = json.loads(line)
            except ValueError:
                continue
            if e.get("type") == "thread.started":
                sid = e.get("thread_id")
            if e.get("type") == "item.completed" and e.get("item", {}).get("type") == "agent_message":
                text += e["item"].get("text", "") + "\n"
            if e.get("type") in ("error", "turn.failed"):
                text += f"[{e.get('type')}] {json.dumps(e)[:300]}\n"
        return sid, text

    def close(self):
        self.rpc.close()

    def cleanup(self, cwd):
        rpc = codex_app_server(cwd)
        try:
            rpc.call("thread/delete", {"threadId": self.sid})
        except Exception as e:
            log("thread/delete failed:", e)
        rpc.close()


class Grok:
    def holder_start(self, cwd, prompt):
        self.rpc = grok_acp(cwd)
        self.sid = self.rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [],
                                                 "_meta": {"yoloMode": True}})["sessionId"]
        return self.holder_turn(prompt)

    def holder_turn(self, prompt):
        while self.rpc.next(0.1) is not ...:
            pass
        r = self.rpc.call("session/prompt", {"sessionId": self.sid, "prompt": [{"type": "text", "text": prompt}]}, 300)
        text = ""
        while (m := self.rpc.next(0.5)) not in (None, ...):
            u = m.get("params", {}).get("update", {})
            if u.get("sessionUpdate") == "agent_message_chunk":
                text += u.get("content", {}).get("text", "")
        return r.get("_meta", {}).get("sessionId"), text

    def external(self, cwd, prompt):
        out = run([GROK, "-p", prompt, "-r", self.sid, "--permission-mode", "bypassPermissions",
                   "--output-format", "json"], cwd)
        try:
            r = json.loads(out)
        except ValueError:
            return None, out[-300:]
        sid = r.get("session_id") or r.get("sessionId") or r.get("_meta", {}).get("sessionId")
        return sid, r.get("result") or r.get("text") or json.dumps(r)[:300]

    def close(self):
        self.rpc.close()


def main(kind):
    a = {"claude": Claude, "codex": Codex, "grok": Grok}[kind]()
    var = ENV_VAR[kind]
    cwd = workdir(f"resume-{kind}")
    try:
        sid, text = a.holder_start(cwd, env_turn(var, "holder.txt", "MARK-H1"))
        log("holder turn 1: sid", sid, "marks", marks(text))
        sid_x, text = a.external(cwd, env_turn(var, "external.txt", "MARK-X"))
        log("external resume: sid", sid_x, "marks", marks(text))
        sid, text = a.holder_turn(LIST)
        log("holder turn 2: sid", sid, "sees", marks(text))
        a.close()
        sid_f, text = a.external(cwd, LIST)
        log("fresh resume after holder closed: sid", sid_f, "sees", marks(text))
        for f in ("holder.txt", "external.txt"):
            p = cwd / f
            log(f"{var} in {f}:", p.read_text() if p.exists() else "(missing)")
        log("created sid:", a.sid)
    finally:
        a.close()
        if hasattr(a, "cleanup") and hasattr(a, "sid"):
            a.cleanup(cwd)
        forget_sessions(cwd)
        remove_tree(cwd)


if __name__ == "__main__":
    main(sys.argv[1])
