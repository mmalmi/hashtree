#!/usr/bin/env bash
set -euo pipefail
base=$RUNNER_TEMP/signed-graph
mkdir -p "$base"/{evidence,procedure}
test "$(df -PB1 . | awk 'END{print $4}')" -gt 21474836480
source=$(cat fixtures/worker-named/source.txt)
git diff --exit-code "$source" HEAD -- ts
python3 fixtures/worker-shipping/seal.py "$base"
cp fixtures/worker-named/{network-proof.py,namespace-setup.py} "$base/procedure/"
image=mcr.microsoft.com/playwright@sha256:5b8f294aff9041b7191c34a4bab3ac270157a28774d4b0660e9743297b697e48
docker pull "$image" >/dev/null
id=$(docker image inspect "$image" --format '{{.Id}}')
sudo chown -R 1000:1000 "$base"
c=$(docker run --init -d --network none --cap-drop ALL --user 1000:1000 --cpus=1 --memory=1g --label iris.task=app-fips-isolated --name signed-graph-worker -v "$base:/stage" -w /stage/package "$image" sleep 600)
trap 'docker rm -f "$c" >/dev/null' EXIT
sudo python3 "$base/procedure/namespace-setup.py" "$c" signed-graph-worker "$id" 1000 | sudo tee "$base/evidence/namespace.json" >/dev/null
docker exec "$c" python3 /stage/procedure/network-proof.py /stage/evidence/network.json
docker exec "$c" bash -ec 'node --version > /stage/evidence/node-version.txt; npm --version > /stage/evidence/npm-version.txt; npm pack --ignore-scripts --pack-destination /stage/evidence > /stage/evidence/helper-pack.log 2>&1'
python3 - "$base" <<'VERIFY'
from pathlib import Path
import json,tarfile,hashlib,sys
root=Path(sys.argv[1]); expected=json.loads((root/'evidence/expected-helper-files.json').read_text())
archive=root/'evidence/hashtree-nostr-pubsub-0.1.10.tgz'
with tarfile.open(archive) as t:
    members=t.getmembers(); assert all(m.isfile() for m in members)
    actual={m.name:{'mode':m.mode,'sha256':hashlib.sha256(t.extractfile(m).read()).hexdigest()} for m in members}
assert actual==expected
(root/'evidence/package.sha256').write_text(''.join(hashlib.sha256(p.read_bytes()).hexdigest()+'  '+p.name+'\n' for p in sorted((root/'evidence').glob('hashtree-*.tgz'))))
(root/'evidence/packaging.json').write_text(json.dumps({'passed':True,'preservedHelperFiles':47,'addedWorkspaceLicense':True,'helperFiles':len(actual),'helperSha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'helperBytes':archive.stat().st_size},indent=2)+'\n')
print((root/'evidence/packaging.json').read_text())
VERIFY
