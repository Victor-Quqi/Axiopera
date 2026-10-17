"""Read-tier MCP checks against real Claude/Grok CLIs.

usage: python spikes/read_mcp.py claude|grok|daemon
Claude options: --default-only, --host-only, --initialize.
Grok option: --status-only (no model call).
The local server only returns fixed strings, including for the destructive-hint tool.
"""

import json
import subprocess
import sys
import time
from pathlib import Path

from probe import (AXIOPERA, CLAUDE, GROK, claude_stream, claude_user, clean_env, forget_sessions,
                   grok_acp, log, remove_tree, text_of, workdir)

TOOLS = [
    {"name": "read", "annotations": {"readOnlyHint": True}},
    {"name": "unmarked"},
    {"name": "destructive", "annotations": {"readOnlyHint": True, "destructiveHint": True}},
]
SERVER = "axiopera_probe"


def serve():
    for line in sys.stdin:
        m = json.loads(line)
        if "id" not in m:
            continue
        method = m["method"]
        if method == "initialize":
            result = {"protocolVersion": m["params"]["protocolVersion"],
                      "capabilities": {"tools": {}}, "serverInfo": {"name": SERVER, "version": "1"}}
        elif method == "tools/list":
            result = {"tools": [{**t, "description": "Return the fixed probe marker.",
                                 "inputSchema": {"type": "object", "properties": {}}} for t in TOOLS]}
        elif method == "tools/call":
            name = m["params"]["name"]
            # A server-side record proves execution independently of the model's report.
            with Path("mcp-calls.txt").open("a", encoding="utf-8") as f:
                f.write(name + "\n")
            result = {"content": [{"type": "text", "text": "probe-ok-" + name}]}
        else:
            result = {}
        print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": result}), flush=True)


def server_command():
    return {"command": sys.executable, "args": [str(Path(__file__).resolve()), "server"]}


def probe_servers(value):
    """Keep only our server; status payloads can contain user server configuration."""
    if isinstance(value, dict):
        if SERVER in (value.get("name"), value.get("id"), value.get("serverName")):
            return [{k: value[k] for k in ("name", "id", "serverName", "status", "tools", "session") if k in value}]
        return [s for v in value.values() for s in probe_servers(v)]
    if isinstance(value, list):
        return [s for v in value for s in probe_servers(v)]
    return []


def shape(value):
    if isinstance(value, dict):
        return {k: shape(v) for k, v in value.items()}
    if isinstance(value, list):
        return [shape(value[0])] if value else []
    return type(value).__name__


def claude(cwd):
    for mode in (("default",) if "--default-only" in sys.argv else ("dontAsk", "default")):
        calls_file = cwd / "mcp-calls.txt"
        calls_file.unlink(missing_ok=True)
        approval_args = [] if "--host-only" in sys.argv else ["--permission-prompt-tool", "stdio"]
        p = claude_stream(cwd, "--model", "haiku", "--permission-mode", mode,
                          "--permission-prompts", "host", *approval_args,
                          "--no-session-persistence",
                          "--disallowedTools", "Edit,Write,NotebookEdit,Agent",
                          "--allowedTools", "Bash(axiopera *)", "--strict-mcp-config",
                          "--mcp-config", json.dumps({"mcpServers": {SERVER: server_command()}}))
        try:
            if "--initialize" in sys.argv:
                p.send({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}})
            p.send(claude_user(
                "Run this permission probe. Attempt each item once, even if an earlier item fails; "
                "do not use alternative tools or retry denied actions. "
                "1. Read sample.txt with Read. 2. Glob *.txt. 3. Grep probe in sample.txt. "
                "4. Bash: cat sample.txt. 5. Bash: axiopera --version. "
                "6. Bash: mkdir blocked-directory. "
                f"7. Call all three MCP tools from {SERVER}: read, unmarked, destructive. "
                "Finish with a short table of outcomes."))
            pending = {}
            end = time.monotonic() + 180
            while time.monotonic() < end:
                m = p.next(1)
                if m is None:
                    raise RuntimeError("Claude exited before result")
                if m is ...:
                    continue
                kind = m.get("type")
                if kind == "assistant":
                    for b in m["message"]["content"]:
                        if b["type"] == "tool_use":
                            log(mode, "tool", b["name"], b["input"])
                elif kind == "user":
                    for b in m["message"]["content"]:
                        if b.get("type") == "tool_result":
                            log(mode, "result", b["tool_use_id"], "error", b.get("is_error", False),
                                str(b.get("content"))[:200])
                elif kind == "control_request":
                    req = m["request"]
                    log(mode, "request", req["subtype"], req.get("tool_name"), "keys", sorted(req))
                    if req.get("tool_name", "").startswith("mcp__"):
                        status_id = "status-" + m["request_id"]
                        pending[status_id] = m
                        p.send({"type": "control_request", "request_id": status_id,
                                "request": {"subtype": "mcp_status"}})
                    else:
                        respond_claude(p, m, False)
                elif kind == "control_response":
                    response = m["response"]
                    original = pending.pop(response["request_id"], None)
                    if original:
                        servers = probe_servers(response.get("response", {}))
                        log(mode, "mcp_status", servers)
                        name = original["request"]["tool_name"]
                        allowed = any(
                            name == f"mcp__{SERVER}__{t['name']}"
                            and t.get("annotations", {}).get("readOnly") is True
                            and t.get("annotations", {}).get("destructive") is not True
                            for s in servers for t in s.get("tools", []))
                        respond_claude(p, original, allowed)
                elif kind == "result":
                    log(mode, "denied", [d["tool_name"] for d in m.get("permission_denials", [])])
                    log(mode, "executed MCP", calls_file.read_text() if calls_file.exists() else "none",
                        "mkdir executed", (cwd / "blocked-directory").exists())
                    break
            else:
                raise TimeoutError("Claude probe")
        finally:
            p.close()


def respond_claude(p, m, allowed):
    behavior = {"behavior": "allow", "updatedInput": m["request"].get("input", {})} if allowed else {
        "behavior": "deny", "message": "Denied by the read-tier probe. Continue with the next item."}
    p.send({"type": "control_response", "response": {
        "subtype": "success", "request_id": m["request_id"], "response": behavior}})


def grok(cwd):
    def reject(_rpc, m):
        log("grok request", m["method"])
        return {"outcome": {"outcome": "selected", "optionId": "reject-once"},
                "_meta": {"followup_message": "Denied by probe; continue."}}

    env = clean_env(**{f"GROK_CLAUDE_{k}_ENABLED": "0" for k in ("MCPS", "HOOKS", "SKILLS", "RULES", "AGENTS")})
    for disabled in ([], [f"{SERVER}__unmarked", f"mcp__{SERVER}__destructive"]):
        calls_file = cwd / "mcp-calls.txt"
        calls_file.unlink(missing_ok=True)
        rpc = grok_acp(cwd, on_request=reject, env=env)
        try:
            sid = rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [
                {"name": SERVER, **server_command(), "env": []}], "_meta": {
                    "yoloMode": False, "autoMode": False, "agentProfile": {
                        "name": "probe", "description": "MCP permission probe", "disallowedTools": disabled}}})["sessionId"]
            status = rpc.call("_x.ai/mcp/list", {})
            log("grok disabled", disabled, "MCP status", probe_servers(status))
            log("grok MCP status shape", shape(status))
            if "--status-only" in sys.argv:
                break
            reply = rpc.call_async("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text":
                f"Use search_tool to find all tools from {SERVER}, then attempt each once with use_tool: "
                "read, unmarked, destructive. All return a fixed string. Continue after failures. "
                "Do not call other tools. Report their results briefly."}]})
            end = time.monotonic() + 120
            while time.monotonic() < end:
                if not reply.empty():
                    log("grok prompt reply", reply.get().get("result", {}))
                    break
                m = rpc.next(1)
                if m in (None, ...):
                    continue
                u = m.get("params", {}).get("update", {})
                if u.get("sessionUpdate") in ("tool_call", "tool_call_update"):
                    log("grok tool", {k: u[k] for k in ("title", "status", "rawInput") if k in u},
                        text_of(u)[:1200])
            else:
                raise TimeoutError("Grok probe")
            log("grok executed MCP", calls_file.read_text() if calls_file.exists() else "none")
            log("grok final MCP status", probe_servers(rpc.call("_x.ai/mcp/list", {})))
        finally:
            rpc.close()


def daemon(cwd):
    home = cwd / "home"
    env = clean_env(AXIOPERA_HOME=str(home), AXIOPERA_TASK_ID="")
    (cwd / ".mcp.json").write_text(json.dumps({"mcpServers": {SERVER: server_command()}}))
    (cwd / ".claude").mkdir()
    (cwd / ".claude/settings.local.json").write_text(json.dumps({"enabledMcpjsonServers": [SERVER]}))

    def cli(*args):
        p = subprocess.run([str(AXIOPERA), *args], cwd=cwd, env=env, capture_output=True,
                           text=True, encoding="utf-8", timeout=30)
        if p.returncode:
            raise RuntimeError(p.stdout + p.stderr)
        return p.stdout.strip()

    try:
        log("spawn", cli("spawn", "claude", "--tier", "read", "--model", "haiku", "--name", "probe",
                         "--timeout", "3m", "--cwd", str(cwd),
                         f"Call these three MCP tools from {SERVER} once each: read, unmarked, destructive. "
                         "Continue after refusals. Then use Bash once to run mkdir blocked-directory. "
                         "Do not retry or use other tools. Report results briefly."))
        end = time.monotonic() + 180
        while time.monotonic() < end:
            records = list(home.glob("tasks/*/task.json"))
            if records:
                task = json.loads(records[0].read_text(encoding="utf-8"))
                if task["status"] in ("idle", "failed", "cancelled"):
                    break
            time.sleep(1)
        else:
            raise TimeoutError("daemon read MCP probe")
        events = [json.loads(line) for line in records[0].with_name("events.jsonl").read_text(encoding="utf-8").splitlines()]
        blocked = [e for e in events if e["kind"] == "blocked"]
        log("daemon blocked", blocked)
        assert not any(e["kind"] == "approval_request" for e in events), "read approval escaped to parent"
        assert len(blocked) == 3, "each refusal should produce exactly one blocked event"
        assert (cwd / "mcp-calls.txt").read_text().splitlines() == ["read"]
        assert not (cwd / "blocked-directory").exists()
        assert task["status"] == "idle"
        log("daemon verified: only read executed; three blocked events; no parent approval")
    finally:
        cli("daemon", "stop", "--force")
        time.sleep(1)


if __name__ == "__main__":
    if sys.argv[1] == "server":
        serve()
    else:
        backend = sys.argv[1]
        log("version", subprocess.check_output([GROK if backend == "grok" else CLAUDE, "--version"],
                                              env=clean_env(), text=True).strip())
        cwd = workdir("read-mcp-" + backend)
        try:
            (cwd / "sample.txt").write_text("probe sample\n", encoding="utf-8")
            {"claude": claude, "grok": grok, "daemon": daemon}[backend](cwd)
        finally:
            forget_sessions(cwd)
            remove_tree(cwd)
