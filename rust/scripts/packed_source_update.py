"""Construct only explicit public Git objects and refs, then correct file sizes."""

import json
from pathlib import Path
import re
import tarfile


PACKAGES = [
    ('@hashtree/core', '0.3.2',
     'https://registry.npmjs.org/@hashtree/core/-/core-0.3.2.tgz',
     'ef8a4a7b8b652511776a9f8ba2a140d37c76d28566feb001655a92ac99731a9a'),
    ('@msgpack/msgpack', '3.1.2',
     'https://registry.npmjs.org/@msgpack/msgpack/-/msgpack-3.1.2.tgz',
     '1b3108884ea9f9840b614f4568fb2337fc250a4f7ac0f683578a6db009863d15'),
]


def install_sdk(root, download):
    extracted = 0
    for name, version, url, digest in PACKAGES:
        archive = root / (name.replace('/', '-') + '.tgz')
        download(url, digest, archive)
        assert archive.stat().st_size < 2 * 1024**2
        destination = root / 'sdk/node_modules' / name
        destination.mkdir(parents=True)
        with tarfile.open(archive) as source:
            for member in source.getmembers():
                relative = Path(member.name).relative_to('package')
                assert not relative.is_absolute() and '..' not in relative.parts
                target = destination / relative
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                else:
                    assert member.isfile()
                    extracted += member.size
                    assert extracted < 4 * 1024**2
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(source.extractfile(member).read())
        manifest = json.loads((destination / 'package.json').read_text())
        assert manifest['name'] == name and manifest['version'] == version


def produce_complete_tree(root, tools, producer, source, inputs, run, download,
                          verify, environment, output, receipt):
    tree = root / 'source-tree'
    gitdir = tree / '.git'
    packs = gitdir / 'objects/pack'
    packs.mkdir(parents=True)
    pack = root / 'complete.pack'
    tips = '\n'.join(sorted(set(inputs['refs'].values()))) + '\n'
    run('pack-all-public-tips', ['git', '-C', str(source), '-c', 'pack.threads=1',
                               '-c', 'pack.windowMemory=32m', '-c', 'pack.deltaCacheSize=16m',
                               'pack-objects', '--revs', '--stdout'], producer,
        input_text=tips, output_file=pack)
    packid = run('index-complete-pack', ['git', 'index-pack', '--threads=1', str(pack)],
                 producer).strip()
    assert re.fullmatch('[0-9a-f]{40}', packid)
    for suffix in ['pack', 'idx']:
        (root / ('complete.' + suffix)).rename(packs / ('pack-' + packid + '.' + suffix))
    (gitdir / 'HEAD').write_text('ref: ' + inputs['head'] + '\n')
    for ref, oid in inputs['refs'].items():
        assert ref.startswith(('refs/heads/', 'refs/tags/')) and '..' not in Path(ref).parts
        target = gitdir / ref
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(oid + '\n')
    run('packed-server-info', ['git', '--git-dir', str(gitdir), 'update-server-info'], producer)
    verify(gitdir, producer, 'packed-input')
    for item in tree.rglob('*'):
        assert not item.is_symlink()
        if item.is_file():
            item.chmod(0o444)
    added = run('upload-complete-packed-tree', [str(tools / 'htree'), 'add', '--no-ignore',
                                               str(tree)], producer)
    old_hash = re.search(r'^\s*hash:\s+([0-9a-f]{64})\s*$', added, re.M).group(1)
    old_key = re.search(r'^\s*key:\s+([0-9a-f]{64})\s*$', added, re.M).group(1)
    install_sdk(root, download)
    run('correct-source-file-sizes', ['node', str(Path(__file__).with_name('repair_source_sizes.mjs')),
                                      str(root), old_hash, old_key], producer, timeout=180)
    plan = json.loads((root / 'metadata-plan.json').read_text())
    assert plan['allPayloadCidsPreserved'] and plan['filesChecked'] == len(inputs['refs']) + 5
    (output / 'metadata-plan.json').write_text(json.dumps(plan, indent=2) + '\n')
    reader = environment(root, 'metadata-reader', None, tools)
    for index, node in enumerate(plan['nodes']):
        raw = root / 'corrected-nodes' / (node['hash'] + '.bin')
        uploaded = run('upload-corrected-node-' + str(index),
                       [str(tools / 'htree'), 'add', '--unencrypted', str(raw)], producer, timeout=120)
        assert re.search(r'^\s*hash:\s+' + node['hash'] + r'\s*$', uploaded, re.M)
        checked = root / ('checked-node-' + str(index))
        run('read-corrected-node-' + str(index),
            [str(tools / 'htree'), 'get', node['hash'], '--output', str(checked)], reader, timeout=120)
        assert checked.read_bytes() == raw.read_bytes()
    receipt['explicit_public_pack'] = {'pack': packid, 'tips': len(set(inputs['refs'].values())),
                                       'sdk_packages': PACKAGES, 'metadata': plan}
    return {'hash': plan['hash'], 'key': plan['key']}
