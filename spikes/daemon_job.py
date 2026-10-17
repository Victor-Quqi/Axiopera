"""Process lifetime on Windows.

launch: each host agent runs `winproc launch` through its own shell tool, then the host exits.
        Does the heartbeat process keep running, and which launch path did it take?
job:    each agent runs inside a fresh kill-on-close job and starts a long command. Terminating the
        job must leave nothing behind, and the agent's own command spawning must still work.

usage: python daemon_job.py launch|job claude|codex|codex-sandboxed|grok
"""

import json
import subprocess
import sys
import time
from pathlib import Path

from probe import CLAUDE, CODEX, GROK, clean_env, forget_sessions, log, remove_tree, workdir

WINPROC = str(Path(__file__).parent / "winproc/target/release/winproc.exe")


def host_argv(kind, prompt):
    return {
        "claude": [CLAUDE, "-p", prompt, "--model", "haiku", "--permission-mode", "bypassPermissions",
                   "--no-session-persistence"],
        "codex": [CODEX, "exec", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox",
                  "--ephemeral", "-c", 'model_reasoning_effort="low"', prompt],
        "codex-sandboxed": [CODEX, "exec", "--skip-git-repo-check", "-s", "workspace-write", "--ephemeral",
                            "-c", 'model_reasoning_effort="low"', prompt],
        "grok": [GROK, "-p", prompt, "--permission-mode", "bypassPermissions"],
    }[kind]


def read_heartbeat(path):
    try:
        lines = Path(path).read_text().splitlines()
        return lines[0], lines[1]
    except (OSError, IndexError):
        return None, None


def launch(kind, cwd):
    hb = cwd / "heartbeat.txt"
    prompt = f'Run exactly this shell command once and report its output: "{WINPROC}" launch "{hb}" 40'
    t = time.monotonic()
    p = subprocess.run(host_argv(kind, prompt), cwd=cwd, env=clean_env(), capture_output=True, timeout=300,
                       stdin=subprocess.DEVNULL)
    log(f"host exited {p.returncode} after {time.monotonic() - t:.0f}s")
    out = p.stdout.decode(errors="replace")
    log("host said:", " ".join(out.split())[:300])
    header, tick1 = read_heartbeat(hb)
    time.sleep(4)
    _, tick2 = read_heartbeat(hb)
    log("heartbeat process:", header)
    log("ticks 4s apart after host exit:", tick1, "->", tick2)


def marker_processes(marker):
    # The owner tells whether the command ran as the user or as a sandbox account.
    ps = (f"Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -like '*{marker}*' "
          f"-and $_.Name -eq 'python.exe' }} | ForEach-Object {{ [pscustomobject]@{{Name=$_.Name; "
          f"ProcessId=$_.ProcessId; User=(Invoke-CimMethod -InputObject $_ -MethodName GetOwner).User}} }} "
          f"| ConvertTo-Json -Compress")
    out = subprocess.run(["powershell.exe", "-NoProfile", "-Command", ps], capture_output=True, text=True).stdout.strip()
    if not out:
        return []
    items = json.loads(out)
    return [f"{i['Name']}:{i['ProcessId']} as {i['User']}" for i in (items if isinstance(items, list) else [items])]


def job(kind, cwd):
    marker = f"axiopera-marker-{kind}"
    prompt = (f'Run exactly this shell command and wait for it to finish: python -c "import time; time.sleep(300)" {marker}')
    argv = host_argv(kind, prompt)
    cmdline = subprocess.list2cmdline(argv) + f' < NUL > "{cwd / "host.log"}" 2>&1'
    runner = subprocess.Popen([WINPROC, "job-run", "45", f'cmd /c "{cmdline}"'], cwd=cwd, env=clean_env(),
                              stdout=subprocess.PIPE, text=True)
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline and not marker_processes(marker):
        time.sleep(3)
    log("marker processes before kill:", marker_processes(marker))
    for line in runner.communicate(timeout=120)[0].splitlines():
        log("job-run:", line)
    time.sleep(1)
    left = marker_processes(marker)
    log("marker processes after kill:", left or "none")


def main(test, kind):
    cwd = workdir(f"{test}-{kind}")
    try:
        {"launch": launch, "job": job}[test](kind, cwd)
    finally:
        time.sleep(1)
        forget_sessions(cwd)
        remove_tree(cwd)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
