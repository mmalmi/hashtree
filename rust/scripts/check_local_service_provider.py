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
    expected = {
        'nostr-pubsub-fips': ('0.6.0', 'f5774e279e0511b5a94e68704b01eb28fd5ca9b2203ab54bb7727f9eff8f179a'),
        'nostr-pubsub-social-graph': ('0.3.0', 'e9dd2d0a1cdacf1ea254ea05d7f63dd6624ff4ee6dae62c4c4000dd4db8b9666'),
        'nostr-social-graph': ('0.2.0', '83a7c3e6dedf8c0474c9e09938496abde4a9c77cd38ec4832d9b56ee9602b8f8'),
        'nostr-social-memory': ('0.2.0', '39fb16cde2e5bb0e6eeed1c5c8740b01a3c90daa277c30c397715e84aa7a89c9'),
        'nvpn-fips-core': ('0.4.119', 'ba0e1a75a2b7fb5d71e4a602c41d8a1ed163417e56798e2198df1dfe3f73cda2'),
        'nvpn-fips-tcp-endpoint': ('0.2.54', '5a6e31c1319cb5ef6dc754905828436c7a1596a6f4888b16223673330f23bf8e'),
        'nvpn-fips-tcp': ('0.2.6', '299df22559c4af63e7d276fcb90cf000f2f1cd5546c213f17d8860154c1383d7'),
    }
    for name, (version, checksum) in expected.items():
        entries = [p for p in lock['package'] if p['name'] == name]
        assert len(entries) == 1 and entries[0]['version'] == version, name
        assert entries[0]['checksum'] == checksum, name
    receipt['cohort'] = {name: version for name, (version, _) in expected.items()}
    receipt['cohortChecksums'] = {name: checksum for name, (_, checksum) in expected.items()}
    wrappers = {}
    for name in ('git-remote-htree', 'hashtree-cli', 'hashtree-embedded'):
        manifest = tomllib.loads(Path('crates', name, 'Cargo.toml').read_text())
        assert manifest['package']['version'] == '0.4.0', name
        wrappers[name] = manifest
    assert wrappers['git-remote-htree']['features']['default'] == ['lmdb']
    assert wrappers['hashtree-cli']['dependencies']['git-remote-htree']['version'] == '=0.4.0'
    embedded_cli = wrappers['hashtree-embedded']['dependencies']['hashtree-cli']
    assert embedded_cli['version'] == '0.4.0'
    assert embedded_cli['default-features'] is False
    assert embedded_cli['features'] == ['lmdb', 'fips-webrtc']
    receipt['wrappers'] = {name: manifest['package']['version'] for name, manifest in wrappers.items()}
    receipt['embeddedCliFeatures'] = embedded_cli['features']
    command = ['cargo', 'test', '--locked', '-p', 'hashtree-cli', '-p',
               'hashtree-embedded', '-p', 'git-remote-htree', '-p',
               'hashtree-fips-transport', '--features',
               'hashtree-cli/fips-webrtc,hashtree-cli/git-remote-wrapper',
               '--lib', '--test', 'provider_route', '--no-run', '--message-format=json']
    receipt['buildCommand'] = command
    artifacts = run('build', command, output, receipt, 2400)
    selected = {'hashtree_cli': ['fips_transport::'], 'hashtree_fips_transport': [],
                'provider_route': [], 'hashtree_embedded': [],
                'git_remote_htree': ['root_observation', 'standalone_lookup']}
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
        if name == 'hashtree_embedded':
            assert 'host_runtime_serves_initial_keyed_root_from_upstream_blossom: test' in listing
        if name == 'git_remote_htree':
            assert count == 4, 'Changed Nostr adapter boundary test selection'
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
