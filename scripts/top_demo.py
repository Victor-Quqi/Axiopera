"""Fake tasks in every state, for looking at `axiopera top` in a real terminal.

    cargo build --locked
    python scripts/top_demo.py [--all] [--keep] [--bin PATH]

Writes the tasks to a new temporary data directory and runs `axiopera top` on it in this
terminal, while a thread keeps writing the way the daemon does: events appended, some a half
line at a time, `task.json` replaced whole, tasks added and removed. No daemon or agent runs.
q or Ctrl-C quits; the directory is then deleted unless --keep. Arguments after `--` go to top,
e.g. `-- trun` for the view of one task.
"""

import argparse
import json
import os
import shutil
import signal
import subprocess
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MIN = 60_000
CLAUDE = "claude:7c1e2d34-0d4b-4a8e-9a51-3f6b2c9d8e10"
CODEX = "codex:019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b"
SAYS = [
    "Reading the frame code first to see where rows are cut.",
    "The feed mixes every task's events; I will group them per task instead.",
    "Running the top tests now.",
    "两个测试失败了，先看宽字符的截断。",
    "Tests pass. Checking the narrow window case before reporting.",
]
TOOLS = [
    ("Read", "src/top.rs"),
    ("Grep", "fn frame"),
    ("Bash", "cargo test --locked top::"),
    ("Edit", "src/top.rs"),
    ("Bash", "运行测试：cargo test"),
    ("WebSearch", "DECAWM autowrap 🚀"),
    ("Bash", "git diff --stat"),
]


def now_ms():
    return int(time.time() * 1000)


class Home:
    """Writes as the daemon does. Failures are kept, not printed, since `top` owns the screen."""

    def __init__(self, path):
        self.root = Path(path) / "tasks"
        self.seq = 0
        self.tasks = {}
        self.errors = []

    def task(self, id, parent, agent, tier, status, ago, name=None, prompt=None, **more):
        at = now_ms() - ago
        if prompt:
            # As the daemon does: before task.json.
            (self.root / id).mkdir(parents=True, exist_ok=True)
            (self.root / id / "prompt.md").write_text(prompt, encoding="utf-8")
        self.tasks[id] = {
            "id": id, "name": name, "parent": parent, "agent": agent, "tier": tier,
            "cwd": str(ROOT), "model": None, "effort": None, "timeout_secs": None,
            "status": status, "session": None, "session_file": None, "turns": 1,
            "delivered": 1, "changed_files": [], "questions": [], "stalled": False,
            "detached": False, "created_ms": at, "updated_ms": at, **more,
        }
        self._save(id)

    def update(self, id, **fields):
        self.tasks[id].update(fields, updated_ms=now_ms())
        self._save(id)

    def _save(self, id):
        d = self.root / id
        (d / "turns").mkdir(parents=True, exist_ok=True)
        tmp = d / "task.json.tmp"
        tmp.write_text(json.dumps(self.tasks[id], ensure_ascii=False), encoding="utf-8")
        try:
            os.replace(tmp, d / "task.json")
        except OSError as e:
            self.errors.append(f"replacing {id}/task.json: {e}")

    def event(self, task, kind, ago=0, partial=False, **data):
        self.seq += 1
        e = {"seq": self.seq, "ts": now_ms() - ago, "task": task, "kind": kind, **data}
        line = json.dumps(e, ensure_ascii=False) + "\n"
        try:
            with open(self.root / task / "events.jsonl", "a", encoding="utf-8", newline="\n") as f:
                if partial:
                    f.write(line[: len(line) // 2])
                    f.flush()
                    time.sleep(0.4)
                    line = line[len(line) // 2 :]
                f.write(line)
        except OSError as e:
            self.errors.append(f"appending to {task}: {e}")

    def result(self, id, turn, text):
        (self.root / id / "turns" / f"{turn}.md").write_text(text, encoding="utf-8")

    def remove(self, id):
        self.tasks.pop(id, None)
        try:
            shutil.rmtree(self.root / id)
        except OSError as e:
            self.errors.append(f"removing {id}: {e}")


def build(h):
    """The tasks, then their past events in time order."""
    question = lambda id: [{"id": id, "text": "Which port should the dev server use?", "answer": None, "asked_ms": now_ms() - MIN}]
    # Under a Claude Code session: a running task with a sub-task that has ended its turn but
    # still runs background work, and one queued from inside it by a native sub-agent.
    h.task("trun", CLAUDE, "codex", "read", "running", 3 * MIN, name="review",
           prompt="Review the uncommitted changes to src/top.rs for correctness bugs.\n\n"
                  "Focus on terminal handling and the event reader.\nReport file:line findings.")
    h.task("tbg", "task:trun", "claude", "read", "running", 2 * MIN, name="sub-tests", turns=0,
           prompt="Run the test suite in the background and report failures.")
    h.task("tnat", "task:trun/claude:native-1", "grok", "work", "queued", 5_000, name="native-child",
           prompt="Check the Grok adapter for the same issue.")
    h.task("twait", CLAUDE, "claude", "work", "waiting", 4 * MIN, name="问题澄清",
           prompt="把开发服务器的端口改成可配置，先确认用哪个端口。", questions=question("q7k2"))
    h.task("tappr", CLAUDE, "codex", "full", "running", 5 * MIN, name="deploy-check", turns=0,
           prompt="Dry-run the deploy, then push if it passes.")
    h.task("tstall", CLAUDE, "grok", "work", "running", 30 * MIN, name="long-build", turns=0,
           stalled=True, prompt="Build everything with make and report warnings.")
    # Under a Codex session: stopped in every way.
    h.task("tfail", CODEX, "claude", "work", "failed", 10 * MIN, name="flaky")
    h.task("tint", CODEX, "codex", "work", "interrupted", 20 * MIN, name="interrupted")
    # Its open question no longer matters.
    h.task("tcxl", CODEX, "codex", "read", "cancelled", 25 * MIN, name="cancelled",
           questions=question("qold"))
    h.task("tdet", CODEX, "claude", "work", "idle", 15 * MIN, name="taken-over", detached=True)
    # From a terminal or an agent axiopera cannot tell apart. `ancient` shows only with --all;
    # `old-parent` shows because of its running sub-task.
    h.task("tidle", "user", "codex", "work", "idle", 5 * MIN, name="docs", turns=3,
           changed_files=["README.md", "docs/usage.md", "docs/design.md"],
           prompt="Update the usage docs for the new top command.")
    h.result("tidle", 3, "\nUpdated docs/usage.md and the two READMEs; design.md notes the reader.\n\n"
             "- usage: new section\n- README: one line each\n")
    h.task("told", "user", "codex", "read", "idle", 3 * 24 * 60 * MIN, name="ancient")
    h.task("tparent", "user", "claude", "work", "idle", 2 * 60 * MIN, name="old-parent")
    h.task("tchild", "task:tparent", "codex", "read", "running", MIN, name="still-running", turns=0)
    # Spawned by a task that has been removed since.
    h.task("torphan", "task:tgone", "codex", "read", "idle", 10 * MIN, name="orphan")

    past = [
        (25 * MIN, "tcxl", "cancelled", {"reason": "cancelled by request", "by": CODEX}),
        (25 * MIN, "tstall", "turn_started", {"turn": 1}),
        (20 * MIN, "tint", "interrupted", {"reason": "the daemon stopped"}),
        (15 * MIN, "tdet", "detached", {"reason": "its session file was changed by another process"}),
        (12 * MIN, "tstall", "tool", {"name": "run_terminal_cmd", "detail": "make -j8 all"}),
        (10 * MIN, "tfail", "failed", {"error": "API Error: 429 rate limited"}),
        (6 * MIN, "tidle", "file_changed", {"path": "docs/usage.md"}),
        (5 * MIN + 1_000, "tidle", "said", {"text": "Updated docs/usage.md and the two READMEs."}),
        (5 * MIN, "tidle", "turn_ended", {"turn": 3, "ok": True}),
        (4 * MIN, "tappr", "turn_started", {"turn": 1}),
        (3 * MIN + 5_000, "tappr", "said", {"text": "Dry run first; pushing needs approval."}),
        (3 * MIN, "tappr", "tool", {"name": "shell", "detail": "🚀 ./deploy.sh --dry-run"}),
        (2 * MIN, "trun", "turn_started", {"turn": 2}),
        (2 * MIN, "tstall", "stalled", {"quiet_secs": 600}),
        (110_000, "tbg", "turn_started", {"turn": 1}),
        (100_000, "tbg", "tool", {"name": "Bash", "detail": "cargo test --locked"}),
        (90_000, "trun", "tool", {"name": "Bash", "detail": "printf '\x1b[2J\x1b[31mred'\nrm -rf build"}),
        (80_000, "trun", "tool", {"name": "Bash", "detail": "echo " + "很长的中文参数" * 20}),
        (70_000, "trun", "tool", {"name": "Grep", "detail": "x" * 300}),
        (MIN, "twait", "question", {"id": "q7k2", "text": "Which port should the dev server use?"}),
        (MIN, "tchild", "turn_started", {"turn": 1}),
        (40_000, "tbg", "turn_ended", {"turn": 1, "ok": True}),
        (30_000, "tappr", "approval_request", {"id": "r1", "summary": "git push origin main"}),
    ]
    h.update("tbg", turns=1)
    for ago, task, kind, data in sorted(past, key=lambda p: -p[0]):
        h.event(task, kind, ago, **data)


def live(h, stop):
    """One step every 0.7 seconds, on a cycle that shows each kind of change."""
    tick = turns = added = 0
    pending = "r1"
    idle = False
    while not stop.wait(0.7):
        tick += 1
        if not idle:
            if tick % 3 == 1:
                h.event("trun", "said", text=SAYS[tick // 3 % len(SAYS)])
            name, detail = TOOLS[tick % len(TOOLS)]
            h.event("trun", "tool", partial=tick % 5 == 0, name=name, detail=detail)
        if tick % 20 == 10:
            turns += 1
            h.result("trun", 1 + turns, f"Turn {1 + turns}: no bugs found in the reader.\nDetails follow.\n")
            h.event("trun", "said", text=f"Turn {1 + turns}: no bugs found in the reader.")
            h.event("trun", "turn_ended", turn=turns + 1, ok=True)
            h.update("trun", status="idle", turns=1 + turns)
            idle = True
        elif tick % 20 == 14:
            h.update("trun", status="running")
            h.event("trun", "turn_started", turn=turns + 2)
            idle = False
        if tick % 4 == 0:
            if pending:
                h.event("tappr", "approved" if tick % 8 else "denied", id=pending, by=CLAUDE)
                pending = None
            else:
                pending = f"r{tick}"
                h.event("tappr", "approval_request", id=pending, summary="git push origin main")
        # Background work that starts a turn of its own, then ends it.
        if tick % 15 == 7:
            h.event("tbg", "tool", name="monitor", detail="")
        elif tick % 15 == 12:
            h.event("tbg", "turn_ended", turn=h.tasks["tbg"]["turns"] + 1, ok=True)
            h.update("tbg", turns=h.tasks["tbg"]["turns"] + 1)
        # A new task now and then; the oldest new one is removed, as gc would.
        if tick % 25 == 1:
            added += 1
            id = f"tnew{added}"
            h.task(id, "user", "claude", "work", "queued", 0, name=f"new-{added}", turns=0)
            if f"tnew{added - 2}" in h.tasks:
                h.remove(f"tnew{added - 2}")
        elif tick % 25 == 3:
            h.update(f"tnew{added}", status="running")
            h.event(f"tnew{added}", "turn_started", turn=1)
        elif tick % 25 == 20:
            h.event(f"tnew{added}", "turn_ended", turn=1, ok=True)
            h.update(f"tnew{added}", status="idle", turns=1)


def main():
    exe = "axiopera.exe" if os.name == "nt" else "axiopera"
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--bin", default=str(ROOT / "target" / "debug" / exe))
    ap.add_argument("--all", action="store_true", help="pass --all to top")
    ap.add_argument("--keep", action="store_true", help="keep the data directory")
    ap.add_argument("top_args", nargs="*", help="after --: more arguments for top")
    args = ap.parse_args()
    home = Path(tempfile.mkdtemp(prefix="axiopera-top-demo-"))
    h = Home(home)
    build(h)
    stop = threading.Event()
    writer = threading.Thread(target=live, args=(h, stop))
    writer.start()
    env = dict(os.environ, AXIOPERA_HOME=str(home))
    try:
        argv = [args.bin, "top", *(["--all"] if args.all else []), *args.top_args]
        child = subprocess.Popen(argv, env=env)
        # Ctrl-C reaches top as a key or a signal; it restores the terminal and exits.
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        code = child.wait()
    finally:
        stop.set()
        writer.join()
    print(f"top exited with {code}")
    for e in h.errors:
        print("write failed:", e)
    if args.keep:
        print(f"data kept: {home}")
    else:
        shutil.rmtree(home, ignore_errors=True)
    return 1 if code or h.errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
