#!/bin/bash
set -euo pipefail
destination="$RUNNER_TEMP/qualified"
mkdir -p "$destination"
git rev-parse HEAD > "$destination/source.txt"
git rev-parse HEAD^ > "$destination/base.txt"
rustc -vV > "$destination/rustc.txt"
cargo -V > "$destination/cargo.txt"
sw_vers > "$destination/platform.txt"
xcodebuild -version >> "$destination/platform.txt"
xcrun --show-sdk-path >> "$destination/platform.txt"
xcrun --show-sdk-version >> "$destination/platform.txt"
shasum -a 256 rust/Cargo.lock rust/crates/git-remote-htree/examples/publish_prebuilt_git_root.rs rust/crates/git-remote-htree/src/nostr_client.rs .github/workflows/qualify-prebuilt-git-root.yml rust/scripts/qualify-prebuilt-git-root.sh > "$destination/source-files.sha256"
cd rust
cargo fetch --locked
export CARGO_NET_OFFLINE=true
profile="$RUNNER_TEMP/no-network.sb"
printf '%s\n' '(version 1)' '(allow default)' '(deny network*)' > "$profile"
python3 - "$profile" <<'PY'
import errno,socket,subprocess,sys
with socket.socket() as listener:
    listener.bind(('127.0.0.1',0))
    listener.listen()
    port=listener.getsockname()[1]
    with socket.create_connection(('127.0.0.1',port),timeout=3):pass
    connection,_=listener.accept()
    connection.close()
    test="""import errno,socket,sys
try: socket.create_connection(('127.0.0.1',int(sys.argv[1])),timeout=3)
except OSError as e:
    assert e.errno in (errno.EPERM, errno.EACCES),e
else: raise RuntimeError('Sandbox allowed an outbound connection')"""
    subprocess.run(['/usr/bin/sandbox-exec','-f',sys.argv[1],sys.executable,'-c',test,str(port)],check=True)
PY
tool=publish_prebuilt_git_root
/usr/bin/sandbox-exec -f "$profile" cargo build --frozen -p git-remote-htree --no-default-features --example "$tool"
cp "target/debug/examples/$tool" "$destination/$tool"
cp "$profile" "$destination/"
cd "$destination"
test "$(lipo -archs "$tool")" = arm64
test -x "$tool"
file "$tool" > executable.txt
stat -f '%Sp' "$tool" >> executable.txt
shasum -a 256 "$tool" > SHA256SUMS
tar -cf prebuilt-git-root-macos-arm64.tar "$tool"
rm "$tool"
