"""Compare native Pi selections and actual requests with Axiopera. Usage: python spikes/pi_models.py /path/to/pi"""

import contextlib
import json
import queue
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

from pi_extensions import Fixture, wait_until
from probe import AXIOPERA, Pty


@contextlib.contextmanager
def native(f, *options):
    with subprocess.Popen(
        [f.pi, "--mode", "rpc", "--no-session", *options], cwd=f.cwd, env=f.env,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, encoding="utf-8",
    ) as proc:
        replies = queue.Queue()

        def read_frames():
            for line in proc.stdout:
                replies.put(line)
            replies.put(None)

        def call(command, **args):
            proc.stdin.write(json.dumps({"type": command, "id": command, **args}) + "\n")
            proc.stdin.flush()
            while True:
                line = replies.get(timeout=20)
                assert line is not None, f"Pi exited during {command}"
                frame = json.loads(line)
                if frame.get("id") == command:
                    assert frame.get("success"), command
                    return frame["data"]

        threading.Thread(target=read_frames, daemon=True).start()
        try:
            yield call
        finally:
            proc.kill()


def cli_models(f):
    rows = f.run([f.pi, "--list-models"]).splitlines()
    return {"/".join(line.split()[:2]) for line in rows[1:] if line.strip()}


def dynamic_models(f, url, catalog, task):
    ready = f.root / "catalog-ready"
    f.env.pop("PI_OFFLINE")
    f.env.update(PI_SKIP_VERSION_CHECK="1", HTTPS_PROXY="http://127.0.0.1:1", HTTP_PROXY="http://127.0.0.1:1",
                 NO_PROXY="localhost,127.0.0.1", FIXTURE_CATALOG_READY=str(ready), FIXTURE_BASE_URL=url)
    directory = f.agent / "extensions"
    directory.mkdir()
    (directory / "catalog.ts").write_text("""
import { existsSync } from "node:fs";
const model = (id, reasoning) => ({
  id, name: id, reasoning, input: ["text"], contextWindow: 10000, maxTokens: 200,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
});
export default (pi) => {
  for (const [provider, models] of [["dynamic", [model("seed", false)]], ["unseeded", []]]) {
    pi.registerProvider(provider, {
      api: "openai-completions", apiKey: "fixture", baseUrl: process.env.FIXTURE_BASE_URL, models,
      async refreshModels({ allowNetwork, signal }) {
        if (!allowNetwork) return models;
        while (!signal.aborted && !existsSync(process.env.FIXTURE_CATALOG_READY)) {
          await new Promise(resolve => setTimeout(resolve, 50));
        }
        return [...models, model("discovered", true)];
      },
    });
  }
};
""", encoding="utf-8")
    target = "dynamic/discovered"
    initial = cli_models(f)
    assert "dynamic/seed" in initial and not {target, "unseeded/discovered"}.intersection(initial)
    assert {r["id"] for r in catalog()["models"]} == initial
    with native(f) as rpc:
        def ids():
            return {r["provider"] + "/" + r["id"] for r in rpc("get_available_models")["models"]}

        assert ids() == initial
        ready.touch()
        wait_until(lambda: {target, "unseeded/discovered"} <= ids(), "Pi did not publish the refreshed catalog")
        rpc("set_model", provider="dynamic", modelId="discovered")
        assert "high" in rpc("get_available_thinking_levels")["levels"]
    ready.unlink()
    # A new Pi process can forward an unlisted model ID from a known provider.
    task("dynamic", ["--model", target], target, "off", None)
    failed_task(f, ["--model", "unseeded/discovered"], "Pi exited before its RPC response")


def failed_task(f, options, error):
    spawn = json.loads(f.run([AXIOPERA, "--json", "spawn", "pi", "--tier", "full", *options, "probe"]))
    task_id = spawn["data"]["task"]
    f.run([AXIOPERA, "wait", task_id, "--timeout", "20s"])
    status = json.loads(f.run([AXIOPERA, "--json", "status", task_id]))["data"][0]
    assert status["status"] == "failed"
    assert error in f.run([AXIOPERA, "log", task_id])


def empty_catalog(f):
    f.env["PI_OFFLINE"] = "1"
    home = Path(f.env["AXIOPERA_HOME"])
    home.mkdir()
    (home / "config.toml").write_text(f"[agents.pi]\npath = {json.dumps(str(f.pi))}\n", encoding="utf-8")
    assert "No models available" in f.run([f.pi, "--list-models"])
    with native(f) as rpc:
        assert not rpc("get_available_models")["models"]
        model = rpc("get_state")["model"]
        assert all(model[k] == "unknown" for k in ("api", "provider", "id"))
    try:
        listing = json.loads(f.run([AXIOPERA, "--json", "models", "pi"]))
        data = listing["data"]
        assert data["models"] == []
        if data.get("native_model") == "unknown/unknown":
            # 0.5.0 displays Pi's placeholder as the default model in an empty profile.
            assert data["native_effort"] == "off"
            assert listing["text"].splitlines()[0] == "Without --model, spawns use unknown/unknown, effort off."
        else:
            assert not data.get("native_model") and not data.get("native_effort")
        print("empty catalog:", listing["text"].replace("\n", " | "), flush=True)
        failed_task(f, [], "No API key")
    finally:
        f.run([AXIOPERA, "daemon", "stop", "--force"])


def exercise(f):
    f.env["PI_OFFLINE"] = "1"
    home = Path(f.env["AXIOPERA_HOME"])
    home.mkdir()
    path_line = f"[agents.pi]\npath = {json.dumps(str(f.pi))}\n"

    def configure(model=None):
        (home / "config.toml").write_text(path_line + (f'model = "{model}"\n' if model else ""), encoding="utf-8")

    configure()
    settings = {"defaultProvider": "fixture", "defaultModel": "research", "defaultThinkingLevel": "medium",
                "modelThinkingLevels": {"fixture/research": "high"}}
    (f.agent / "settings.json").write_text(json.dumps(settings), encoding="utf-8")
    models = [
        {"id": "research", "name": "Research", "reasoning": True,
         "thinkingLevelMap": {"minimal": None, "xhigh": "xhigh", "max": "maximum"}},
        {"id": "draft", "name": "Draft", "reasoning": True},
        {"id": "plain", "name": "Plain", "reasoning": False},
    ]
    for row in models:
        row.update(input=["text"], contextWindow=128000, maxTokens=4096,
                   compat={"supportsReasoningEffort": True})
    seen = []

    def respond(request):
        seen.append((request["model"], request.get("reasoning_effort")))
        return "probe-finished"

    def cli(*args):
        return f.run([AXIOPERA, *args])

    def catalog():
        return json.loads(cli("--json", "models", "pi"))["data"]

    def task(name, options, full_model, effort, wire_effort):
        before = len(seen)
        spawned = json.loads(cli("--json", "spawn", "pi", "--tier", "full", "--name", name, *options, name))
        task_id = spawned["data"]["task"]
        cli("wait", task_id, "--timeout", "30s")
        status = json.loads(cli("--json", "status", task_id))["data"][0]
        assert status["status"] == "idle", (name, status["status"], cli("log", task_id)[-600:])
        assert (status["actual_model"], status["actual_effort"]) == (full_model, effort), name
        assert seen[before:] == [(full_model.split("/", 1)[1], wire_effort)], (name, seen[before:])
        assert f"model {full_model}, effort {effort}" in cli("inbox", task_id), name
        assert "probe-finished" in cli("result", task_id, "--force"), name
        print(name, full_model, effort, wire_effort, flush=True)

    with f.model(respond, models) as url:
        try:
            with native(f) as rpc:
                available = rpc("get_available_models")["models"]
                state = rpc("get_state")
            assert (state["model"]["id"], state["thinkingLevel"]) == ("research", "high")
            listing = catalog()
            assert (listing["native_model"], listing["native_effort"]) == ("fixture/research", "high")
            actual = {r["id"]: (r["name"], r["efforts"]) for r in listing["models"]}
            expected = {}
            for row in available:
                full = row["provider"] + "/" + row["id"]
                with native(f, "--model", full) as rpc:
                    expected[full] = (row["name"], rpc("get_available_thinking_levels")["levels"])
            assert actual == expected and len(actual) == len(models), (actual, expected)
            assert cli_models(f) == set(actual)
            text = cli("models", "pi").splitlines()
            assert text[0] == "Without --model, spawns use fixture/research, effort high."
            for full, (_, levels) in expected.items():
                assert any(line.startswith(full + " ") and "effort: " + ", ".join(levels) in line for line in text)
            assert not seen
            print("native/Pi RPC and Axiopera catalogs agree:", expected, flush=True)

            pty = Pty([str(f.pi), "--no-session", "--approve"], f.cwd, f.env)
            try:
                wait_until(lambda: "research" in pty.screen, "Pi TUI did not show the saved model")
                pty.type("NATIVE-TUI-SAVED")
                wait_until(lambda: "probe-finished" in pty.screen, "Pi TUI did not finish")
                assert seen == [("research", "high")], seen
                for command, confirmation in (("/model fixture/draft", "Model: draft"),
                                              ("/thinking low", "Thinking level: low")):
                    pty.type(command)
                    time.sleep(0.5)
                    if confirmation not in pty.screen:
                        # The completion menu can consume the first Enter.
                        pty.write("\r")
                    wait_until(lambda: confirmation in pty.screen, confirmation)
                pty.type("NATIVE-TUI-SESSION")
                wait_until(lambda: len(seen) == 2, "Pi did not send the second turn")
                assert seen[-1] == ("draft", "low")
                task("saved", [], "fixture/research", "high", "high")
            finally:
                pty.close()
            task("model-only", ["--model", "fixture/draft"], "fixture/draft", "medium", "medium")
            task("effort-only", ["--effort", "low"], "fixture/research", "low", "low")
            task("both", ["--model", "fixture/research", "--effort", "max"], "fixture/research", "max", "maximum")
            task("plain", ["--model", "fixture/plain"], "fixture/plain", "off", None)
            cli("daemon", "stop")
            configure("fixture/draft")
            assert catalog()["configured_model"] == "fixture/draft"
            assert cli("models", "pi").startswith("Without --model, spawns use fixture/draft (Axiopera config).")
            task("configured", [], "fixture/draft", "medium", "medium")
            task("override-config", ["--model", "fixture/research", "--effort", "low"], "fixture/research", "low", "low")
            cli("daemon", "stop")
            configure()
            dynamic_models(f, url, catalog, task)
            assert seen[-1] == ("discovered", None) and seen.count(("discovered", None)) == 1
        finally:
            cli("daemon", "stop", "--force")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="axiopera-pi-models-") as scratch:
        pi = Path(sys.argv[1]).resolve(strict=True)
        exercise(Fixture(Path(scratch) / "configured", pi))
        empty_catalog(Fixture(Path(scratch) / "empty", pi))
