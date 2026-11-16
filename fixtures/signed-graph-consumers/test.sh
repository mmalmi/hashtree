#!/usr/bin/env bash
set -euo pipefail
image=$(jq -r .image "fixtures/signed-graph-consumers/$APP.json")
docker run --rm --init --network none --ipc=host --cpus=3 --memory=10g -v "$STAGE:/stage" -w /stage/app -e APP="$APP" -e CI=1 -e PW_WORKERS=2 -e PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 -e PLAYWRIGHT_BROWSERS_PATH=/ms-playwright "$image" bash -ec '
 export PATH=/stage/tools:$PATH
 test "$(tail -n +3 /proc/net/dev | cut -d: -f1 | xargs)" = lo
 pnpm install --frozen-lockfile --offline --store-dir /stage/store
 node -p "JSON.stringify({node:process.version,pnpm:require(\"/stage/pnpm/package.json\").version,sdk:require(\"nostr-social-graph/package.json\").version,playwright:require(\"@playwright/test/package.json\").version})" > /stage/evidence/toolchain.json
 node /stage/profile.mjs
 tar -czf /stage/evidence/qualified-dist.tar.gz -C dist .
 sha256sum /stage/evidence/qualified-dist.tar.gz > /stage/evidence/qualified-dist.sha256
 '
