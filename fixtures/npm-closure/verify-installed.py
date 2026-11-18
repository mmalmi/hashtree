from pathlib import Path
import hashlib,json,tarfile

root=Path.cwd()
rows=[]
for name,filename,count in [('@hashtree/worker','hashtree-worker-0.4.31.tgz',253),('@hashtree/nostr-pubsub','hashtree-nostr-pubsub-0.1.10.tgz',48)]:
    folder=(root/'node_modules'/name).resolve()
    archive=root/'artifacts'/filename
    with tarfile.open(archive) as t:
        members=t.getmembers()
        assert len(members)==count and len({m.name for m in members})==count
        assert all(m.isfile() and m.mode==0o644 for m in members)
        expected={m.name.removeprefix('package/') for m in members}
        actual={str(p.relative_to(folder)) for p in folder.rglob('*') if p.is_file()}
        assert actual==expected
        for m in members:
            path=folder/m.name.removeprefix('package/')
            assert path.read_bytes()==t.extractfile(m).read()
            assert path.stat().st_mode & 0o777 == 0o644
    rows.append({'package':name,'files':count,'allBytesAndModesMatch':True,'archiveSha256':hashlib.sha256(archive.read_bytes()).hexdigest()})
print(json.dumps(rows,indent=2))
