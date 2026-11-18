from pathlib import Path
import subprocess, sys, tarfile, io, shutil
root = Path(sys.argv[1]); source = Path('fixtures/worker-named/source.txt').read_text().strip()
raw = subprocess.check_output(['git', 'archive', source, 'ts/packages/hashtree-worker'])
(root / 'evidence/worker-source.tar').write_bytes(raw)
with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
    for member in archive.getmembers():
        if not member.isfile(): continue
        path = root / 'shipping/worker' / Path(member.name).relative_to('ts/packages/hashtree-worker')
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(archive.extractfile(member).read())
for name in ['package.json', 'pnpm-workspace.yaml', '.npmrc', 'pnpm-lock.yaml']:
    shutil.copyfile(Path('fixtures/worker-shipping') / name, root / 'shipping' / name)
for name in ['shippingRuntimeStore.test.ts']:
    shutil.copyfile(Path('fixtures/worker-shipping') / name, root / 'shipping/worker/tests' / name)
shutil.copyfile('fixtures/worker-shipping/pnpm-lock.yaml', root / 'evidence/shipping-lock.yaml')
shutil.copyfile('fixtures/worker-shipping/archives.json', root / 'evidence/shipping-archives.json')
(root / 'evidence/source.txt').write_text(source + '\n')

import hashlib
helper = Path('fixtures/worker-shipping/hashtree-nostr-pubsub-0.1.10.tgz')
assert hashlib.sha256(helper.read_bytes()).hexdigest() == '68c297df922d52cb0ccc12c05e492fd36bff315825fa798d87bb915f2d28d680'
(root / 'shipping/artifacts').mkdir()
shutil.copyfile(helper, root / 'shipping/artifacts' / helper.name)
shutil.copyfile('fixtures/worker-shipping/candidate-helper.json', root / 'evidence/candidate-helper.json')
old_worker = subprocess.check_output(['git', 'show', '2d49ef4e10fb1f4931b74e8b763a59808df07b59:ts/packages/hashtree-worker/src/relay/worker.ts'])
(root / 'procedure/old-worker.ts').write_bytes(old_worker)
