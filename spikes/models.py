"""Query native model catalogs through an isolated Axiopera daemon. No inference turns.

Run after cargo build --locked: python -B spikes/models.py [claude|codex|grok ...]
"""

import json
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from probe import AXIOPERA, clean_env, codex_app_server, log, remove_tree


def main():
    errors = "--errors" in sys.argv
    agents = [arg for arg in sys.argv[1:] if arg != "--errors"] or ["claude", "codex", "grok"]
    assert all(agent in ("claude", "codex", "grok") for agent in agents)
    parent = (Path(__file__).resolve().parent.parent / ".local").resolve()
    parent.mkdir(exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="models-probe-", dir=parent)).resolve()
    env = clean_env(AXIOPERA_HOME=str(root / "data"))
    threads = []
    try:
        for agent in agents:
            result = subprocess.run(
                [str(AXIOPERA), "models", agent, "--cwd", str(root), "--json"],
                cwd=root, env=env, capture_output=True, text=True, encoding="utf-8", timeout=35,
            )
            if result.returncode:
                raise RuntimeError(f"{agent}: {result.stderr.strip()}")
            reply = json.loads(result.stdout)
            catalog = reply["data"]
            assert isinstance(catalog["models"], list), agent
            assert all(row["id"] for row in catalog["models"]), agent
            assert "description" not in reply["text"], agent
            log(agent, "models", len(catalog["models"]), "effort menus",
                sum(bool(row.get("efforts")) for row in catalog["models"]))
        assert not list((root / "data" / "tasks").glob("*/task.json"))
        log("No tasks created")
        if errors:
            for agent in (agent for agent in agents if agent == "codex"):
                result = subprocess.run(
                    [str(AXIOPERA), "spawn", agent, "--tier", "read", "--timeout", "20s", "--effort", "axiopera-invalid-effort", "Reply OK"],
                    cwd=root, env=env, capture_output=True, text=True, encoding="utf-8", timeout=20,
                )
                assert result.returncode == 0, result.stderr
                until = time.monotonic() + 20
                while time.monotonic() < until:
                    paths = list((root / "data" / "tasks").glob("*/task.json"))
                    tasks = [json.loads(path.read_text(encoding="utf-8")) for path in paths]
                    task = next(task for task in tasks if task["agent"] == agent)
                    if task["status"] == "failed":
                        break
                    time.sleep(0.1)
                else:
                    raise RuntimeError(f"{agent}: invalid effort did not fail")
                if agent == "codex" and task.get("session"):
                    threads.append(task["session"])
                result = subprocess.run(
                    [str(AXIOPERA), "inbox"], cwd=root, env=env,
                    capture_output=True, text=True, encoding="utf-8", timeout=10,
                )
                assert f"axiopera models {agent}" in result.stdout, result.stdout
                assert "effort:" in result.stdout, result.stdout
                log(agent, "invalid effort includes catalog")
    finally:
        stopped = subprocess.run(
            [str(AXIOPERA), "daemon", "stop", "--force"], cwd=root, env=env,
            capture_output=True, timeout=15,
        )
        assert stopped.returncode == 0, "probe daemon did not stop"
        if errors:
            for path in (root / "data" / "tasks").glob("*/task.json"):
                task = json.loads(path.read_text(encoding="utf-8"))
                if task["agent"] == "codex" and task.get("session"):
                    threads.append(task["session"])
        if threads:
            rpc = codex_app_server(root)
            try:
                for thread in set(threads):
                    rpc.call("thread/delete", {"threadId": thread})
            finally:
                rpc.close()
        projects = (Path.home() / ".claude/projects").resolve()
        session_dir = (projects / re.sub(r"[^A-Za-z0-9]", "-", str(root))).resolve()
        assert session_dir.parent == projects and "models-probe-" in session_dir.name
        if session_dir.exists():
            remove_tree(session_dir)
        assert root.parent == parent and root.name.startswith("models-probe-")
        remove_tree(root)


if __name__ == "__main__":
    main()
