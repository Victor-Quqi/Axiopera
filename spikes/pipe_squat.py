"""Can a process in the Codex sandbox stand in for the daemon on its named pipe?

A `winproc serve` inside the sandbox (app-server `command/exec`, workspace-write) first creates
the daemon's pipe while no daemon runs, then tries to add an instance next to a running daemon.
Clients must refuse the first, and the second must fail; sandboxed clients must still connect.
No model calls. Build `target/debug/axiopera.exe` and `winproc` first.
"""

import os
import subprocess
import threading
import time
from pathlib import Path

from probe import AXIOPERA, clean_env, codex_app_server, log, remove_tree, workdir

WINPROC = str(Path(__file__).parent / "winproc/target/release/winproc.exe")
PIPES = "\\\\.\\pipe\\"


def fnv1a(data):
    h = 0xCBF29CE484222325
    for b in data:
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def main():
    cwd = workdir("pipe-squat")
    home = cwd / "home"
    whoami = subprocess.run(["whoami", "/user", "/fo", "csv", "/nh"], capture_output=True, text=True)
    sid = whoami.stdout.strip().split(",")[1].strip('"')
    # The daemon's pipe name, derived as in platform::windows::pipe_name.
    name = f"axiopera-{fnv1a(f'{sid}|{str(home).lower()}'.encode()):016x}"
    env = clean_env(AXIOPERA_HOME=str(home))
    # command/exec takes no environment; a script sets the data directory inside the sandbox.
    script = cwd / "axiopera.cmd"
    script.write_text(f'@set "AXIOPERA_HOME={home}"\r\n@"{AXIOPERA}" %*\r\n')

    def axiopera(*args):
        r = subprocess.run([str(AXIOPERA), *args], capture_output=True, text=True, env=env, timeout=30)
        return r.returncode, (r.stdout + r.stderr).strip()

    rpc = codex_app_server(cwd)

    def sandboxed(*argv):
        try:
            r = rpc.call("command/exec", {"command": list(argv), "cwd": str(cwd),
                                          "sandboxPolicy": {"type": "workspaceWrite"}, "timeoutMs": 20000},
                         timeout=60)
            return r.get("exitCode"), (r.get("stdout", "") + r.get("stderr", "")).strip()
        except Exception as e:  # a command the sandbox refuses comes back as an error
            return None, str(e)

    def squatter(count):
        out = {}
        serve = lambda: out.update(r=sandboxed(WINPROC, "serve", name, "user", str(count)))
        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        return thread, out

    def last_line(out):
        return out.get("r", (None, ""))[1].splitlines()[-1:]

    try:
        log("sandbox token restricted:", "restricted=true" in sandboxed(WINPROC, "token")[1])

        # No daemon yet: the client must notice who serves the pipe.
        thread, out = squatter(1)
        for _ in range(50):
            if name in os.listdir(PIPES):
                break
            time.sleep(0.1)
        log("squatter created the pipe:", name in os.listdir(PIPES))
        log("client:", axiopera("daemon", "status"))
        thread.join(30)
        axiopera("daemon", "stop", "--force")
        time.sleep(1)

        # Daemon running: the DACL must keep the sandbox from adding an instance.
        log("daemon:", axiopera("daemon", "status"))
        thread, out = squatter(3)
        time.sleep(3)
        answers = [axiopera("daemon", "status") for _ in range(8)]
        log("daemon answered", sum(c == 0 and o.startswith("daemon ") for c, o in answers), "of 8")
        for _ in range(3):
            if not thread.is_alive():
                break
            # Unblock the accepts of a squatter that did get an instance.
            subprocess.run([WINPROC, "connect", name], capture_output=True, timeout=20)
        thread.join(30)
        log("squatter:", last_line(out))

        # Sandboxed clients still connect, including old ones asking for GENERIC_WRITE.
        log("sandboxed status:", sandboxed("cmd", "/c", str(script), "daemon", "status"))
        log("sandboxed gc:", sandboxed("cmd", "/c", str(script), "gc")[1][-80:])
        log("sandboxed GENERIC_WRITE client exit:", sandboxed(WINPROC, "connect", name)[0])
    finally:
        axiopera("daemon", "stop", "--force")
        rpc.close()
        time.sleep(1)
        remove_tree(cwd)


if __name__ == "__main__":
    main()
