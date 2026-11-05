#!/usr/bin/env python3
"""Publish opaque public Git blobs with a disposable signer, then verify closure.

This workflow never receives the repository owner's signing key. The owner binds
the independently verified root locally; a second phase verifies that public ref.
"""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tarfile
import tempfile
import threading
import time
import urllib.request


HERE = Path(__file__).resolve().parent
INPUT = json.loads((HERE / 'upload_sizes_source_input.json').read_text())
ARCHIVE = 'https://github.com/mmalmi/hashtree/releases/download/v0.2.150/hashtree-x86_64-unknown-linux-musl.tar.gz'
ARCHIVE_SHA = 'd309f938ead838fabaf1c072c89d9b844bfdfb7d3a3549f90804109a26f536de'
TOOLS = {
    'htree': 'fb74a19d54c45e12df39b5e79144000963d16959253ef4b071894cc0758f4e13',
    'git-remote-htree': 'ab3177515c89b59c82dd433b43361314a7b6e31c6f493469309257c8019ec21c',
}
WS_URL = 'https://registry.npmjs.org/ws/-/ws-8.21.1.tgz'
WS_SHA = 'bb0f7e58ba1f64746672734d36175fe185f226491e336abc0743e2a8f4472ec1'
PUBLIC_REF = 'htree://npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree'
FLOOR = 20 * 1024**3
OUTPUT = Path(os.environ['RUNNER_TEMP']) / 'source-update-evidence'
OUTPUT.mkdir(exist_ok=False)
receipt = {'source': INPUT['master'], 'expected': INPUT, 'tools': TOOLS, 'steps': []}


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()


def capacity():
    free = shutil.disk_usage(OUTPUT).free
    receipt['minimum_free_bytes'] = min(receipt.get('minimum_free_bytes', free), free)
    if free < FLOOR:
        raise RuntimeError('20 GiB physical free-space reserve reached')


def run(name, command, env, cwd=None, timeout=2100):
    capacity()
    started = time.monotonic()
    out = OUTPUT / f'{name}.out'
    err = OUTPUT / f'{name}.log'
    with out.open('wb') as stdout, err.open('wb') as stderr:
        child = subprocess.Popen(command, env=env, cwd=cwd, stdout=stdout, stderr=stderr,
                                 start_new_session=True)
        try:
            while child.poll() is None:
                capacity()
                if time.monotonic() - started > timeout:
                    raise TimeoutError(name)
                if out.stat().st_size + err.stat().st_size > 16 * 1024**2:
                    raise RuntimeError(f'{name}: log budget exceeded')
                time.sleep(1)
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
            code = child.wait()
    receipt['steps'].append({'name': name, 'exit': code, 'seconds': round(time.monotonic() - started, 2)})
    if code:
        print('\n'.join(err.read_text(errors='replace').splitlines()[-40:]))
        raise RuntimeError(f'{name}: exit {code}')
    print(f'{name}: passed', flush=True)
    return out.read_text()


def download(url, expected, target):
    capacity()
    with urllib.request.urlopen(url, timeout=90) as response, target.open('wb') as output:
        shutil.copyfileobj(response, output)
    if digest(target) != expected:
        raise RuntimeError(f'Input checksum mismatch: {target.name}')


def environment(root, role, relay, tools, writer=False):
    config = root / f'{role}-config'
    config.mkdir()
    relays = [relay] if relay else ['wss://relay.ditto.pub', 'wss://temp.iris.to', 'wss://relay.snort.social']
    (config / 'config.toml').write_text(
        '[server]\nenable_fips = false\nbind_address = "127.0.0.1:0"\n'
        '[nostr]\nrelays = ' + json.dumps(relays) + '\n'
        '[blossom]\nservers = []\nread_servers = ["https://cdn.iris.to", "https://upload.iris.to"]\n'
        'write_servers = ' + json.dumps(['https://upload.iris.to'] if writer else []) + '\n')
    env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'],
               HTREE_CONFIG_DIR=str(config), HTREE_DATA_DIR=str(root / f'{role}-data'),
               HTREE_PREFER_LOCAL_DAEMON='false', HTREE_LOCAL_DAEMON_ONLY='false',
               GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull, GIT_NO_REPLACE_OBJECTS='1',
               GIT_GRAFT_FILE='', GIT_NO_LAZY_FETCH='1', GIT_TERMINAL_PROMPT='0',
               TOKIO_WORKER_THREADS='2', RAYON_NUM_THREADS='2', TMPDIR=str(root))
    for key in ['GIT_CONFIG_COUNT', 'GIT_CONFIG_PARAMETERS', 'GIT_DIR', 'GIT_WORK_TREE',
                'GIT_INDEX_FILE', 'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES',
                'GIT_SHALLOW_FILE', 'NOSTR_RELAYS', 'HTREE_GIT_REBUILD_FROM_LOCAL']:
        env.pop(key, None)
    return env


def verify(repo, env, name):
    def git(*args):
        return subprocess.check_output(['git', '-C', str(repo), *args], env=env, text=True)
    refs = dict(line.split() for line in git('for-each-ref', '--format=%(refname) %(objectname)').splitlines())
    assert refs == INPUT['refs'], 'Public Git ref set changed'
    assert git('symbolic-ref', 'HEAD').strip() == INPUT['head']
    assert git('rev-parse', '--is-shallow-repository').strip() == 'false'
    assert not (repo / 'objects/info/alternates').exists()
    ids = sorted({line.split()[0] for line in git('rev-list', '--objects', '--all').splitlines()})
    encoded = ('\n'.join(ids) + '\n').encode()
    assert len(ids) == INPUT['objects']
    assert hashlib.sha256(encoded).hexdigest() == INPUT['object_ids_sha256']
    child = subprocess.Popen(['git', '-C', str(repo), 'cat-file', '--batch'], env=env,
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    def feed():
        try:
            child.stdin.write(encoded)
            child.stdin.close()
        except BrokenPipeError:
            pass
    thread = threading.Thread(target=feed)
    thread.start()
    result = hashlib.sha256()
    count = 0
    try:
        for block in iter(lambda: child.stdout.read(1024 * 1024), b''):
            capacity()
            result.update(block)
            count += len(block)
        assert child.wait() == 0
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()
        thread.join()
    assert result.hexdigest() == INPUT['payloads_sha256']
    assert count == INPUT['payload_bytes']
    run(name + '-fsck', ['git', '-C', str(repo), 'fsck', '--strict', '--full'], env, timeout=180)
    receipt[name] = {'refs': len(refs), 'objects': len(ids), 'payloads_sha256': result.hexdigest(),
                     'nonshallow': True, 'alternates': False, 'strict_fsck': True}


def main():
    capacity()
    with tempfile.TemporaryDirectory(prefix='htree-source-update-', dir=os.environ['RUNNER_TEMP']) as temporary:
        root = Path(temporary)
        tools = root / 'tools'
        tools.mkdir()
        archive = root / 'tools.tar.gz'
        download(ARCHIVE, ARCHIVE_SHA, archive)
        with tarfile.open(archive) as source:
            for name, expected in TOOLS.items():
                members = [entry for entry in source.getmembers() if Path(entry.name).name == name]
                assert len(members) == 1 and members[0].isfile() and members[0].size < 256 * 1024**2
                with source.extractfile(members[0]) as data, (tools / name).open('wb') as output:
                    shutil.copyfileobj(data, output)
                assert digest(tools / name) == expected
                (tools / name).chmod(0o700)
        publication = INPUT.get('publication')
        if publication:
            env = environment(root, 'public-reader', None, tools)
            info = run('resolve-public-root', [str(tools / 'htree'), 'info', PUBLIC_REF], env, timeout=180)
            assert publication['nhash'] in info, 'Signed public root differs from the qualified root'
            repo = root / 'public.git'
            run('public-cold-clone', ['git', 'clone', '--mirror', PUBLIC_REF, str(repo)], env, cwd=root)
            verify(repo, env, 'public-reader')
            receipt['publication'] = publication
            return

        archive = root / 'ws.tar.gz'
        download(WS_URL, WS_SHA, archive)
        ws = root / 'ws'
        ws.mkdir()
        with tarfile.open(archive) as source:
            for member in source.getmembers():
                relative = Path(member.name).relative_to('package')
                assert '..' not in relative.parts and not relative.is_absolute()
                target = ws / relative
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                else:
                    assert member.isfile()
                    target.parent.mkdir(parents=True, exist_ok=True)
                    with source.extractfile(member) as data, target.open('wb') as output:
                        shutil.copyfileobj(data, output)
        relay_log = (OUTPUT / 'relay.log').open('w')
        relay = subprocess.Popen(['node', str(HERE / 'source_update_relay.mjs'), str(root)],
                                 stdout=relay_log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 15
            while not (root / 'relay.json').exists():
                assert relay.poll() is None and time.monotonic() < deadline, 'Loopback relay did not start'
                time.sleep(.1)
            url = json.loads((root / 'relay.json').read_text())['url']
            producer = environment(root, 'producer', url, tools, writer=True)
            identity = run('disposable-identity', [str(tools / 'htree'), 'user'], producer, timeout=30)
            npub = re.search(r'npub1[a-z0-9]+', identity).group(0)
            # Reuse the previously accepted complete public tree. Publishing into
            # an empty namespace with the old helper can omit historical objects.
            # This announcement is signed only by the disposable fixture identity
            # and stays on the loopback relay. It does not change the real source.
            seed = INPUT['seed_root']
            assert seed['refs']['refs/heads/master'] == INPUT['previous_master']
            assert re.fullmatch('[0-9a-f]{64}', seed['hash'])
            assert re.fullmatch('[0-9a-f]{64}', seed['key'])
            run('seed-accepted-public-tree', [str(tools / 'htree'), 'publish', 'hashtree',
                                            seed['hash'], '--key', seed['key']], producer, timeout=180)
            advertised = run('verify-seed-refs', ['git', 'ls-remote', '--symref',
                                                 f'htree://{npub}/hashtree'], producer, cwd=root, timeout=180)
            seed_refs = {}
            for line in advertised.splitlines():
                value, ref = line.split('\t')
                if ref == 'HEAD':
                    if value.startswith('ref: '):
                        assert value[5:] == seed['head'], 'Accepted seed HEAD binding changed'
                    else:
                        assert value == seed['refs'][seed['head']], 'Accepted seed HEAD target changed'
                elif not ref.endswith('^{}'):
                    seed_refs[ref] = value
            assert seed_refs == seed['refs'], 'Accepted seed ref binding changed'
            receipt['seed_refs_verified'] = True
            source = root / 'input.git'
            run('init-input', ['git', 'init', '--bare', str(source)], producer, timeout=30)
            refspecs = [f'+{value}:{name}' for name, value in INPUT['refs'].items()]
            run('fetch-public-inputs', ['git', '-C', str(source), 'fetch', '--no-tags', '--no-write-fetch-head',
                                       'https://github.com/mmalmi/hashtree.git', *refspecs], producer)
            run('input-head', ['git', '-C', str(source), 'symbolic-ref', 'HEAD', INPUT['head']], producer, timeout=30)
            verify(source, producer, 'input')
            run('produce-public-blobs', ['git', '-C', str(source), 'push', '--mirror', 'htree://self/hashtree'], producer)
            events = json.loads((root / 'events.json').read_text())
            roots = [event for event in events if event['kind'] == 30064 and ['d', 'hashtree'] in event['tags']]
            (OUTPUT / 'root-events.json').write_text(json.dumps(roots, indent=2) + '\n')
            # The ordinary helper publishes after each pushed ref. Only the final
            # root may contain the entire requested ref set; the cold clone below
            # must prove that set and its complete object closure.
            assert roots, 'No source root announcement was captured'
            latest = max(event['created_at'] for event in roots)
            final = [event for event in roots if event['created_at'] == latest]
            bindings = {tuple((tag[0], tag[1]) for tag in event['tags']
                              if len(tag) > 1 and tag[0] in ('hash', 'key')) for event in final}
            assert len(bindings) == 1, 'Ambiguous final source roots at the same timestamp'
            event = min(final, key=lambda event: event['id'])
            receipt['incremental_root_announcements'] = len(roots)
            tags = {tag[0]: tag[1] for tag in event['tags'] if len(tag) > 1}
            assert re.fullmatch('[0-9a-f]{64}', tags['hash'])
            assert re.fullmatch('[0-9a-f]{64}', tags['key']), 'Preserve the accepted legacy key format'
            assert event['content'] == tags['hash']
            serialized = json.dumps([0, event['pubkey'], event['created_at'], event['kind'],
                                     event['tags'], event['content']], separators=(',', ':'), ensure_ascii=False)
            assert hashlib.sha256(serialized.encode()).hexdigest() == event['id']
            reader = environment(root, 'private-reader', url, tools)
            repo = root / 'verified.git'
            run('private-cold-clone', ['git', 'clone', '--mirror', f'htree://{npub}/hashtree', str(repo)], reader, cwd=root)
            verify(repo, reader, 'private-reader')
            info = run('private-root-info', [str(tools / 'htree'), 'info', f'htree://{npub}/hashtree'], reader, timeout=180)
            nhash = re.search(r'nhash1[a-z0-9]+', info).group(0)
            receipt['root'] = {'hash': tags['hash'], 'key': tags['key'], 'nhash': nhash}
            receipt['private_fixture_event'] = event
            receipt['real_signing_identity_used'] = False
            receipt['public_mutable_ref_published'] = False
        finally:
            relay.terminate()
            relay.wait(timeout=10)
            relay_log.close()


try:
    main()
    receipt['status'] = 'passed'
except BaseException as error:
    receipt['status'] = 'failed'
    receipt['error'] = str(error)
    raise
finally:
    (OUTPUT / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
