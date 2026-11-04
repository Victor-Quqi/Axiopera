"""Query native model catalogs through an isolated Axiopera daemon. No inference turns.

Run after cargo build --locked: python -B spikes/models.py [claude|codex|grok ...] [--errors] [--tasks]

--errors spawns Codex with an invalid effort and expects the catalog with the error.
--tasks runs two one-line turns per agent, one without --model and one with --model and
--effort low, and checks the model and effort each task records against the request and
against the catalog's spawn default.
"""

import json
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from probe import AXIOPERA, clean_env, codex_app_server, forget_sessions, log, remove_tree


def run(root, env, *args, timeout=35):
    return subprocess.run(
        [str(AXIOPERA), *args], cwd=root, env=env,
        capture_output=True, text=True, encoding="utf-8", timeout=timeout,
    )


def finished_task(root, id, timeout=180):
    """The task once its first turn has ended."""
    path = root / "data" / "tasks" / id / "task.json"
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        task = json.loads(path.read_text(encoding="utf-8"))
        if task["status"] in ("idle", "failed", "interrupted"):
            return task
        time.sleep(0.5)
    raise RuntimeError(f"{id}: no turn ended within {timeout}s")


def check_tasks(root, env, agent, catalog):
    """A turn without --model runs on the spawn default; one with --model on the request."""
    default = catalog.get("native_model")
    others = [row["id"] for row in catalog["models"]
              if row["id"] not in (default, "default") and not row.get("disabled")]
    model = {"claude": "haiku", "codex": default, "grok": others[0] if others else default}[agent]
    for args in ([], ["--model", model, "--effort", "low"]):
        result = run(root, env, "--json", "spawn", agent, "--tier", "read", "--timeout", "150s", *args,
                     "Reply with exactly: OK")
        assert result.returncode == 0, result.stderr
        id = json.loads(result.stdout)["data"]["task"]
        task = finished_task(root, id)
        assert task["status"] == "idle", task
        actual, effort = task.get("actual_model"), task.get("actual_effort")
        log(agent, args or "no --model", "ran on", actual, effort)
        if args:
            # Claude resolves the alias to a full ID.
            assert actual and model in actual, (model, actual)
            assert effort == "low", effort
        elif agent == "grok":
            # Grok can pick its default again between the query and the spawn.
            if actual != default:
                log(agent, "spawn default", default, "differs from the session's", actual)
        else:
            assert actual == default, (default, actual)
            assert effort == catalog.get("native_effort"), (catalog.get("native_effort"), effort)
        status = run(root, env, "status", id).stdout
        assert f"model {actual}" in status, status
        inbox = run(root, env, "inbox", id).stdout
        assert f"turn 1 finished; model {actual}" in inbox, inbox


def main():
    errors = "--errors" in sys.argv
    run_tasks = "--tasks" in sys.argv
    agents = [arg for arg in sys.argv[1:] if not arg.startswith("--")] or ["claude", "codex", "grok"]
    assert all(agent in ("claude", "codex", "grok") for agent in agents)
    parent = (Path(__file__).resolve().parent.parent / ".local").resolve()
    parent.mkdir(exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="models-probe-", dir=parent)).resolve()
    env = clean_env(AXIOPERA_HOME=str(root / "data"))
    threads = []
    catalogs = {}
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
                sum(bool(row.get("efforts")) for row in catalog["models"]),
                "spawn default", catalog.get("native_model"), catalog.get("native_effort"))
            catalogs[agent] = catalog
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
        if run_tasks:
            for agent in agents:
                check_tasks(root, env, agent, catalogs[agent])
    finally:
        stopped = subprocess.run(
            [str(AXIOPERA), "daemon", "stop", "--force"], cwd=root, env=env,
            capture_output=True, timeout=15,
        )
        assert stopped.returncode == 0, "probe daemon did not stop"
        if errors or run_tasks:
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
        if run_tasks:
            forget_sessions(root)
        assert root.parent == parent and root.name.startswith("models-probe-")
        remove_tree(root)


if __name__ == "__main__":
    main()
