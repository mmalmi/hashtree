"""Production CLI round trip using disposable stores and loopback peers only."""
import os
import pathlib
import re
import signal
import subprocess
import sys
import tempfile
import time

binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix="hashtree-torrent-cli-") as temporary:
    root = pathlib.Path(temporary)
    config = root / "config"
    config.mkdir()
    (config / "config.toml").write_text("[updater]\nauto_check = false\n")
    env = dict(os.environ, HTREE_CONFIG_DIR=str(config), TOKIO_WORKER_THREADS="2")
    seed_store, reader_store = root / "seed", root / "reader"
    data = bytes((i * 17 + i // 127) % 256 for i in range(280_013))
    source = root / "input.bin"
    source.write_bytes(data)

    def command(store, *args, ok=True):
        result = subprocess.run(
            [binary, "--data-dir", str(store), *map(str, args)],
            env=env, capture_output=True, timeout=30,
        )
        if (result.returncode == 0) != ok:
            raise AssertionError(f"{args}: exit {result.returncode}: {result.stderr.decode(errors='replace')}")
        return result.stdout

    added = command(seed_store, "add", source, "--local").decode()
    cid = re.search(r"nhash1[023456789acdefghjklmnpqrstuvwxyz]+", added).group()
    torrent = root / "snapshot.torrent"
    command(seed_store, "torrent", "export", cid, "--name", "payload.bin", "--output", torrent, ok=False)
    assert not torrent.exists(), "private snapshot was exported without permission"
    command(seed_store, "torrent", "export", cid, "--name", "payload.bin", "--output", torrent, "--allow-plaintext")
    original = torrent.read_bytes()
    command(seed_store, "torrent", "export", cid, "--name", "payload.bin", "--output", torrent, "--allow-plaintext", ok=False)
    assert torrent.read_bytes() == original, "existing torrent file was overwritten"
    command(reader_store, "torrent", "inspect", torrent)
    assert not (reader_store / "update-check.json").exists()

    log_path = root / "seed.log"
    with log_path.open("w") as log:
        seeder = subprocess.Popen(
            [binary, "--data-dir", str(seed_store), "torrent", "seed", cid,
             "--name", "payload.bin", "--allow-plaintext"],
            env=env, stdout=subprocess.PIPE, stderr=log,
        )
        try:
            deadline = time.monotonic() + 15
            port = None
            while time.monotonic() < deadline:
                match = re.search(r"Seeding on port (\d+)", log_path.read_text())
                if match:
                    port = match.group(1)
                    break
                assert seeder.poll() is None, log_path.read_text()
                time.sleep(0.025)
            assert port, log_path.read_text()
            peer = f"127.0.0.1:{port}"
            output = root / "range.bin"
            command(reader_store, "torrent", "get", torrent, "payload.bin", "--peer", peer,
                    "--offset", "32760", "--length", "20000", "--output", output)
            assert output.read_bytes() == data[32760:52760]
            command(reader_store, "torrent", "get", torrent, "payload.bin", "--peer", peer,
                    "--offset", "0", "--length", "1", "--output", output, ok=False)
            assert output.read_bytes() == data[32760:52760]
            imported = command(reader_store, "torrent", "import", torrent, "payload.bin", "--peer", peer).decode().strip()
            assert imported == cid, "import must reproduce the normal encrypted hashtree CID"
            assert command(reader_store, "cat", imported) == data
            assert b"No pinned" not in command(reader_store, "pins")
            seeder.send_signal(signal.SIGINT)
            assert seeder.wait(timeout=10) == 0, log_path.read_text()
        finally:
            if seeder.poll() is None:
                seeder.kill()
                seeder.wait()
    print("CLI export, seed, range read, encrypted import and output preservation passed")
