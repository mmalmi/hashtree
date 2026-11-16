#!/usr/bin/env bash
set -euo pipefail
image=$(jq -r .image "fixtures/signed-graph-consumers/$APP.json")
uid=1000
name=iris-direct-test-$APP
id=$(docker image inspect "$image" --format '{{.Id}}')
cp fixtures/signed-graph-consumers/network-proof.py "$STAGE/network-proof.py"
sudo chown -R "$uid:$uid" "$STAGE"
container=$(docker run -d --init --network none --cap-drop ALL --shm-size=1g --cpus=3 --memory=10g --label iris.task=app-fips-isolated --user "$uid:$uid" --name "$name" -v "$STAGE:/stage" -w /stage/app -e APP="$APP" -e CI=1 -e PW_WORKERS=2 -e FIPS_DUMMY=1 -e PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 -e PLAYWRIGHT_BROWSERS_PATH=/ms-playwright "$image" sleep 4800)
trap 'docker rm -f "$container" >/dev/null' EXIT
# A local-only dummy lets browsers exercise online graph sync and WebRTC; there
# is no path to another host. Reuse the accepted independent route and child checks.
sudo python3 fixtures/signed-graph-consumers/namespace-setup.py "$container" "$name" "$id" "$uid" | sudo tee "$STAGE/evidence/namespace.json" >/dev/null
docker exec "$container" python3 /stage/network-proof.py /stage/evidence/network.json
docker exec "$container" bash -ec '
 export PATH=/stage/tools:$PATH
 pnpm install --frozen-lockfile --offline --store-dir /stage/store
 node -p "JSON.stringify({node:process.version,pnpm:require(\"/stage/pnpm/package.json\").version,sdk:require(\"nostr-social-graph/package.json\").version,playwright:require(\"@playwright/test/package.json\").version})" > /stage/evidence/toolchain.json
 node /stage/profile.mjs
 tar -czf /stage/evidence/qualified-dist.tar.gz -C dist .
 sha256sum /stage/evidence/qualified-dist.tar.gz > /stage/evidence/qualified-dist.sha256
 '
