"""Local interoperability fixture using libtorrent's ordinary filesystem storage."""
import json
import pathlib
import sys
import time
import libtorrent as lt

mode, directory, torrent, ready = sys.argv[1:5]
directory = pathlib.Path(directory)
if mode == "create":
    fs = lt.file_storage()
    lt.add_files(fs, str(directory / "bundle"))
    fmt = sys.argv[5]
    flags = {"v1": lt.create_torrent.v1_only, "v2": lt.create_torrent.v2_only, "hybrid": 0}[fmt]
    creator = lt.create_torrent(fs, 32768, flags=flags)
    creator.set_creation_date(0)
    lt.set_piece_hashes(creator, str(directory))
    pathlib.Path(torrent).write_bytes(lt.bencode(creator.generate()))
    sys.exit(0)

settings = {
    "listen_interfaces": "127.0.0.1:0",
    "enable_dht": False,
    "dht_bootstrap_nodes": "",
    "enable_lsd": False,
    "enable_upnp": False,
    "enable_natpmp": False,
    "alert_mask": lt.alert.category_t.error_notification | lt.alert.category_t.status_notification,
}
session = lt.session(settings)
params = lt.load_torrent_file(torrent)
params.save_path = str(directory)
params.flags &= ~(lt.torrent_flags.paused | lt.torrent_flags.auto_managed)
params.trackers = []
params.url_seeds = []
handle = session.add_torrent(params)
if mode == "download":
    handle.connect_peer(("127.0.0.1", int(sys.argv[5])))

deadline = time.monotonic() + 45
announced = False
while time.monotonic() < deadline:
    for alert in session.pop_alerts():
        if isinstance(alert, (lt.file_error_alert, lt.torrent_error_alert)):
            raise RuntimeError(alert.message())
    state = handle.status()
    if mode == "download" and state.is_seeding:
        pathlib.Path(ready).write_text(json.dumps({"complete": True}))
        print(json.dumps({"downloaded": state.total_payload_download}))
        sys.exit(0)
    if mode == "seed" and state.is_seeding and session.listen_port() and not announced:
        pathlib.Path(ready).write_text(json.dumps({"port": session.listen_port()}))
        announced = True
    time.sleep(0.02)
raise TimeoutError("reference client did not finish before deadline")
