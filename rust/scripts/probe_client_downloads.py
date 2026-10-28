#!/usr/bin/env python3
"""Compare exact public block routes; retain metrics, never execute fetched data."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

BLOCKS = (
    '8e2fd80abaf5b1bc08937cf8c473fc092cda5c3ea407acfc1129d1057ca1cf0e',
    '4111540f8bd9e8b342d92f1d3640e17cebb23fdc045a5d231fd563bdb4072743',
)
SERVERS = ('https://cdn.iris.to', 'https://upload.iris.to')
parser = argparse.ArgumentParser()
parser.add_argument('--manifest', default='Cargo.toml')
parser.add_argument('--package', default='hashtree-client')
parser.add_argument('--client-first', action='store_true')
args = parser.parse_args()
BLOCKS = tuple(os.environ.get('HAPS_PROBE_BLOCKS', ','.join(BLOCKS)).split(','))
assert all(len(h) == 64 and all(c in '0123456789abcdef' for c in h) for h in BLOCKS)
def client_probe():
    subprocess.run(['cargo', 'run', '--locked', '--manifest-path', args.manifest, '-p', args.package,
                    '--example', 'download_probe'], check=True, timeout=1200)
if args.client_first:
    client_probe()
print(subprocess.check_output(['curl', '--version'], text=True), flush=True)
with tempfile.TemporaryDirectory(prefix='client-http-probe-') as temporary:
    for block in BLOCKS:
        for server in SERVERS:
            for protocol in ('--http1.1', '--http2'):
                # Two transfers in one curl invocation permit connection reuse;
                # num_connects in each result shows whether reuse actually occurred.
                paths = [Path(temporary) / f'body-{i}' for i in range(2)]
                command = ['curl', '--silent', '--show-error', '--max-time', '30',
                           '--max-filesize', str(4 * 1024 * 1024), protocol,
                           '--write-out', '%{json}\n']
                for path in paths:
                    path.unlink(missing_ok=True)
                    command += ['--output', str(path), f'{server}/{block}.bin']
                result = subprocess.run(command, text=True, capture_output=True, timeout=70)
                transfers = []
                for line in result.stdout.splitlines():
                    row = json.loads(line)
                    transfers.append({key: row.get(key) for key in (
                        'response_code', 'http_version', 'size_download', 'time_total',
                        'time_starttransfer', 'time_connect', 'time_appconnect',
                        'num_connects', 'speed_download', 'exitcode', 'errormsg')})
                print(json.dumps(dict(phase='curl', block=block, server=server,
                    requested_protocol=protocol, exit_code=result.returncode,
                    transfers=transfers, stderr=result.stderr,
                    files=[dict(bytes=p.stat().st_size,
                                hash_valid=hashlib.sha256(p.read_bytes()).hexdigest() == block)
                           if p.exists() else None for p in paths])), flush=True)
subprocess.run(['cargo', 'tree', '--locked', '--manifest-path', args.manifest, '-p', args.package, '-e', 'features',
                '-i', 'reqwest@0.12.28'], check=True)
if not args.client_first:
    client_probe()
