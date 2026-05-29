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
