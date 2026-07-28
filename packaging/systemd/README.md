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

## Resumable LMDB-to-Pool migration

`hashtree-pool-migrate@.service` runs one hash-verified migration pass as the
unprivileged `hashtree` user. It is deliberately not a restartable daemon.
Every invocation is inert until a root controller publishes an exact v3
request and htree durably acknowledges it before opening either LMDB store.

Install an immutable migration binary and the exact unit fragment:

```bash
sudo install -o root -g root -m 0555 target/release/htree \
  /usr/local/bin/htree-pool-migration
sudo install -o root -g root -m 0644 \
  packaging/systemd/hashtree-pool-migrate@.service \
  /etc/systemd/system/hashtree-pool-migrate@.service
sudo install -d -o hashtree -g hashtree -m 0750 \
  /var/lib/hashtree/pool-migration/cursors
```

The loaded unit must have no drop-ins, conditions, start/stop helpers, reload
helpers, restart, or control process. It must retain `Type=oneshot`,
`Restart=no`, `TimeoutStartSec=infinity`, `PrivateNetwork=true`,
`NoNewPrivileges=true`, the loader-variable `UnsetEnvironment` list, and one
direct `ExecStart`. The binary, fragment, and environment file must be
root-owned and not group/world writable. A daemon reload is mandatory after
installing them. Every queried scalar or environment property must be present;
omission is not treated as an empty or safe value. systemd 255 suppresses
empty `Exec*` arrays even when queried explicitly, so omitted forbidden hook
arrays are accepted only after the launcher verifies the exact root-owned
fragment, empty `DropInPaths`, and `NeedDaemonReload=no`; any emitted nonempty
hook is rejected.

### Strict environment file

Create `/etc/hashtree/pool-migrate-NAME.env` as a root-owned `0644` file.
Only the keys understood by the v3 validator are legal:

```text
HTREE_POOL_TARGET_DATA_DIR=/var/lib/hashtree
HTREE_POOL_LAUNCH_REQUEST=/var/lib/hashtree/pool-migration/ROLLOUT/attempts-v3/NONCE/launch-request.json
HTREE_POOL_LAUNCH_WAIT_SECONDS=120
HTREE_POOL_SOURCE_LMDB_DIR=/mnt/old-store/blobs
HTREE_POOL_SOURCE_EXTERNAL_ARGS=--source-external-dir /mnt/old-store/blob-files-v1
HTREE_POOL_STATE_FILE=/var/lib/hashtree/pool-migration/cursors/old-store.cursor
HTREE_POOL_BATCH_SIZE=256
HTREE_POOL_MAX_BUFFER_MIB=64
HTREE_POOL_SOURCE_READ_CONCURRENCY=4
HTREE_POOL_REOPEN_BATCHES=256
HTREE_POOL_LIMIT_ARGS=--max-items 100000
```

Use an empty `HTREE_POOL_SOURCE_EXTERNAL_ARGS=` when the source has no
external directory. `HTREE_POOL_LIMIT_ARGS` is exactly `--max-items N` for an
`online-bounded` pass and is empty for a `final-stopped-full` pass. Unknown,
duplicate, quoted, or loader-affecting assignments are rejected. Htree binds
the file path, inode, bytes, SHA-256, systemd-loaded path, and resulting
process environment.

### Single-use attempt directories

The controller creates a fresh 64-lowercase-hex nonce for every process:

```text
/absolute/ROLLOUT/attempts-v3/NONCE/
```

Linux ownership is exact:

```bash
sudo install -d -o root -g root -m 0755 /absolute/ROLLOUT/attempts-v3
sudo install -d -o root -g hashtree -m 1770 \
  /absolute/ROLLOUT/attempts-v3/NONCE
```

After CLI parsing and the immutable migration arguments/environment pass their
local preflight, the service immediately publishes a durable O_EXCL
`launch-started.json`. Its existence consumes the nonce even when the request
never arrives, controller authority validation fails, or the process dies.
The attempt must begin without `launch-started.json`, `launch-request.json`,
`launch-ack.json`, or `terminal-audit.json`. Never remove a claim to retry;
allocate a new nonce.

The root controller serializes the request to a temporary file, fsyncs it,
sets ownership `root:hashtree` and mode `0640`, atomically renames it to
`launch-request.json`, then fsyncs the attempt directory. The service writes
and fsyncs an unnamed file, links it O_EXCL as `launch-ack.json`, fsyncs the
directory, revalidates its inode/content, and only then opens the source or
Pool. A visible acknowledgement is therefore complete, never a partial JSON
write.

### Request and topology schemas

All paths are absolute canonical paths. Every `device`/`inode` pair is the
value captured by the controller from the exact object. `argv` includes every
expanded argument exactly as `/proc/MAINPID/cmdline` exposes it.

```json
{
  "schema": "hashtree-pool-migration-launch-request/v3",
  "attemptNamespace": "/absolute/ROLLOUT/attempts-v3",
  "attemptNamespaceIdentity": {"device": 1, "inode": 2},
  "attemptIdentity": {"device": 1, "inode": 3},
  "nonce": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "bootId": "canonical-lowercase-boot-uuid",
  "systemdInvocationId": "0123456789abcdef0123456789abcdef",
  "systemdUnit": "hashtree-pool-migrate@NAME.service",
  "systemdManager": "system",
  "systemdFragment": {
    "path": "/etc/systemd/system/hashtree-pool-migrate@.service",
    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
  },
  "systemdEnvironmentFile": {
    "path": "/etc/hashtree/pool-migrate-NAME.env",
    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
  },
  "mainPid": 1234,
  "procStartTimeTicks": 5678,
  "binary": {
    "path": "/usr/local/bin/htree-pool-migration",
    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
  },
  "argv": [
    "/usr/local/bin/htree-pool-migration",
    "--data-dir", "/var/lib/hashtree",
    "storage", "pool", "migrate-lmdb",
    "--launch-request", "/absolute/ROLLOUT/attempts-v3/NONCE/launch-request.json",
    "--launch-request-wait-seconds", "120",
    "--source", "/mnt/old-store/blobs",
    "--source-external-dir", "/mnt/old-store/blob-files-v1",
    "--state-file", "/var/lib/hashtree/pool-migration/cursors/old-store.cursor",
    "--batch-size", "256",
    "--max-buffer-mib", "64",
    "--source-read-concurrency", "4",
    "--reopen-batches", "256",
    "--max-items", "100000",
    "--resume"
  ],
  "controller": {
    "rolloutId": "ROLLOUT",
    "phase": "online-bounded",
    "executable": {
      "path": "/absolute/controller",
      "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
    },
    "state": {
      "path": "/absolute/ROLLOUT/state.json",
      "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
    }
  },
  "source": {
    "lmdbPath": "/mnt/old-store/blobs",
    "lmdbIdentity": {
      "directory": {"device": 2, "inode": 10},
      "data": {"device": 2, "inode": 11},
      "lock": {"device": 2, "inode": 12}
    },
    "externalPath": "/mnt/old-store/blob-files-v1",
    "externalIdentity": {"device": 2, "inode": 13},
    "baseline": {
      "path": "/absolute/ROLLOUT/source-baseline.manifest",
      "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
    }
  },
  "pool": {
    "path": "/var/lib/hashtree/blob-pool-v1",
    "lmdbIdentity": {
      "directory": {"device": 3, "inode": 20},
      "data": {"device": 3, "inode": 21},
      "lock": {"device": 3, "inode": 22}
    },
    "topology": {
      "path": "/absolute/ROLLOUT/pool-topology.json",
      "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
    }
  },
  "cursor": {
    "path": "/var/lib/hashtree/pool-migration/cursors/old-store.cursor",
    "parentIdentity": {"device": 3, "inode": 30},
    "exists": false,
    "value": null,
    "sha256": null
  },
  "cas": [{
    "label": "controller-safety-cas",
    "path": "/absolute/ROLLOUT/safety.cas",
    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
  }]
}
```

`externalPath` and `externalIdentity` are both JSON `null` when absent. A
present cursor has a lowercase 64-hex `value` and the SHA-256 of its exact
newline-terminated file. `complete` is terminal and never launchable.

The controller state is a separate root-owned, non-group/world-writable,
deny-unknown-fields CAS. It binds the same rollout, boot, source and target
LMDB identities, and full Pool manifest as the request:

```json
{
  "schema": "hashtree-pool-migration-controller-state/v3",
  "rolloutId": "ROLLOUT",
  "phase": "final-stopped-full",
  "bootId": "canonical-lowercase-boot-uuid",
  "sourceLmdbIdentity": {
    "directory": {"device": 2, "inode": 10},
    "data": {"device": 2, "inode": 11},
    "lock": {"device": 2, "inode": 12}
  },
  "sourceExternalIdentity": {"device": 2, "inode": 13},
  "poolLmdbIdentity": {
    "directory": {"device": 3, "inode": 20},
    "data": {"device": 3, "inode": 21},
    "lock": {"device": 3, "inode": 22}
  },
  "poolManifestSha256": "0000000000000000000000000000000000000000000000000000000000000000",
  "poolTopologySha256": "0000000000000000000000000000000000000000000000000000000000000000",
  "sourceWritersFenced": true,
  "targetWritersFenced": true,
  "fenceHeldUntilCompletion": true,
  "sourceWriterProcessesWithOpenHandles": 0,
  "targetWriterProcessesWithOpenHandles": 0,
  "stoppedWriterUnits": [
    "hashtree.service",
    "iris-audio-crawler.service"
  ]
}
```

`sourceExternalIdentity` is JSON `null` when the source has no external root.
The topology hash binds every target member LMDB and external-root identity.
`stoppedWriterUnits` is nonempty, uniquely sorted, and names the complete
systemd service set whose processes can write any bound source or target
object. Before each
final-pass Pool open and immediately before `complete`, htree revalidates the
controller-state CAS and requires every named unit to remain loaded,
inactive/dead, without a main PID, control PID, or pending job. The root
controller must also prevent those units from restarting and keep both writer
fences held until htree has durably published `complete`; the state file is an
attestation, not a substitute for the controller's process/open-handle audit.

The topology CAS is strict JSON:

```json
{
  "schema": "hashtree-pool-migration-topology/v3",
  "poolPath": "/var/lib/hashtree/blob-pool-v1",
  "manifestSha256": "0000000000000000000000000000000000000000000000000000000000000000",
  "members": [{
    "id": "00000000-0000-0000-0000-000000000001",
    "path": "/srv/pool/member-1",
    "directoryIdentity": {"device": 4, "inode": 40},
    "lmdbIdentity": {
      "directory": {"device": 4, "inode": 40},
      "data": {"device": 4, "inode": 41},
      "lock": {"device": 4, "inode": 42}
    },
    "marker": {
      "path": "/srv/pool/member-1/.hashtree-pool-member-v1",
      "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
    },
    "externalPath": "/srv/pool/member-1-external",
    "externalDirectoryIdentity": {"device": 4, "inode": 43},
    "externalMarker": {
      "path": "/srv/pool/member-1-external/.hashtree-pool-external-v1",
      "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
    }
  }]
}
```

Members are uniquely sorted by canonical UUID. Source, catalog, member,
external, cursor, attempt, namespace, and evidence roots cannot overlap.
Every LMDB data/lock inode must also be globally unique; distinct directories
cannot hide hardlinked aliases. The manifest hash covers the full stored
generation, order, state, and configuration, not only member IDs.

### Online and final passes

An `online-bounded` request requires a positive `--max-items`, may resume an
authorized hex cursor, and never writes `complete`, even if it reaches the
current source end. Each new process uses a fresh attempt nonce while
continuing the same cursor authority. Before acknowledgement, the process
takes a nonblocking exclusive kernel lease on the pinned cursor-parent
directory and revalidates the cursor under that lease. Every cursor
publication compares the live canonical bytes with the last acknowledged or
published value before replacing them, so concurrent v3 attempts cannot
regress a cursor or overwrite `complete`.

For cutover, fence every source and target writer and keep both fences held
through durable completion. While all Pool writers and handles remain fenced,
remove only an exactly audited list of physically absent `Pending` records
with `cleanup_stale_pending_exact_offline_sync`; generic `delete_many_sync` is
not a safe substitute. Allocate a fresh cursor path whose file is absent, set
`HTREE_POOL_LIMIT_ARGS=` so argv has no `--max-items`, and use phase
`final-stopped-full`. This pass rescans the source from the beginning, verifies
source bytes even for already-catalogued hashes, compares committed target
bytes, and syncs the catalog and every member. Its terminal source proof also
scans the complete `blobs` and `metadata` key sets. A wholly legacy source with
no metadata is accepted; once any metadata exists, the two key sets must agree
exactly so a mixed legacy store cannot silently omit blob-only rows.

Before writing `complete`, the same process performs an exhaustive,
authority-pinned terminal target audit. Every target catalog entry, including
target-only entries, must be `Stored`; every body must match its declared
SHA-256 and size; the complete by-member index, member blob/metadata counts,
exact per-member blob/metadata key sets and metadata bytes, manifest, and move
state must agree exactly. Any `Pending`, `Moving`, invalid, missing, corrupt,
or addressable orphan payload or metadata row withholds `complete`. The process
first durably publishes the source/target counts and audit digests as the
attempt's O_EXCL `terminal-audit.json`, rechecks paths and writer-fence evidence,
and only then publishes `complete`. Never reuse an online cursor for the final
pass: new hashes can sort below it.

The controller sequence is:

```bash
sudo systemctl daemon-reload
sudo systemctl start --no-block hashtree-pool-migrate@NAME.service
# Read InvocationID/MainPID and /proc/MAINPID/stat field 22.
# Publish the exact root-owned request, then require its matching durable ack.
systemctl status hashtree-pool-migrate@NAME.service
journalctl -u hashtree-pool-migrate@NAME.service
```

Require the final pass's terminal audit and durable `complete` before cutover.
Retain the source, rollout evidence, claims, requests, acknowledgements, and
cursors through the rollback window. Remove the temporary unit and migration
binary only after that window closes.
