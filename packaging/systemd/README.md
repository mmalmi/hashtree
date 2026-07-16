# Systemd deployment snippets

These files document production systemd pieces that are not part of the
cross-platform release artifacts.

## Mounted state directory ownership

If `HTREE_DATA_DIR` is an external mount, systemd can start the daemon after a
reboot with the mount root owned by `root:root`. The daemon usually runs as the
unprivileged `hashtree` user, so that leaves the store unreadable and causes a
rapid restart loop with `Permission denied (os error 13)`.

Install `hashtree.service.d/state-dir-permissions.conf` next to the service
unit on hosts that use `/srv/hashtree/state` as the mounted store:

```bash
sudo install -d /etc/systemd/system/hashtree.service.d
sudo install -m 0644 packaging/systemd/hashtree.service.d/state-dir-permissions.conf \
  /etc/systemd/system/hashtree.service.d/state-dir-permissions.conf
sudo systemctl daemon-reload
sudo systemctl restart hashtree.service
```

The `ExecStartPre=+` prefix is intentional: it runs the directory repair as
root even though the service itself uses `User=hashtree`.

## Resumable LMDB-to-pool migration

`hashtree-pool-migrate@.service` runs one hash-verified migration pass as the
unprivileged service user. It is a temporary operational tool, not a mandatory
storage daemon. Install the branch/release binary under the distinct
`/usr/local/bin/htree-pool-migration` name so the running daemon can remain on its
existing binary during the online copy:

```bash
sudo install -m 0755 target/release/htree /usr/local/bin/htree-pool-migration
sudo install -m 0644 packaging/systemd/hashtree-pool-migrate@.service \
  /etc/systemd/system/hashtree-pool-migrate@.service
sudo install -d -o hashtree -g hashtree -m 0750 /var/lib/hashtree/pool-migration
```

Create one `/etc/hashtree/pool-migrate-NAME.env` per source:

```text
HTREE_POOL_TARGET_DATA_DIR=/var/lib/hashtree
HTREE_POOL_SOURCE_LMDB_DIR=/mnt/old-store/blobs
HTREE_POOL_SOURCE_EXTERNAL_DIR=/mnt/old-store/blob-files-v1
HTREE_POOL_STATE_FILE=/var/lib/hashtree/pool-migration/old-store.cursor
HTREE_POOL_BATCH_SIZE=256
```

Then run and inspect the source independently:

```bash
sudo systemctl daemon-reload
sudo systemctl start hashtree-pool-migrate@NAME.service
systemctl status hashtree-pool-migrate@NAME.service
journalctl -u hashtree-pool-migrate@NAME.service
```

The cursor advances only after a verified destination batch commits. Replaying a
batch is idempotent. A completed cursor causes `--resume` to begin another full
pass, because an online writer can add a hash before an earlier cursor. Stop
source writes and run a final stopped-write pass before cutover, then fully verify
the destination. Remove the migration units, environment files, cursors, and
separate migration binary after the rollback-retention period.
