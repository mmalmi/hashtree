#!/usr/bin/env bash
set -euo pipefail
mkdir -p "$STAGE"/{app,pnpm,tools,evidence,store}
fix=fixtures/signed-graph-consumers
json="$fix/$APP.json"
image=$(jq -r .image "$json")
url=$(jq -r .pnpm.url "$json")
sha=$(jq -r .pnpm.sha256 "$json")
source_hash=$(jq -r .archiveSha256 "$json")
printf '%s  %s\n' "$source_hash" "$fix/$APP.tar.gz" | sha256sum -c
cp "$json" "$STAGE/evidence/source.json"
cp "$fix/profile.mjs" "$STAGE/profile.mjs"
tar -C "$STAGE/app" -xzf "$fix/$APP.tar.gz"
curl -fsSL --retry 3 "$url" -o "$STAGE/pnpm.tgz"
printf '%s  %s\n' "$sha" "$STAGE/pnpm.tgz" | sha256sum -c
tar -C "$STAGE/pnpm" --strip-components=1 -xzf "$STAGE/pnpm.tgz"
ln -s /stage/pnpm/bin/pnpm.cjs "$STAGE/tools/pnpm"
url=$(jq -r .native.url "$json")
curl -fsSL --retry 3 "$url" -o "$STAGE/htree.tgz"
printf '%s  %s\n' "$(jq -r .native.sha256 "$json")" "$STAGE/htree.tgz" | sha256sum -c
tar -tzf "$STAGE/htree.tgz" | while IFS= read -r entry; do case "${entry##*/}" in htree|git-remote-htree) tar -xzOf "$STAGE/htree.tgz" "$entry" > "$STAGE/tools/${entry##*/}";; esac; done
for bin in htree git-remote-htree; do printf '%s  %s\n' "$(jq -r --arg k "$bin" '.native.executables[$k]' "$json")" "$STAGE/tools/$bin" | sha256sum -c; chmod +x "$STAGE/tools/$bin"; done
url=https://github.com/mmalmi/nostr-social-graph/releases/download/v4.2.0/nostr-social-graph-4.2.0.tgz
curl -fsSL --retry 3 "$url" -o "$STAGE/sdk.tgz"
printf '%s  %s\n' '158b105ecccb6d5049e4d9944f40c59ea66ac504a9539e11948f33c5bad66f03' "$STAGE/sdk.tgz" | sha256sum -c
docker pull "$image"
docker image inspect "$image" --format '{{.Id}}' > "$STAGE/evidence/image.txt"
# Fetch immutable dependencies before disabling all external network access.
docker run --rm --cpus=2 --memory=8g -v "$STAGE:/stage" -w /stage/app "$image" sh -ec 'export PATH=/stage/tools:$PATH; node -v; pnpm --version; pnpm fetch --frozen-lockfile --ignore-scripts --store-dir /stage/store'
