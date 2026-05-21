#!/usr/bin/env bash
# E2E test for hashtree-updater's AppImage install dispatcher on Linux.
#
# Spins up a linux/arm64 container (squirreldisk's published AppImage is
# arm64 only), installs the published htree CLI from crates.io, stages a
# fake old AppImage, runs `htree update install`, and asserts the file was
# replaced atomically with the executable bit set and ELF magic bytes intact.
#
# Run with:
#   bash rust/scripts/test-update-install-linux.sh
#
# Requires Docker. The first run takes a few minutes while cargo builds
# hashtree-cli inside the container.
set -euo pipefail

PUB_REF="htree://npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fsquirreldisk/latest"
PLATFORM="linux/arm64"
HTREE_VERSION="${HTREE_VERSION:-0.2.45}"

echo "==> Running AppImage swap e2e in $PLATFORM container..."
docker run --rm --platform "$PLATFORM" rust:slim-bookworm bash -c "
set -euo pipefail
echo '==> Installing build deps...'
apt-get update -qq && apt-get install -y -qq pkg-config libssl-dev clang lld file ca-certificates >/dev/null

echo '==> Installing htree CLI ${HTREE_VERSION} from crates.io (this is the slow step)...'
cargo install --quiet --root /usr/local --locked --no-default-features --features cashu,fuse hashtree-cli --version ${HTREE_VERSION} 2>&1 | tail -5 || \
  cargo install --quiet --root /usr/local hashtree-cli --version ${HTREE_VERSION} 2>&1 | tail -5

DEST=/tmp/MyApp.AppImage
echo '==> Staging fake old AppImage...'
printf 'old fake appimage' > \$DEST
chmod 0644 \$DEST
OLD_SIZE=\$(stat -c %s \$DEST)
echo \"  old size=\$OLD_SIZE perms=\$(stat -c %a \$DEST)\"

echo '==> Running htree update install...'
htree update install '${PUB_REF}' --to \$DEST --kind appimage 2>&1 | tail -15

NEW_SIZE=\$(stat -c %s \$DEST)
NEW_PERMS=\$(stat -c %a \$DEST)
NEW_HEAD=\$(head -c 4 \$DEST | od -A n -t x1 | tr -d ' ')

echo ''
echo '==> Results:'
echo \"  size  : \$OLD_SIZE -> \$NEW_SIZE\"
echo \"  perms : 644 -> \$NEW_PERMS\"
echo \"  head  : \$NEW_HEAD (expect 7f454c46 = ELF)\"

[ \"\$NEW_SIZE\" -gt 1000000 ] || { echo 'FAIL: new file too small'; exit 1; }
case \"\$NEW_PERMS\" in *[1357]) : ;; *) echo 'FAIL: new file not executable'; exit 1 ;; esac
[ \"\$NEW_HEAD\" = '7f454c46' ] || { echo 'FAIL: new file does not start with ELF magic'; exit 1; }

echo ''
echo 'PASS'
"
