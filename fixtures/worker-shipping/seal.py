from pathlib import Path
import hashlib, tarfile, json, subprocess, sys
root = Path(sys.argv[1]); source = Path('fixtures/worker-named/source.txt').read_text().strip()
helper = Path('fixtures/worker-shipping/hashtree-nostr-pubsub-0.1.10.tgz')
worker = Path('fixtures/worker-shipping/hashtree-worker-0.4.31.tgz')
assert hashlib.sha256(helper.read_bytes()).hexdigest() == '68c297df922d52cb0ccc12c05e492fd36bff315825fa798d87bb915f2d28d680'
assert hashlib.sha256(worker.read_bytes()).hexdigest() == '8239c09cee24c14f246a6bbae0304b4de6ea2a5ab789d5eb6d2a0c3317075509'
assert subprocess.check_output(['git','diff','--name-only','acbf683f7dbbd6be0276bd3ef1b799d306ce8326',source]).decode().splitlines() == ['ts/packages/hashtree-nostr-pubsub/LICENSE']
with tarfile.open(helper) as archive:
    expected = {}
    for member in archive.getmembers():
        assert member.isfile() and member.mode == 0o644
        assert member.name.startswith('package/') and '..' not in Path(member.name).parts
        data = archive.extractfile(member).read()
        expected[member.name] = {'mode':member.mode,'sha256':hashlib.sha256(data).hexdigest()}
        path = root / member.name; path.parent.mkdir(parents=True,exist_ok=True); path.write_bytes(data)
        if not member.name.startswith('package/dist/'):
            assert data == subprocess.check_output(['git','show',source+':ts/packages/hashtree-nostr-pubsub/'+member.name.removeprefix('package/')])
license = subprocess.check_output(['git','show',source+':ts/packages/hashtree-nostr-pubsub/LICENSE'])
assert license == subprocess.check_output(['git','show',source+':ts/LICENSE'])
(root / 'package/LICENSE').write_bytes(license)
expected['package/LICENSE'] = {'mode':0o644,'sha256':hashlib.sha256(license).hexdigest()}
(root / 'evidence/expected-helper-files.json').write_text(json.dumps(expected,indent=2)+'\n')
(root / 'evidence/hashtree-worker-0.4.31.tgz').write_bytes(worker.read_bytes())
(root / 'evidence/source.txt').write_text(source+'\n')
(root / 'evidence/source.tar').write_bytes(subprocess.check_output(['git','archive',source,'ts']))
(root / 'evidence/reused-qualification.json').write_text(json.dumps({'source':source,'workerBuildSource':'acbf683f7dbbd6be0276bd3ef1b799d306ce8326','workerRun':44337726807,'workerTests':316,'helperBuildSource':'2d49ef4e10fb1f4931b74e8b763a59808df07b59','helperRun':44336032246,'helperTests':28,'change':'Add the exact workspace LICENSE; preserve all 47 qualified helper files and the exact worker archive.'},indent=2)+'\n')
