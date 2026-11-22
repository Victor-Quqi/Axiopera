"""Compare OpenCode's TUI, v2 API, and managed model selections against a loopback provider.

python spikes/opencode_models.py /path/to/opencode [--axiopera /path/to/axiopera]
"""

import argparse
import base64
import contextlib
import json
import queue
import sqlite3
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlencode
from urllib.request import ProxyHandler, Request, build_opener

from opencode_managed import cleanup_profile, isolated_profile, send_completion
from probe import AXIOPERA, Pty

def until(check, message):
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline:
        if value := check():
            return value
        time.sleep(.15)
    raise TimeoutError(message)


def native_catalog(opencode, cwd, env):
    auth = base64.b64encode(b"opencode:probe").decode()
    http = build_opener(ProxyHandler({}))
    with subprocess.Popen(
        [opencode, "serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"], cwd=cwd,
        env=dict(env, OPENCODE_USERNAME="opencode", OPENCODE_PASSWORD="probe"),
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    ) as proc:
        try:
            ready = queue.Queue()
            threading.Thread(target=lambda: ready.put(proc.stdout.readline()), daemon=True).start()
            base = json.loads(ready.get(timeout=20))["url"].rstrip("/")

            def get(path):
                url = base + path + "?" + urlencode({"location[directory]": str(cwd)})
                with http.open(Request(url, headers={"Authorization": "Basic " + auth}), timeout=15) as response:
                    return json.load(response)["data"]

            # The agent endpoint can expose built-ins before the project agents have registered.
            models = get("/api/model")
            primary = next(a for a in get("/api/agent") if a["mode"] != "subagent" and not a.get("hidden"))
            return models, get("/api/model/default"), primary
        finally:
            proc.stdin.close()
            try:
                proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                proc.kill()


def exercise(root, opencode, axiopera):
    cwd, exe, env = isolated_profile(root, opencode, axiopera)
    env["TERM"] = "xterm-256color"
    subprocess.run(["git", "init", "-q"], cwd=cwd, env=env, check=True)
    preferences = Path(env["XDG_STATE_HOME"]) / "opencode/model.json"
    database = Path(env["XDG_DATA_HOME"]) / "opencode/opencode.db"
    requests = queue.Queue()
    failures = []
    focus_marker = "OPENCODE-FOCUS-SYSTEM-PROBE"
    primary_id = "build"

    def compare(label, actual, expected):
        if actual != expected:
            failures.append(f"{label}: got {actual!r}, expected {expected!r}")

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            # Title generation can quote the prompt. Only the agent request carries tools.
            if body.get("tools"):
                has_focus = any(focus_marker in str(m["content"]) for m in body["messages"] if m["role"] == "system")
                requests.put((body["model"], self.headers.get("x-probe-effort"), body.get("reasoning_effort"), has_focus))
            send_completion(self, body["model"], {"content": "probe-finished"}, "stop")

    def cli(*args, raw=False):
        cmd = [exe, "--caller", "manual:opencode-models"]
        if not raw:
            cmd.append("--json")
        result = subprocess.run([*cmd, *args], cwd=cwd, env=env, capture_output=True,
                                text=True, encoding="utf-8", timeout=45, check=True)
        return result.stdout.strip() if raw else json.loads(result.stdout)["data"]

    def wire(label):
        model, header, body, has_focus = requests.get(timeout=30)
        compare(label + " variant header/body", header, body)
        compare(label + " primary instructions", has_focus, primary_id == "focus")
        return model, body

    def saved():
        return json.loads(preferences.read_text(encoding="utf-8")) if preferences.exists() else {}

    def tui(pick=None, cycle=False):
        pty = Pty([str(opencode), "--standalone", "--auto"], cwd, env)
        try:
            until(lambda: any(m in pty.screen.lower() for m in ("alpha", "beta", "gamma")), "TUI model missing")
            if pick:
                model, variant = pick
                if cycle:
                    # The primary agent already selects beta/low. Ctrl+T switches its variant to high.
                    until(lambda: model in pty.screen.lower(), "primary model not shown")
                    time.sleep(.5)
                    pty.write("\x14")
                else:
                    pty.type("/models")
                    until(lambda: "gamma" in pty.screen.lower(), "model picker did not open")
                    pty.type(model)
                    until(lambda: (saved().get("recent") or [{}])[0].get("modelID") == model, "model not saved")
                    time.sleep(.5)
                    pty.type(variant)
                until(lambda: saved().get("variant", {}).get("fixture/" + model) == variant,
                      "variant not saved")
                return
            pty.type("NATIVE-MODEL-PROBE")
            return wire("native TUI")
        finally:
            pty.close()

    def check_task(label, *options, model, effort, tier="full"):
        task = cli("spawn", "opencode", "--tier", tier, *options, label)["task"]
        actual = wire(label)
        compare(label + " model request", actual, (model, effort))
        cli("wait", task, "--timeout", "40s", raw=True)
        status = cli("status", task)[0]
        model = "fixture/" + actual[0]
        compare(label + " status", (status["status"], status.get("actual_model")), ("idle", model))
        effort = status.get("actual_effort")
        compare(label + " status effort", effort, actual[1])
        end = next(e for e in cli("log", task) if e["kind"] == "turn_ended")
        compare(label + " turn log", (end["model"], end.get("effort")), (model, effort))
        stamp = "model " + model + (", effort " + effort if effort else "")
        inbox = cli("inbox", task, raw=True)
        compare(label + " inbox", stamp in inbox, True)
        compare(label + " inbox effort", ", effort " in inbox, effort is not None)
        compare(label + " result", "probe-finished" in cli("result", task, "--force", raw=True), True)
        with contextlib.closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)) as db:
            agent = db.execute("select agent from session_v2 where id = ?", (status["session"],)).fetchone()[0]
        compare(label + " stored primary agent", agent, primary_id)
        print(label, "wire", actual, "agent", agent, flush=True)

    with ThreadingHTTPServer(("127.0.0.1", 0), Provider) as server:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        variants = [{"id": v, "headers": {"x-probe-effort": v}, "body": {"reasoning_effort": v}}
                    for v in ("low", "high")]
        provider = {"package": "@opencode/ai/providers/openai-compatible", "name": "Fixture",
                    "settings": {"apiKey": "fixture", "baseURL": f"http://127.0.0.1:{server.server_port}/v1"},
                    "models": {name: {"variants": variants} for name in ("alpha", "beta", "gamma")}}
        project = {"enabled_providers": ["fixture"], "snapshots": False, "providers": {"fixture": provider}}
        try:
            for case, expected in (("saved", ("gamma", "high")), ("project", ("alpha", "high")),
                                   ("primary", ("beta", "low")), ("saved-variant", ("beta", "high"))):
                print("Checking", case, flush=True)
                if case == "project":
                    project["model"] = "fixture/alpha#high"
                if case == "primary":
                    primary_id = "focus"
                    project.update(default_agent="focus", agent={"focus": {"mode": "primary",
                                   "description": "Fixture agent", "model": "fixture/beta#low", "prompt": focus_marker}})
                (cwd / "opencode.json").write_text(json.dumps(project), encoding="utf-8")
                if case in ("saved", "saved-variant"):
                    tui(pick=expected, cycle=case == "saved-variant")
                native = tui()
                compare(case + " fresh TUI", native, expected)
                rows, api_default, primary = native_catalog(opencode, cwd, env)
                compare(case + " native primary", primary["id"], primary_id)
                print(case, "TUI", native, "server default", api_default["id"], "primary", primary["id"], flush=True)
                catalog = cli("models", "opencode")
                expected_rows = {row["providerID"] + "/" + row["id"]: [v["id"] for v in row["variants"]] for row in rows}
                compare(case + " model catalog", {m["id"]: m["efforts"] for m in catalog["models"]}, expected_rows)
                if case == "saved":
                    native_cli = subprocess.check_output([opencode, "models", "--standalone"], cwd=cwd, env=env,
                                                         text=True, encoding="utf-8", timeout=30)
                    compare("native CLI model IDs", set(native_cli.splitlines()), set(expected_rows))
                lines = cli("models", "opencode", raw=True).splitlines()
                model, effort = catalog["native_model"], catalog.get("native_effort")
                compare(case + " listed default", (model, effort), ("fixture/" + native[0], native[1]))
                print(case, "Axiopera catalog", (model, effort), flush=True)
                suffix = ", effort " + effort if effort else ""
                compare(case + " text default", lines[0], f"Without --model, spawns use {model}{suffix}.")
                for name, efforts in expected_rows.items():
                    compare(case + " text " + name, any(line.startswith(name + " ") and
                            "effort: " + ", ".join(efforts) in line for line in lines), True)
                check_task(case, model=native[0], effort=native[1])
                check_task(case + "-read", tier="read", model=native[0], effort=native[1])

            check_task("effort-only", "--effort", "low", model="beta", effort="low")
            check_task("model-only", "--model", "fixture/gamma", model="gamma", effort="high")
            check_task("both", "--model", "fixture/gamma", "--effort", "low", model="gamma", effort="low")
            check_task("default-variant", "--effort", "default", model="beta", effort=None)
            # Give the configured-model run a separate daemon and immutable settings.
            cli("daemon", "stop", "--force", raw=True)
            configured = root / "configured-axiopera"
            configured.mkdir()
            (configured / "config.toml").write_text(
                f'[agents.opencode]\npath = {json.dumps(str(opencode))}\nmodel = "fixture/alpha"\n', encoding="utf-8")
            env["AXIOPERA_HOME"] = str(configured)
            catalog = cli("models", "opencode")
            compare("configured JSON model", catalog.get("configured_model"), "fixture/alpha")
            compare("configured JSON effort", catalog.get("configured_effort"), "high")
            compare("configured text model", cli("models", "opencode", raw=True).splitlines()[0],
                    "Without --model, spawns use fixture/alpha, effort high (Axiopera config).")
            check_task("configured", model="alpha", effort="high")
            check_task("override-config", "--model", "fixture/beta", "--effort", "low", model="beta", effort="low")
        finally:
            cli("daemon", "stop", "--force", raw=True)
            server.shutdown()
    if failures:
        raise AssertionError("\n" + "\n".join(failures))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("opencode", type=Path)
    parser.add_argument("--axiopera", type=Path, default=AXIOPERA)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="axiopera-opencode-models-"))
    try:
        exercise(root, args.opencode.resolve(strict=True), args.axiopera.resolve(strict=True))
    finally:
        cleanup_profile(root)
