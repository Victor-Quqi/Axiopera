axiopera: run other coding agents (claude, codex, grok) as your sub-agents.

Use axiopera for a different agent or model, or for work that must continue after your
session ends. For same-agent delegation that needs your full context, use your built-in
sub-agents: axiopera sub-agents see only the task you write.

Dispatch
  axiopera spawn codex "Fix the failing test in tests/parser.rs; run cargo test before finishing."
  axiopera spawn claude --tier read --name review -f task.md
  Tiers: read (look, don't change; MCP: read-only tools only, none on Grok),
         work (default: the agent's own auto mode), full (the agent's full-access mode).
  Options: --model, --effort, --name (use it instead of the ID), --cwd, --timeout 30m,
  --worktree (an isolated git worktree).

Get results
  axiopera inbox           takes what your sub-tasks have for you: results, questions, approval
                           requests and problems. Each is handed over once. Never blocks.
  axiopera wait            blocks until there is something new, lists it, and takes nothing.
@claude Run `axiopera wait` as a background command. When it exits, run `axiopera inbox`, and
@claude start `axiopera wait` again while sub-tasks still run.
@codex The output of spawn says how results reach you.
@grok Before you spawn, start `axiopera watch --caller grok:<session>` with your monitor tool,
@grok persistent, unless it is running; start it again if told it ended while sub-tasks run.
@grok For each line it prints, run `axiopera inbox`.
  Exit codes: 0 success, 1 command error, 3 nothing new yet (or wait timed out),
  4 nothing new and nothing running.

Talk to a running sub-agent
  axiopera send <task> "Also update the docs."    steers the running turn, or starts a new turn
                                                   in the same session if it already finished.
  Results note when they may predate your message; late messages are handled in the next turn.

Answer questions
  Questions arrive like results; sub-agents can keep working until you answer.
  axiopera reply <question> "Use port 8080."      reaches it mid-turn, or resumes it.
  Approval requests pause the sub-agent until you decide; unanswered requests are denied
  after the approval timeout (2 minutes by default) and the sub-agent goes on without it.
  axiopera approve <request>      axiopera deny <request> --reason "Use a dry run instead."

For long messages, use `send <task> -f msg.md` or `reply <question> -f answer.md`; use `-` to read stdin.

Sub-agents can talk to each other: give them each other's task IDs and tell them to run
`axiopera tell <task> "..."` (Codex sub-agents have it as the `tell` tool). You get a copy of
every note.

Inspect and stop
  axiopera status           your sub-tasks       axiopera log <task>      recent events
  axiopera result <task>    latest turn result   axiopera cancel <task>   stop it and its children
  axiopera detach <task>    hand it to the user: prints the native command to continue it
