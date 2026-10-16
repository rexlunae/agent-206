#!/usr/bin/env python3
"""Verify owned Joshua lifecycle with a mock process serving runtime API metadata."""
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/agent206').resolve())
with tempfile.TemporaryDirectory(prefix='a206-joshua-smoke-') as temporary:
    root = Path(temporary)
    home = root / 'home'
    models = home / 'models'
    models.mkdir(parents=True)
    filename = str(uuid.uuid4())
    (models / filename).write_bytes(b'')
    runtime = root / 'bin'
    runtime.mkdir()
    executable = runtime / 'joshua'
    executable.write_text(f'#!{sys.executable}\n' + '''
import http.server, json, sys, uuid
host, port = sys.argv[sys.argv.index('--addr') + 1].rsplit(':', 1)
identity = uuid.uuid4()
class Api(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != '/v1/models':
            self.send_error(404)
            return
        data = json.dumps({'data': [{'id': str(identity), 'context_length': identity.int % 100000 + 1}]}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
http.server.HTTPServer((host, int(port)), Api).serve_forever()
''')
    executable.chmod(0o700)
    environment = dict(os.environ, HOME=str(home), PATH=str(runtime) + os.pathsep + os.environ.get('PATH', ''))
    state = root / 'state'
    def cli(*arguments):
        return subprocess.check_output([binary, '--data-dir', str(state), *arguments], env=environment, text=True, stderr=subprocess.STDOUT, timeout=20)
    def snapshot():
        return tomllib.loads(cli('snapshot'))['Snapshot']
    try:
        # A separately configured connection must survive every managed lifecycle.
        cli('providers', 'add', 'custom', '--label', 'User configured', '--endpoint', 'http://127.0.0.1:9876/v1')
        user = snapshot()['settings']['providers'][0]
        for _ in range(2):
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0))
                port = probe.getsockname()[1]
            cli('joshua', 'start', str(uuid.uuid4()), filename, '--port', str(port))
            current = snapshot()
            managed = current['managed_joshua_provider']
            assert len(current['settings']['providers']) == 2, current
            for attempt in range(50):
                try:
                    result = tomllib.loads(cli('refresh-models', managed))
                except subprocess.CalledProcessError:
                    time.sleep(0.05)
                    continue
                if 'Catalog' in result:
                    break
                time.sleep(0.05)
            else:
                raise AssertionError('Mock Joshua API never became ready')
            assert len(result['Catalog']['models']) == 1
            owned = next(provider for provider in current['settings']['providers'] if provider['id'] == managed)
            exported = cli('settings')
            edited = exported.replace(owned['base_url'], 'http://127.0.0.1:9876/v1')
            assert edited != exported
            settings_file = root / 'edited.toml'
            settings_file.write_text(edited)
            try:
                cli('settings', '--apply', str(settings_file))
            except subprocess.CalledProcessError as error:
                assert 'stop managed Joshua' in error.output, error.output
            else:
                raise AssertionError('A managed connection was repointed while its child was running')
            assert snapshot()['settings']['providers'] == current['settings']['providers']
            cli('joshua', 'stop')
            stopped = snapshot()
            assert stopped['settings']['providers'] == [user], stopped
            assert not stopped.get('catalogs'), stopped
            assert 'managed_joshua_provider' not in stopped, stopped
        print('PASS: managed Joshua start/discover/stop/restart retires only its own connection and catalog')
    finally:
        cli('joshua', 'stop')
        cli('stop-gateway')
