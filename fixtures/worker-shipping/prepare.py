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
for name in ['shippingWorkerScopes.test.ts', 'shippingRuntimeStore.test.ts']:
    shutil.copyfile(Path('fixtures/worker-shipping') / name, root / 'shipping/worker/tests' / name)
shutil.copyfile('fixtures/worker-shipping/pnpm-lock.yaml', root / 'evidence/shipping-lock.yaml')
shutil.copyfile('fixtures/worker-shipping/archives.json', root / 'evidence/shipping-archives.json')
(root / 'evidence/source.txt').write_text(source + '\n')
