"""Can a process in the Codex sandbox reach a named pipe that only the current user may open?

For each DACL variant, a `winproc serve` pipe takes one connection from a normal process and one
from inside the Codex sandbox (app-server `command/exec`, workspace-write, the same sandbox turns use).
No model calls.
"""

import subprocess
from pathlib import Path

from probe import codex_app_server, log, remove_tree, workdir

WINPROC = str(Path(__file__).parent / "winproc/target/release/winproc.exe")
VARIANTS = ["user", "logon", "restricted", "everyone"]


def sandboxed(rpc, cwd, *argv):
    r = rpc.call("command/exec", {"command": [WINPROC, *argv], "cwd": str(cwd),
                                  "sandboxPolicy": {"type": "workspaceWrite"}, "timeoutMs": 20000})
    return r.get("exitCode"), (r.get("stdout", "") + r.get("stderr", "")).strip()


def main():
    cwd = workdir("ipc-sandbox")
    rpc = codex_app_server(cwd)
    try:
        log("sandbox token:", sandboxed(rpc, cwd, "token"))
        for v in VARIANTS:
            name = f"axiopera-spike-{v}"
            server = subprocess.Popen([WINPROC, "serve", name, v, "2"], stdout=subprocess.PIPE, text=True)
            log(server.stdout.readline().strip())
            normal = subprocess.run([WINPROC, "connect", name], capture_output=True, text=True, timeout=20)
            log(f"[{v}] normal client exit {normal.returncode}:", (normal.stdout + normal.stderr).strip()[:120])
            code, out = sandboxed(rpc, cwd, "connect", name)
            log(f"[{v}] sandboxed client exit {code}:", out[:160])
            if server.poll() is None and code != 0:
                # The sandboxed client never connected; unblock the server's second accept.
                subprocess.run([WINPROC, "connect", name], capture_output=True, timeout=20)
            for line in server.stdout.read().splitlines():
                log(f"[{v}] server:", line[:400])
            server.wait(10)
    finally:
        rpc.close()
        remove_tree(cwd)


if __name__ == "__main__":
    main()
