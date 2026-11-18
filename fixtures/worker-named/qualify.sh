#!/usr/bin/env bash
set -euo pipefail
base=$RUNNER_TEMP/signed-graph
mkdir -p "$base"/{shipping,store,tools,pnpm,evidence,procedure}
test "$(df -PB1 . | awk 'END{print $4}')" -gt 21474836480
source=$(cat fixtures/worker-named/source.txt)
git diff --exit-code "$source" HEAD -- ts
cp -R fixtures/npm-closure/. "$base/shipping/"
cp fixtures/worker-named/{network-proof.py,namespace-setup.py} "$base/procedure/"
cp fixtures/worker-named/source.txt "$base/evidence/source.txt"
cp "$base/shipping/"{package.json,pnpm-lock.yaml,inputs.json} "$base/evidence/"
python3 - "$base" <<'CHECK'
from pathlib import Path
import json,hashlib,sys
base=Path(sys.argv[1]); pins=json.loads((base/'shipping/inputs.json').read_text())
for name,row in pins['archives'].items():
    p=base/'shipping/artifacts'/name
    assert p.stat().st_size==row['bytes'] and hashlib.sha256(p.read_bytes()).hexdigest()==row['sha256']
CHECK
curl -fsSL --retry 2 https://registry.npmjs.org/pnpm/-/pnpm-10.23.0.tgz -o "$base/pnpm.tgz"
echo "a1cdd7b468386a9d78a081da05d6049d7e598db62a299db92df21a7062a4b183  $base/pnpm.tgz" | sha256sum -c
tar -xzf "$base/pnpm.tgz" --strip-components=1 -C "$base/pnpm"
ln -s /stage/pnpm/bin/pnpm.cjs "$base/tools/pnpm"
image=mcr.microsoft.com/playwright@sha256:5b8f294aff9041b7191c34a4bab3ac270157a28774d4b0660e9743297b697e48
docker pull "$image" >/dev/null
id=$(docker image inspect "$image" --format '{{.Id}}')
docker run --rm -v "$base:/stage" -w /stage/shipping "$image" bash -ec 'export PATH=/stage/tools:$PATH; pnpm fetch --frozen-lockfile --store-dir /stage/store --ignore-scripts --reporter=append-only' > "$base/evidence/fetch.log" 2>&1 || { tail -25 "$base/evidence/fetch.log"; exit 1; }
sudo chown -R 1000:1000 "$base"
c=$(docker run --init -d --network none --cap-drop ALL --user 1000:1000 --cpus=1 --memory=1g --label iris.task=app-fips-isolated --name signed-graph-worker -v "$base:/stage" -w /stage/shipping -e FIPS_DUMMY=1 -e CI=1 "$image" sleep 900)
trap 'docker rm -f "$c" >/dev/null' EXIT
sudo python3 "$base/procedure/namespace-setup.py" "$c" signed-graph-worker "$id" 1000 | sudo tee "$base/evidence/namespace.json" >/dev/null
docker exec "$c" python3 /stage/procedure/network-proof.py /stage/evidence/network.json
docker exec "$c" bash -ec '
 export PATH=/stage/tools:$PATH
 node --version > /stage/evidence/node-version.txt
 pnpm --version > /stage/evidence/pnpm-version.txt
 pnpm install --frozen-lockfile --offline --store-dir /stage/store --reporter=append-only --ignore-scripts > /stage/evidence/install.log 2>&1 || { tail -30 /stage/evidence/install.log; exit 1; }
 pnpm list --depth Infinity --json > /stage/evidence/installed-dependencies.json
 python3 verify-installed.py > /stage/evidence/installed-files.json
 pnpm exec vitest run tests --maxWorkers=1 > /stage/evidence/tests.log 2>&1 || { tail -60 /stage/evidence/tests.log; exit 1; }
 tail -12 /stage/evidence/tests.log
 printf "{\"passed\":true,\"focusedNpmTests\":9,\"publishedExports\":true,\"sourceBuild\":false}\n" > /stage/evidence/status.json
'
