#!/usr/bin/env python3
"""One cold IrisChat install with process metrics; never launch the application."""
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time

repo = Path(sys.argv[1]).resolve()
binary = repo / 'target/debug/haps'
out = Path(os.environ['RUNNER_TEMP']) / 'cold-install-diagnostics'
out.mkdir()
clock_ticks = os.sysconf('SC_CLK_TCK')
page_size = os.sysconf('SC_PAGE_SIZE')


def run(stage, args, env, timeout):
    started = time.monotonic()
    with (out / (stage + '.stdout')).open('w') as stdout, (out / (stage + '.stderr')).open('w') as stderr, (out / (stage + '.metrics.jsonl')).open('w') as metrics:
        child = subprocess.Popen([str(binary), *args], env=env, stdout=stdout, stderr=stderr, start_new_session=True)
        try:
            while child.poll() is None:
                if time.monotonic() - started > timeout:
                    raise TimeoutError(stage + ' exceeded its bound')
                try:
                    fields = Path(f'/proc/{child.pid}/stat').read_text().rsplit(')', 1)[1].split()
                    metrics.write(json.dumps(dict(unix=time.time(), elapsed=time.monotonic()-started,
                        pid=child.pid, cpu_seconds=(int(fields[11])+int(fields[12]))/clock_ticks,
                        rss_bytes=int(fields[21])*page_size)) + '\n')
                    metrics.flush()
                except FileNotFoundError:
                    pass
                time.sleep(0.25)
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
        print(json.dumps(dict(stage=stage, exit=child.returncode, elapsed=time.monotonic()-started)), flush=True)
    text = (out / (stage + '.stdout')).read_text()
    errors = (out / (stage + '.stderr')).read_text()
    print(text, flush=True)
    print(errors, file=sys.stderr, flush=True)
    return child.returncode, text, errors


with tempfile.TemporaryDirectory(prefix='haps-cold-probe-') as temporary:
    root = Path(temporary)
    env = dict(os.environ, HAPS_HOME=str(root/'haps'), XDG_DATA_HOME=str(root/'data'),
               HTREE_CONFIG_DIR=str(root/'hashtree'), HTREE_PREFER_LOCAL_DAEMON='false')
    for key in ('HTREE_LOCAL_DAEMON_ONLY', 'HAPS_NO_DEFAULTS', 'NOSTR_RELAYS'):
        env.pop(key, None)
    publisher = json.loads((repo/'packages/catalog-root.json').read_text())['pubkey']
    code, text, errors = run('search', ['search', 'iris-chat', '--json'], env, 180)
    assert code == 0, 'cold search failed'
    assert any(item['publisher'] == publisher and item['package']['name'] == 'iris-chat' for item in json.loads(text))
    args = ['--non-interactive', 'install', publisher+'/iris-chat', '--json']
    code, text, errors = run('audit', args, env, 180)
    if code:
        message = json.loads(text).get('error', {}).get('message', '')
        if 'requires 1 audit(s) from your social graph; found 0' in message:
            assert not (root/'haps/installed.json').exists()
            code, text, errors = run('payload', args+['--allow-unaudited'], env, 600)
    if code:
        hashes = sorted(set(re.findall(r'Hashtree block ([0-9a-f]{64})', text+errors)))
        if hashes:
            # Raw client diagnostics run first, before curl can warm a route.
            probe_env = dict(os.environ, HAPS_PROBE_BLOCKS=','.join(hashes[:2]))
            subprocess.run([sys.executable, 'scripts/probe_client_downloads.py',
                '--manifest', str(repo/'Cargo.toml'), '--package', 'haps', '--client-first'],
                env=probe_env, check=True, timeout=1500)
        raise SystemExit(code)
    print('Cold IrisChat payload installation succeeded; application not launched.', flush=True)
