import pathlib,tarfile,hashlib,json
archive=pathlib.Path('/stage/nostr-pubsub-0.5.42.tgz')
assert hashlib.sha256(archive.read_bytes()).hexdigest()=='8181645e74f3e5971a7bef55cceab24c0cee882b0a8eacc9c89657a03729a523'
packages=list(pathlib.Path('node_modules/.pnpm').glob('*/node_modules/nostr-pubsub/package.json'))
assert len(packages)==1, 'Expected exactly one verified nostr-pubsub package'
for p in packages:
 assert json.loads(p.read_text())['version']=='0.5.42'
 with tarfile.open(archive) as t:
  for member in t.getmembers():
   if member.isfile(): assert (p.parent/pathlib.PurePosixPath(member.name).relative_to('package')).read_bytes()==t.extractfile(member).read(), member.name
print('Verified installed nostr-pubsub 0.5.42 against all archive files')
