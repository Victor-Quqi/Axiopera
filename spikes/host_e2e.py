"""End to end: a host agent drives axiopera by itself, starting from `axiopera guide`.

Each host runs headless with the permissions a user would typically give it:
  claude  default mode, axiopera commands allowed
  codex   --approve-for-me (workspace-write sandbox, escalations auto-reviewed)
  grok    auto mode
It spawns Claude, Codex and Grok sub-agents, collects their results, follows up, and cancels one.
Prints the axiopera commands the host ran, its final answer, and the task table afterwards.

usage: python host_e2e.py claude|codex|grok
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

from probe import AXIOPERA, CLAUDE, CODEX, GROK, clean_env, forget_sessions, log, remove_tree, workdir

PROMPT = """Use the `axiopera` CLI to delegate work to other coding agents. Run `axiopera guide` first to learn how.
Then, with every sub-agent at tier read:
1. Delegate three tiny tasks at once: Claude (model haiku) replies with exactly ALPHA, Codex (effort low) replies with exactly BRAVO, Grok replies with exactly CHARLIE.
2. Collect all three replies.
3. Ask the Claude sub-agent a follow-up: reply with exactly DELTA. Collect that reply.
4. Delegate to Grok: write the numbers 1 to 500 in English words, one per line. Cancel it right away.
5. Report every reply you collected, and the output of `axiopera status`."""


def host_argv(kind):
    return {
        "claude": [CLAUDE, "-p", PROMPT, "--model", "sonnet", "--allowedTools", "Bash(axiopera *)",
                   "--output-format", "stream-json", "--verbose"],
        "codex": [CODEX, "exec", "--skip-git-repo-check", "--approve-for-me", "--ephemeral", "--json",
                  "-c", 'model_reasoning_effort="low"', PROMPT],
        "grok": [GROK, "-p", PROMPT, "--permission-mode", "auto", "--output-format", "streaming-json"],
    }[kind]


def commands(kind, messages):
    """Shell commands the host ran, once per tool call."""
    for m in messages:
        if kind == "claude" and m.get("type") == "assistant":
            for b in m["message"]["content"]:
                if b.get("type") == "tool_use" and "command" in b.get("input", {}):
                    yield b["input"]["command"]
        elif kind == "codex" and m.get("type") == "item.completed" and m["item"].get("type") == "command_execution":
            yield m["item"]["command"]
        elif kind == "grok" and m.get("type") == "tool_call" and "command" in m.get("rawInput", {}):
            yield m["rawInput"]["command"]


def final_text(kind, messages):
    if kind == "claude":
        return next((m.get("result", "") for m in reversed(messages) if m.get("type") == "result"), "")
    if kind == "codex":
        return next((m["item"].get("text", "") for m in reversed(messages)
                     if m.get("type") == "item.completed" and m.get("item", {}).get("type") == "agent_message"), "")
    # Grok's streaming-json: text pieces after the last tool call.
    text = ""
    for m in messages:
        if m.get("type") == "tool_call":
            text = ""
        elif m.get("type") == "text":
            text += m.get("data", "")
    return text


def axiopera(env, *args):
    p = subprocess.run([str(AXIOPERA), *args], env=env, capture_output=True, text=True, encoding="utf-8")
    return (p.stdout + p.stderr).strip()


def main(kind):
    cwd = workdir(f"e2e-{kind}")
    home = cwd.parent / f"{cwd.name}-home"
    path = clean_env()["PATH"]
    env = clean_env(AXIOPERA_HOME=str(home), PATH=f"{AXIOPERA.parent}{os.pathsep}{path}")
    try:
        t = time.monotonic()
        p = subprocess.run(host_argv(kind), cwd=cwd, env=env, capture_output=True, timeout=1200,
                           stdin=subprocess.DEVNULL)
        log(f"host exited {p.returncode} after {time.monotonic() - t:.0f}s")
        messages = []
        for line in p.stdout.decode("utf-8", errors="replace").splitlines():
            try:
                messages.append(json.loads(line))
            except ValueError:
                pass
        for c in commands(kind, messages):
            if "axiopera" in c:
                log("ran:", " ".join(c.split())[:200])
        log("final answer:\n" + final_text(kind, messages).strip()[:3000])
        if p.returncode != 0:
            log("stderr tail:", p.stderr.decode("utf-8", errors="replace")[-1500:])
        log("status --all:\n" + axiopera(env, "status", "--all"))
    finally:
        axiopera(env, "daemon", "stop", "--force")
        time.sleep(2)
        forget_codex_threads(home)
        forget_sessions(cwd)
        remove_tree(cwd)
        remove_tree(home)


def forget_codex_threads(home):
    """Delete the rollout files of Codex sub-agents; only paths inside ~/.codex/sessions."""
    sessions = (Path.home() / ".codex/sessions").resolve()
    for task in home.glob("tasks/*/task.json"):
        f = (json.loads(task.read_text(encoding="utf-8")).get("session_file") or {}).get("path")
        if f and Path(f).resolve().is_relative_to(sessions) and Path(f).name.startswith("rollout-"):
            Path(f).unlink(missing_ok=True)


if __name__ == "__main__":
    main(sys.argv[1])
