"""Extract exact public source and prepare the old-default negative control only."""
import hashlib,json,os,pathlib,shutil,tarfile
here=pathlib.Path(__file__).resolve().parent
stage=pathlib.Path(os.environ['IRIS_CLI_STAGE']).resolve()
assert not stage.exists(),'Preserve earlier task outputs'
assert shutil.disk_usage(stage.parent).free>12*1024**3,'Hosted entry reserve'
stage.mkdir(mode=0o700);(stage/'evidence').mkdir();(stage/'home').mkdir();(stage/'tmp').mkdir();(stage/'cargo').mkdir()
m=json.loads((here/'manifest.json').read_text());archive=here/'source.tar.gz'
assert hashlib.sha256(archive.read_bytes()).hexdigest()==m['archiveSha256']
with tarfile.open(archive) as tar:
    assert all(not pathlib.PurePosixPath(v.name).is_absolute() and '..' not in pathlib.PurePosixPath(v.name).parts and '.git' not in pathlib.PurePosixPath(v.name).parts for v in tar.getmembers())
    tar.extractall(stage/'source',filter='data')
for name,digest in m['sourceFiles'].items():assert hashlib.sha256((stage/'source'/name).read_bytes()).hexdigest()==digest
shutil.copytree(stage/'source',stage/'negative')
for name,digest in m['negativeControlFiles'].items():
    data=(here/('negative-'+name)).read_bytes();assert hashlib.sha256(data).hexdigest()==digest;(stage/'negative'/name).write_bytes(data)
(stage/'evidence/source-inputs.json').write_text(json.dumps(m,indent=2)+'\n')
print('Verified exact source and old-default negative control; no fixture executed')
