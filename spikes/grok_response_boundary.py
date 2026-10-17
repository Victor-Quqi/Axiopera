"""Reproduce two Grok answers in one prompt after a streaming interjection."""

import queue
import time
import uuid

from probe import GROK, Rpc, clean_env, forget_sessions, log, remove_tree, workdir


def main():
    cwd = workdir("grok-response-boundary")
    rpc = None
    try:
        env = clean_env(**{
            f"GROK_CLAUDE_{kind}_ENABLED": "0"
            for kind in ("MCPS", "HOOKS", "SKILLS", "RULES", "AGENTS")
        })
        rpc = Rpc([GROK, "agent", "--no-leader", "--reasoning-effort", "low", "stdio"], cwd, env=env,
                  on_request=lambda *_: {"outcome": {"outcome": "cancelled"}})
        rpc.call("initialize", {"protocolVersion": 1, "clientCapabilities": {
            "fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False,
        }})
        sid = rpc.call("session/new", {
            "cwd": str(cwd), "mcpServers": [],
            "_meta": {"yoloMode": False, "autoMode": False},
        })["sessionId"]
        reply = rpc.call_async("session/prompt", {
            "sessionId": sid,
            "prompt": [{"type": "text", "text": (
                "Do not use any tools. Output exactly 60 lines, numbered 1 to 60. "
                "Each line must be: N. FIRST-ANSWER. No introduction or conclusion."
            )}],
        })
        log("prompt submitted")
        injected = False
        interjection = None
        answers = []
        current_answer = ""
        old_result = ""
        completed = 0
        prompt_reply = None
        deadline = time.monotonic() + 150
        while time.monotonic() < deadline:
            msg = rpc.next(0.2)
            if msg is None:
                raise RuntimeError("Grok exited before completing the prompt")
            if msg is not ...:
                method = msg.get("method")
                update = msg.get("params", {}).get("update", {})
                kind = update.get("sessionUpdate")
                if kind == "response_completed":
                    answers.append(current_answer)
                    current_answer = ""
                    log("response_completed", method, "text length:", len(answers[-1]))
                elif kind == "agent_message_chunk":
                    text = update.get("content", {}).get("text", "")
                    old_result += text
                    current_answer += text
                    if text and not injected:
                        interjection = rpc.call_async("_x.ai/interject", {
                            "sessionId": sid, "interjectionId": str(uuid.uuid4()),
                            "text": "Replace the requested list with exactly SECOND-ANSWER. Do not use tools.",
                        })
                        injected = True
                        log("interjected after first text chunk")
                elif kind == "tool_call":
                    raise AssertionError("Unexpected tool call in text-only reproduction")
                elif kind == "turn_completed":
                    completed += 1
                    log("turn_completed", update.get("stop_reason"))
            try:
                prompt_reply = reply.get_nowait()
            except queue.Empty:
                pass
            if prompt_reply is not None and rpc.inbox.empty():
                break
        assert prompt_reply is not None, "Prompt timed out"
        assert prompt_reply.get("result", {}).get("stopReason") == "end_turn", prompt_reply
        assert interjection is not None and "error" not in interjection.get(timeout=5)
        log("answer lengths:", [len(answer) for answer in answers])
        log("turn_completed count:", completed)
        assert len(answers) == 2, "Expected both answers within one prompt"
        assert completed == 1, "Expected both answers in the same turn"
        assert "FIRST-ANSWER" in answers[0]
        assert answers[1].strip() == "SECOND-ANSWER", repr(answers[1])
        assert "FIRST-ANSWER" in old_result and "SECOND-ANSWER" in old_result
        log("REPRODUCED: old collector joins both answers; response boundary isolates SECOND-ANSWER")
    finally:
        if rpc is not None:
            rpc.close()
        forget_sessions(cwd)
        remove_tree(cwd)


if __name__ == "__main__":
    main()
