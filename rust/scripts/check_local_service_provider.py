#!/usr/bin/env python3
"""Check the locked daemon/provider cohort using real loopback-only transports."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import time
import tomllib


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(name, command, output, receipt, timeout):
    stdout = output / (name + '.out')
    stderr = output / (name + '.log')
    started = time.monotonic()
    with stdout.open('w') as out, stderr.open('w') as err:
        child = subprocess.Popen(command, stdout=out, stderr=err, start_new_session=True)
        try:
            while child.poll() is None:
                free = shutil.disk_usage('.').free
                receipt['minimumFreeBytes'] = min(receipt.get('minimumFreeBytes', free), free)
                assert free >= 20 * 1024**3, 'Hosted physical disk reserve reached'
                assert time.monotonic() - started < timeout, name + ' deadline'
                assert stdout.stat().st_size + stderr.stat().st_size < 128 * 1024**2, 'Log budget'
                time.sleep(1)
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
            code = child.wait()
    receipt['steps'].append({'name': name, 'exit': code,
                             'seconds': round(time.monotonic() - started, 2)})
    if code:
        for path in [stdout, stderr]:
            print('\n'.join(path.read_text(errors='replace').splitlines()[-35:]))
        raise RuntimeError(f'{name}: exit {code}')
    print(name + ': passed', flush=True)
    return stdout.read_text()


def main(output, receipt):
    assert shutil.disk_usage('.').free >= 30 * 1024**3, 'Insufficient hosted build headroom'
    receipt['source'] = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    receipt['rustc'] = subprocess.check_output(['rustc', '-Vv'], text=True)
    receipt['cargo'] = subprocess.check_output(['cargo', '-V'], text=True).strip()
    receipt['cargoLockSha256'] = digest(Path('Cargo.lock'))
    receipt['systemPackages'] = subprocess.check_output(['dpkg-query', '-W'], text=True)
    receipt['memoryBefore'] = subprocess.check_output(['free', '-b'], text=True)
    lock = tomllib.loads(Path('Cargo.lock').read_text())
    expected = {'nostr-pubsub-fips': '0.5.50', 'nvpn-fips-core': '0.4.118',
                'nvpn-fips-tcp-endpoint': '0.2.53', 'nvpn-fips-tcp': '0.2.6'}
    for name, version in expected.items():
        entries = [p for p in lock['package'] if p['name'] == name]
        assert len(entries) == 1 and entries[0]['version'] == version, name
        if name == 'nostr-pubsub-fips':
            assert entries[0]['checksum'] == '228f2f4bbd5b1b9e1a1d30204734a8bd4385314d0ef58bf0756ddb6c700eeabf'
    receipt['cohort'] = expected
    command = ['cargo', 'test', '--locked', '-p', 'hashtree-cli', '-p',
               'hashtree-fips-transport', '--features', 'hashtree-cli/fips-webrtc',
               '--lib', '--test', 'provider_route', '--no-run', '--message-format=json']
    receipt['buildCommand'] = command
    artifacts = run('build', command, output, receipt, 2400)
    selected = {'hashtree_cli': ['fips_transport::'], 'hashtree_fips_transport': [],
                'provider_route': []}
    binaries = {}
    for line in artifacts.splitlines():
        entry = json.loads(line)
        if entry.get('reason') == 'compiler-artifact' and entry.get('profile', {}).get('test'):
            name = entry['target']['name']
            if name in selected and entry.get('executable'):
                binaries[name] = entry['executable']
    assert binaries.keys() == selected.keys(), 'Required consumer test binary missing'
    receipt['tests'] = {}
    failures = []
    for name, filters in selected.items():
        binary = binaries[name]
        listing = subprocess.check_output([binary, *filters, '--list'], text=True)
        count = sum(line.endswith(': test') for line in listing.splitlines())
        assert count > 0, 'Empty test selection'
        if name == 'hashtree_cli':
            assert 'strict_public_author_receiver_reads_daemon_root_and_revokes_provider: test' in listing
        command = ['sudo', 'unshare', '--net', 'bash', '-ec',
                   'ip link set lo up\nexec setpriv --reuid="$1" --regid="$2" --init-groups "$3" "${@:4}"',
                   'local-service-provider', str(os.getuid()), str(os.getgid()),
                   binary, *filters, '--nocapture', '--test-threads=1']
        try:
            result = run(name, command, output, receipt, 600)
            assert re.search(rf'test result: ok\. {count} passed; 0 failed; 0 ignored;', result)
            receipt['tests'][name] = {'passed': count, 'binarySha256': digest(Path(binary)),
                                      'loopbackOnly': True}
        except RuntimeError as error:
            failures.append(f'{name}: {error}')
    assert receipt['cargoLockSha256'] == digest(Path('Cargo.lock'))
    run('source-unchanged', ['git', 'diff', '--exit-code'], output, receipt, 30)
    assert not failures, '; '.join(failures)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--logs', required=True, type=Path)
    output = parser.parse_args().logs
    output.mkdir(parents=True, exist_ok=False)
    receipt = {'steps': [], 'status': 'running'}
    try:
        main(output, receipt)
        receipt['status'] = 'passed'
    except BaseException as error:
        receipt.update(status='failed', error=str(error))
        raise
    finally:
        (output / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
