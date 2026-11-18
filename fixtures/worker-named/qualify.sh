#!/usr/bin/env bash
set -euo pipefail
base=$RUNNER_TEMP/signed-graph
mkdir -p "$base"/{shipping,store,tools,pnpm,evidence,procedure}
test "$(df -PB1 . | awk 'END{print $4}')" -gt 21474836480
source=$(cat fixtures/worker-named/source.txt)
git diff --exit-code "$source" HEAD -- ts
python3 fixtures/worker-shipping/prepare.py "$base"
git archive "$source" ts > "$base/evidence/source.tar"
sha256sum "$base/evidence/source.tar" > "$base/evidence/source.sha256"
git ls-tree -rl "$source" ts > "$base/evidence/source-tree.txt"
cp fixtures/worker-named/{network-proof.py,namespace-setup.py} "$base/procedure/"
curl -fsSL --retry 2 https://registry.npmjs.org/pnpm/-/pnpm-10.23.0.tgz -o "$base/pnpm.tgz"
echo "a1cdd7b468386a9d78a081da05d6049d7e598db62a299db92df21a7062a4b183  $base/pnpm.tgz" | sha256sum -c
tar -xzf "$base/pnpm.tgz" --strip-components=1 -C "$base/pnpm"
ln -s /stage/pnpm/bin/pnpm.cjs "$base/tools/pnpm"
image=mcr.microsoft.com/playwright@sha256:5b8f294aff9041b7191c34a4bab3ac270157a28774d4b0660e9743297b697e48
docker pull "$image" >/dev/null
id=$(docker image inspect "$image" --format '{{.Id}}')
docker run --rm -v "$base:/stage" -w /stage/shipping "$image" bash -ec 'export PATH=/stage/tools:$PATH; pnpm fetch --frozen-lockfile --store-dir /stage/store --ignore-scripts --reporter=append-only' > "$base/evidence/fetch.log" 2>&1 || { tail -25 "$base/evidence/fetch.log"; exit 1; }
sudo chown -R 1000:1000 "$base"
c=$(docker run --init -d --network none --cap-drop ALL --user 1000:1000 --shm-size=1g --cpus=3 --memory=10g --label iris.task=app-fips-isolated --name signed-graph-worker -v "$base:/stage" -w /stage/shipping -e FIPS_DUMMY=1 -e CI=1 -e PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 -e PLAYWRIGHT_BROWSERS_PATH=/ms-playwright "$image" sleep 1800)
trap 'docker rm -f "$c" >/dev/null' EXIT
sudo python3 "$base/procedure/namespace-setup.py" "$c" signed-graph-worker "$id" 1000 | sudo tee "$base/evidence/namespace.json" >/dev/null
docker exec "$c" python3 /stage/procedure/network-proof.py /stage/evidence/network.json
docker exec "$c" bash -ec '
 export PATH=/stage/tools:$PATH
 node --version > /stage/evidence/node-version.txt
 npm --version > /stage/evidence/npm-version.txt
 pnpm --version > /stage/evidence/pnpm-version.txt
 pnpm install --frozen-lockfile --offline --store-dir /stage/store --reporter=append-only --ignore-scripts > /stage/evidence/install.log 2>&1 || { tail -30 /stage/evidence/install.log; exit 1; }
 pnpm --filter @hashtree/worker list --depth 1 --json > /stage/evidence/installed-dependencies.json
 cp worker/src/relay/worker.ts /stage/procedure/current-worker.ts
 cp /stage/procedure/old-worker.ts worker/src/relay/worker.ts
 set +e
 pnpm --filter @hashtree/worker test tests/workerGraphBootstrapBrowser.test.ts -t "packaged discovery usable with root-only-restart" > /stage/evidence/untouched-seed-negative.log 2>&1
 negative_status=$?
 cp /stage/procedure/current-worker.ts worker/src/relay/worker.ts
 set -e
 test "$negative_status" -eq 1
 grep -F "Untouched fallback must not become retained user state" /stage/evidence/untouched-seed-negative.log
 set +e
 pnpm --filter @hashtree/worker build > /stage/evidence/build.log 2>&1
 build_status=$?
 pnpm --filter @hashtree/worker test --maxWorkers=2 > /stage/evidence/tests.log 2>&1
 test_status=$?
 printf "{\"build\":%s,\"tests\":%s}\n" "$build_status" "$test_status" > /stage/evidence/status.json
 tail -90 /stage/evidence/tests.log
 if [ "$build_status" -ne 0 ]; then tail -30 /stage/evidence/build.log; fi
 test "$build_status" -eq 0 && test "$test_status" -eq 0 || exit 1
 set -e
 cd worker
 npm pack --ignore-scripts --pack-destination /stage/evidence > /stage/evidence/pack.log 2>&1
 cp /stage/shipping/artifacts/hashtree-nostr-pubsub-0.1.10.tgz /stage/evidence/
 sha256sum /stage/evidence/hashtree-*.tgz > /stage/evidence/package.sha256
'
