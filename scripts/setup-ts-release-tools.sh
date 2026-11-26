#!/usr/bin/env bash
# Qualified Linux x86_64 tools for the npm workflow. Verify archives before use.
set -euo pipefail
mode="${1:?expected verify or publish}"
[[ "$mode" == verify || "$mode" == publish ]]
[[ "$(uname -s)/$(uname -m)" == Linux/x86_64 ]]
root="${RUNNER_TEMP:?}/hashtree-ts-tools"
mkdir -p "$root/bin" "$root/downloads"
fetch() {
  local name="$1" digest="$2" url="$3"
  curl --fail --location --retry 2 --connect-timeout 10 --max-time 240 \
    --output "$root/downloads/$name" "$url"
  echo "$digest  $root/downloads/$name" | sha256sum --check --strict
}
fetch node.tar.xz 48cc0e21e048790e54c0cf5749c2725005313d652651c40b7a21feb9697aaf75 \
  https://nodejs.org/dist/v24.22.2/node-v24.22.2-linux-x64.tar.xz
tar -xJf "$root/downloads/node.tar.xz" -C "$root"
fetch npm.tgz 5dbb86c71d07a1957f2e90734092dd6a58bdcd9ebc2d8d41ca1c6e6a21d364e1 \
  https://registry.npmjs.org/npm/-/npm-12.0.2.tgz
mkdir -p "$root/npm"
tar -xzf "$root/downloads/npm.tgz" -C "$root/npm"
ln -s "$root/npm/package/bin/npm-cli.js" "$root/bin/npm"
export PATH="$root/bin:$root/node-v24.22.2-linux-x64/bin:$PATH"
printf '%s\n' "$root/node-v24.22.2-linux-x64/bin" "$root/bin" >> "$GITHUB_PATH"
test "$(node --version)" = v24.22.2
test "$(npm --version)" = 12.0.2
[[ "$mode" == verify ]] || exit 0

fetch pnpm.tgz a1cdd7b468386a9d78a081da05d6049d7e598db62a299db92df21a7062a4b183 \
  https://registry.npmjs.org/pnpm/-/pnpm-10.23.0.tgz
mkdir -p "$root/pnpm"
tar -xzf "$root/downloads/pnpm.tgz" -C "$root/pnpm"
ln -s "$root/pnpm/package/bin/pnpm.cjs" "$root/bin/pnpm"
test "$(pnpm --version)" = 10.23.0

for component in rustc cargo rust-std; do
  case "$component" in
    rustc) digest=77171ba2a0345fdf2abc4fedda55d6de078dae7a68527c28be8c77dcc9604bd5 ;;
    cargo) digest=d7674918d28093097614cd9728b6ca60db9ea3038f640f0bd1e9a4188c7568ce ;;
    rust-std) digest=3e58dff2d0b72196b5ea4e90536e174d400de88564a52694686b81e091169933 ;;
  esac
  archive="$component-1.99.0-x86_64-unknown-linux-gnu"
  fetch "$archive.tar.xz" "$digest" "https://static.rust-lang.org/dist/2026-10-01/$archive.tar.xz"
  tar -xJf "$root/downloads/$archive.tar.xz" -C "$root"
  bash "$root/$archive/install.sh" --prefix="$root/rust" --disable-ldconfig
done
printf '%s\n' "$root/rust/bin" >> "$GITHUB_PATH"

# Extract the exact D-Bus headers and shared library without package hooks or
# changing the runner. pkg-config, the C linker, and browser system libraries
# come from the Ubuntu 24.04 hosted image; their versions are recorded below.
fetch libdbus-dev.deb ec30a44d0ae84b15ff5e50c3481b61d21d560626ce52326446135841e4cda1a5 \
  https://archive.ubuntu.com/ubuntu/pool/main/d/dbus/libdbus-1-dev_1.14.10-4ubuntu4.1_amd64.deb
fetch libdbus.deb 5d630480f04b4b442300ce847a3fa705ea4d14d80ba6de91f99b51a4e4953b08 \
  https://archive.ubuntu.com/ubuntu/pool/main/d/dbus/libdbus-1-3_1.14.10-4ubuntu4.1_amd64.deb
mkdir -p "$root/sysroot"
dpkg-deb --extract "$root/downloads/libdbus-dev.deb" "$root/sysroot"
dpkg-deb --extract "$root/downloads/libdbus.deb" "$root/sysroot"
printf '%s\n' \
  "PKG_CONFIG_SYSROOT_DIR=$root/sysroot" \
  "PKG_CONFIG_PATH=$root/sysroot/usr/lib/x86_64-linux-gnu/pkgconfig" \
  "LD_LIBRARY_PATH=$root/sysroot/usr/lib/x86_64-linux-gnu" >> "$GITHUB_ENV"
pkg-config --version
cc --version
ldd --version
dpkg-query -W > "$root/system-packages.txt"

# Playwright 1.61.1 pins Chromium 1228 / 149.0.7827.55. The existing tests use
# its headless shell; no browser or OS package installer executes here.
fetch chromium-headless.zip 410c9407d5de3fea80d9398666be06f2aa09154a3fa7b327dc254e336bb4c4b7 \
  https://storage.googleapis.com/chrome-for-testing-public/149.0.7827.55/linux64/chrome-headless-shell-linux64.zip
browser="$root/browsers/chromium_headless_shell-1228"
mkdir -p "$browser"
unzip -q "$root/downloads/chromium-headless.zip" -d "$browser"
ldd "$browser/chrome-headless-shell-linux64/chrome-headless-shell" > "$root/browser-libraries.txt"
if grep -q 'not found' "$root/browser-libraries.txt"; then
  cat "$root/browser-libraries.txt"
  exit 1
fi
echo "PLAYWRIGHT_BROWSERS_PATH=$root/browsers" >> "$GITHUB_ENV"
echo "CARGO_BUILD_JOBS=2" >> "$GITHUB_ENV"
echo "CARGO_INCREMENTAL=0" >> "$GITHUB_ENV"
