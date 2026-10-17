"""Check message acknowledgements during final output and Codex history read-back.

usage: python message_receipts.py claude|codex|grok
"""
import sys
import time
from pathlib import Path
from probe import (claude_stream, claude_user, codex_app_server, grok_acp,
                   forget_sessions, log, remove_tree, workdir)

TASK = 'Write numbers 1 to 180, one per line. Do not use tools. Include START-RECEIPT in the first line.'
MESSAGES = ['After finishing, reply with RECEIPT-ONE.', 'Also include RECEIPT-TWO in your final reply.']


def main(kind):
    cwd = workdir('message-receipts-' + kind)
    rpc = None
    rollout = None
    sent = False
    ends = 0
    try:
        if kind == 'claude':
            rpc = claude_stream(cwd, '--model', 'haiku', '--permission-mode', 'bypassPermissions',
                                '--no-session-persistence', '--replay-user-messages', '--include-partial-messages')
            rpc.send(claude_user(TASK))
        elif kind == 'codex':
            rpc = codex_app_server(cwd)
            th = rpc.call('thread/start', {'cwd': str(cwd), 'sandbox': 'read-only', 'approvalPolicy': 'never'})
            tid = th['thread']['id']
            rollout = th['thread'].get('path')
            turn = rpc.call('turn/start', {'threadId': tid, 'effort': 'low',
                                          'input': [{'type': 'text', 'text': TASK}]})['turn']['id']
        else:
            rpc = grok_acp(cwd)
            sid = rpc.call('session/new', {'cwd': str(cwd), 'mcpServers': [], '_meta': {'yoloMode': True}})['sessionId']
            rpc.call_async('session/prompt', {'sessionId': sid, 'prompt': [{'type': 'text', 'text': TASK}]})
        deadline = time.monotonic() + 150
        last_end = None
        while time.monotonic() < deadline:
            m = rpc.next(1)
            if m in (None, ...):
                if last_end and time.monotonic() - last_end > 8:
                    break
                continue
            method, p = m.get('method', ''), m.get('params', {})
            u = p.get('update', {})
            item = p.get('item', {})
            tag = None
            if kind == 'claude':
                if m.get('type') == 'user': tag = 'echo:' + str(m.get('message', {}).get('content'))[:110]
                if m.get('type') == 'result': tag = 'end:' + m.get('subtype', '')
                output = m.get('type') == 'stream_event' and m.get('event', {}).get('type') == 'content_block_delta'
            elif kind == 'codex':
                if method in ('item/started', 'item/completed'):
                    tag = method + ':' + item.get('type', '')
                    if item.get('type') == 'userMessage':
                        tag += ':' + str(item.get('content', []))[:110]
                if method == 'turn/completed': tag = 'end:' + p.get('turn', {}).get('status', '')
                output = method == 'item/agentMessage/delta'
            else:
                if method == '_x.ai/session/interjection': tag = 'admitted:' + str(p.get('interjectionId'))
                if u.get('sessionUpdate') in ('response_completed', 'turn_completed'):
                    tag = ('end:' if u['sessionUpdate'] == 'turn_completed' else '') + u['sessionUpdate']
                output = u.get('sessionUpdate') == 'agent_message_chunk'
            if tag:
                log(tag)
                if tag.startswith('end:'):
                    ends += 1
                    last_end = time.monotonic()
            if output and not sent:
                sent = True
                for i, text in enumerate(MESSAGES):
                    if kind == 'claude': rpc.send(claude_user(text))
                    elif kind == 'codex':
                        rpc.call('turn/steer', {'threadId': tid, 'expectedTurnId': turn,
                                               'input': [{'type': 'text', 'text': text}]})
                    else:
                        rpc.call('_x.ai/interject', {'sessionId': sid, 'text': text, 'interjectionId': f'receipt-{i}'})
                    log('submitted', i + 1)
        if kind == 'codex':
            history = rpc.call('thread/turns/list', {'threadId': tid, 'limit': 20,
                                                    'sortDirection': 'desc', 'itemsView': 'full'})
            users = [c.get('text', '') for t in history['data'] for i in t.get('items', [])
                     if i.get('type') == 'userMessage' for c in i.get('content', [])]
            log('read-back start:', TASK in users, 'steers:', [text in users for text in MESSAGES])
            assert TASK in users and all(text in users for text in MESSAGES)
        log('summary:', 'sent', sent, 'turns', ends)
        assert sent and ends > 0
    finally:
        if rpc: rpc.close()
        if rollout: Path(rollout).unlink(missing_ok=True)
        forget_sessions(cwd)
        remove_tree(cwd)


if __name__ == '__main__':
    main(sys.argv[1])
