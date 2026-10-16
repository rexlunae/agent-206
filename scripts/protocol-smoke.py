#!/usr/bin/env python3
"""Exercise the real executable, shared gateway, ACP and MCP. No provider fixtures."""
import json
import os
from pathlib import Path
import selectors
import subprocess
import sys
import tempfile
import tomllib

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/agent206').resolve())

def rpc(process, message, expected):
    process.stdin.write(json.dumps(message) + '\n')
    process.stdin.flush()
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    try:
        while selector.select(10):
            line = process.stdout.readline()
            if not line:
                raise RuntimeError('protocol process closed its output')
            reply = json.loads(line)
            if reply.get('id') == expected:
                assert 'error' not in reply, reply
                return reply['result']
        raise TimeoutError('protocol response did not arrive')
    finally:
        selector.close()

with tempfile.TemporaryDirectory(prefix='a206-smoke-') as root:
    def cli(*args):
        return subprocess.check_output([binary, '--data-dir', root, *args], text=True, timeout=15)
    try:
        thread = tomllib.loads(cli('new', 'Protocol verification'))['Thread']['id']
        assert 'Workspace inspector' in cli('extension', 'agent206.inspector')
        acp = subprocess.Popen([binary, '--data-dir', root, 'acp', thread], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        try:
            init = rpc(acp, {'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':1,'clientCapabilities':{}}}, 1)
            assert init['protocolVersion'] == 1
            session = rpc(acp, {'jsonrpc':'2.0','id':2,'method':'session/new','params':{'cwd':os.getcwd(),'mcpServers':[]}}, 2)
            assert session['sessionId'] == thread
        finally:
            acp.terminate()
            acp.wait(timeout=5)
        mcp = subprocess.Popen([binary, '--data-dir', root, 'mcp', thread], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        try:
            rpc(mcp, {'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'Agent 206 protocol smoke','version':'0.1'}}}, 1)
            mcp.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized'})+'\n')
            mcp.stdin.flush()
            tools = rpc(mcp, {'jsonrpc':'2.0','id':2,'method':'tools/list','params':{}}, 2)
            names = {tool['name'] for tool in tools['tools']}
            assert names == {'thread_state','harness_contract','rename_thread'}, names
            state = rpc(mcp, {'jsonrpc':'2.0','id':3,'method':'tools/call','params':{'name':'thread_state','arguments':{}}}, 3)
            assert thread in state['content'][0]['text']
            renamed = rpc(mcp, {'jsonrpc':'2.0','id':4,'method':'tools/call','params':{'name':'rename_thread','arguments':{'title':'Verified by MCP'}}}, 4)
            assert not renamed.get('isError', False), renamed
            snapshot = tomllib.loads(cli('snapshot'))['Snapshot']
            assert snapshot['threads'][0]['title'] == 'Verified by MCP'
        finally:
            mcp.terminate()
            mcp.wait(timeout=5)
        print('PASS: shared gateway, isolated WASM worker, ACP handshake/session, MCP tools and scoped title editing')
    finally:
        cli('stop-gateway')
