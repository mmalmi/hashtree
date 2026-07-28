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

`hashtree-pool-migration-controller@.service` is the root checkpoint broker
and the only unit an operator starts. It validates the rollout authorities,
creates a fresh attempt, starts the bound
`hashtree-pool-migration-worker@.service`, and keeps the writer fences and
checkpoint protocol alive until that worker terminates. The worker runs as the
unprivileged `hashtree` user and is inert until the controller publishes an
exact v3 request that it durably acknowledges before opening either LMDB
store. Neither unit is restartable.

Install two immutable copies of the same release binary and both exact unit
fragments:

```bash
sudo install -o root -g root -m 0555 target/release/htree \
  /usr/local/bin/htree-pool-migration
sudo install -o root -g root -m 0555 target/release/htree \
  /usr/local/bin/htree-pool-migration-controller
sudo install -o root -g root -m 0644 \
  packaging/systemd/hashtree-pool-migration-controller@.service \
  /etc/systemd/system/hashtree-pool-migration-controller@.service
sudo install -o root -g root -m 0644 \
  packaging/systemd/hashtree-pool-migration-worker@.service \
  /etc/systemd/system/hashtree-pool-migration-worker@.service
sudo install -d -o root -g root -m 0755 /etc/hashtree
sudo install -d -o hashtree -g hashtree -m 0750 \
  /var/lib/hashtree/pool-migration/cursors
sudo systemctl daemon-reload
```

Never start the worker directly. Its `BindsTo=` and `After=` relationship to
the same-name controller instance is part of the release authority.

The loaded worker unit must have no drop-ins, conditions, start/stop helpers,
reload helpers, restart, or control process. It must retain `Type=oneshot`,
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

Create `/etc/hashtree/pool-migration-worker-NAME.env` as a root-owned `0644` file.
Only the keys understood by the v3 validator are legal:

```text
HTREE_POOL_TARGET_DATA_DIR=/var/lib/hashtree
HTREE_POOL_LAUNCH_REQUEST=/var/lib/hashtree/pool-migration/ROLLOUT/attempts-v3/NONCE/launch-request.json
HTREE_POOL_LAUNCH_WAIT_SECONDS=120
HTREE_POOL_SOURCE_LMDB_DIR=/mnt/old-store/blobs
HTREE_POOL_SOURCE_EXTERNAL_ARGS=--source-external-dir /mnt/old-store/blob-files-v1
HTREE_POOL_STATE_FILE=/var/lib/hashtree/pool-migration/cursors/old-store.cursor
HTREE_POOL_BATCH_SIZE=4096
HTREE_POOL_MAX_BUFFER_MIB=64
HTREE_POOL_SOURCE_READ_CONCURRENCY=4
HTREE_POOL_REOPEN_BATCHES=256
HTREE_POOL_LIMIT_ARGS=
```

Use an empty `HTREE_POOL_SOURCE_EXTERNAL_ARGS=` when the source has no
external directory. Set `HTREE_POOL_LIMIT_ARGS=--max-items N` for each
`online-bounded` tranche and leave it empty for both stopped-final phases.
Stopped-final batch size is fixed at 4096 to keep the durable checkpoint
namespace practical; source read concurrency is hard capped at 64. Unknown,
duplicate, quoted, or loader-affecting assignments are rejected. Htree binds
the file path, inode, bytes, SHA-256, systemd-loaded path, and resulting
process environment.

Create `/etc/hashtree/pool-migration-controller-NAME.env` as a root-owned
`0644` file containing one unquoted `HTREE_POOL_CONTROLLER_ARGS=` assignment.
Its value is the complete argument vector beginning with:

```text
--data-dir /var/lib/hashtree storage pool launch-migrate-lmdb-v3
```

and must include the exact absolute `--rollout-dir`, `--rollout-id`, `--phase`,
controller executable/unit/fragment/environment paths, controller-state,
source-baseline and Pool-topology inputs, every `--cas` and sorted
`--writer-unit`, the worker unit/fragment/environment/binary, service GID,
target data/Pool/source/external/cursor paths, `--batch-size 4096`,
`--max-buffer-mib`, `--source-read-concurrency`, `--reopen-batches`, and both
launch/acknowledgement waits. Add a nonzero `--max-items N` only for
`online-bounded`; stopped-final phases reject it. The controller reconstructs
and validates the worker argv and environment; it does not trust an
operator-authored launch request.

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
  "systemdUnit": "hashtree-pool-migration-worker@NAME.service",
  "systemdManager": "system",
  "systemdFragment": {
    "path": "/etc/systemd/system/hashtree-pool-migration-worker@.service",
    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
  },
  "systemdEnvironmentFile": {
    "path": "/etc/hashtree/pool-migration-worker-NAME.env",
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
    "--batch-size", "4096",
    "--max-buffer-mib", "64",
    "--source-read-concurrency", "4",
    "--reopen-batches", "256",
    "--resume"
  ],
  "controller": {
    "rolloutId": "ROLLOUT",
    "phase": "final-stopped-full",
    "executable": {
      "path": "/usr/local/bin/htree-pool-migration-controller",
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
  }, {
    "label": "source-terminal-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "path": "/absolute/ROLLOUT/attempts-v3/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/source-terminal.json",
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
  ],
  "writerUnitMasks": [{
    "unit": "hashtree.service",
    "path": "/run/systemd/system/hashtree.service",
    "identity": {"device": 5, "inode": 50},
    "target": "/dev/null"
  }, {
    "unit": "iris-audio-crawler.service",
    "path": "/run/systemd/system/iris-audio-crawler.service",
    "identity": {"device": 5, "inode": 52},
    "target": "/dev/null"
  }],
  "legacyWorkerTemplateMask": {
    "unit": "hashtree-pool-migrate@.service",
    "path": "/run/systemd/system/hashtree-pool-migrate@.service",
    "identity": {"device": 5, "inode": 51},
    "target": "/dev/null"
  },
  "legacyWorkerInstanceMasks": [],
  "sourceTerminalReceiptSha256": [
    "0000000000000000000000000000000000000000000000000000000000000000"
  ]
}
```

`sourceExternalIdentity` is JSON `null` when the source has no external root.
The topology hash binds every target member LMDB and external-root identity.
`stoppedWriterUnits` and its exact runtime mask set are nonempty, uniquely
sorted, and name the complete systemd service set whose processes can write
any bound source or target object. The legacy worker template and every loaded
legacy instance are separately runtime-masked. Before each final-pass Pool
open and immediately before `complete`, htree revalidates the controller-state
CAS and requires every named unit to remain loaded,
inactive/dead, without a main PID, control PID, or pending job. The root
controller must also prevent those units from restarting and keep both writer
fences held until htree has durably published `complete`; the state file is an
attestation, not a substitute for the controller's process/open-handle audit.
`sourceTerminalReceiptSha256` is empty for `final-stopped-source` and contains
the uniquely sorted exact receipt set for `final-stopped-full`.

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

### Supported online and stopped-final flow

The supported full migration is three-stage:

1. While ordinary writers remain online, run repeated `online-bounded`
   tranches with a nonzero `--max-items`. The worker keeps an
   authority-bound durable verified ledger at
   `<state-file>.online-audit-v3`. For a hash absent from that ledger it reads
   and SHA-256-verifies the source body, compares an existing target body
   byte-for-byte or durably writes it, force-syncs the target, then force-syncs
   the ledger before advancing the scan cursor. A crash may leave the ledger
   ahead of the cursor; replay safely skips only exact hash/size proofs. On
   reaching the cursor tail, a metadata-only full-source coverage scan detects
   an old unproved prefix or a newly inserted lower hash and resets the cursor
   for catch-up without rereading already proved bodies. Completion publishes
   `online-target-audit.json`, immutable sorted source evidence, and the
   root-owned `online-target-audit-certification.json`. Keep restarting fresh
   controller attempts with the same rollout/source/Pool/cursor authorities
   until the controller result contains a non-null
   `onlineTargetAuditCertification`.
2. Pass that exact certification as
   `--cas online-target-audit-NONCE=/absolute/attempt/online-target-audit-certification.json`,
   fence every writer that can open the source, install its exact runtime unit
   masks, prove no process retains source LMDB or external-corpus handles, and
   run `final-stopped-source`. The source LMDB `blobs` database is the raw key
   authority: legacy blob-only and partially populated metadata stores are
   accepted, while metadata-only rows are rejected. The worker scans the fresh
   stopped hash/size boundary against the certified online evidence, reading
   zero payload bodies, and publishes exact current-source evidence plus
   `source-terminal.json`. A new or size-changed source row fails closed with
   an instruction to recover the stopped attempt and rerun online catch-up.
3. Keep those source fences and mounts intact, additionally fence every target
   Pool writer/handle, and run `final-stopped-full` with a fresh absent cursor.
   The controller-state `sourceTerminalReceiptSha256` set must contain every
   uniquely sorted source receipt digest, and each receipt is passed as
   `--cas source-terminal-NONCE=/absolute/attempt/source-terminal.json`. At
   most 64 source receipts/environments are accepted. The worker merges their
   sorted evidence, reads and hash-verifies only bodies needed to repair
   missing or interrupted target records, closes and reopens all source/target
   mappings after the bounded epoch, and then performs one exhaustive
   catalog-to-physical-member parity audit.

Before either stage, remove only an exactly audited list of physically absent
`Pending` records with `cleanup_stale_pending_exact_offline_sync`; generic
`delete_many_sync` is not a safe substitute. Every source, target and legacy
migration writer unit must remain stopped and runtime-masked for the complete
stage. Immutable readers may remain available only when the controller's
handle census and mount policy explicitly permit them.

The full-final target audit requires every catalog entry, including
target-only entries, to be `Stored`; the by-member index, exact member
hash/size ownership, physical inline/loose/packed sizes, pack ranges,
blob/metadata/eviction/pin state, persisted byte aggregates, manifest, and
move state must agree. Content authority comes from the root-certified online
body proof plus hash-verified repair writes; the stopped audit does not reread
the complete multi-terabyte payload corpus. Any `Pending`, `Moving`, invalid,
missing, size-mismatched, or addressable orphan row withholds completion. The
worker first publishes O_EXCL `terminal-audit.json`; the controller replays
the exact physical audit with its own read-only Pool open and handle census
before it tears down source mounts or publishes the durable `complete`
cursor. Source-final uses the same root replay rule for the frozen source
receipt before `source-complete` and its certification are published.

Read-only source/keyset, already-proved online pages, and full-union page scans
do not create root checkpoints and do not invoke `systemctl`. A checkpoint
exists only for a bounded target mutation, a newly durable online audit page,
or a terminal publication boundary. Each checkpoint uses exactly one batched
`systemctl show` for the controller, worker, complete writer set, legacy
template, and legacy instances; worker-side liveness and idle controller
polling use pinned files plus `/proc`, without subprocesses.
Each pre/post page fence validates the deduplicated current/prior source mount
set against one `/proc/self/mountinfo` snapshot; receipt files are not opened
on that path. Mapping-epoch and pre/post terminal-audit writer/legacy fence
checks each use one batched `systemctl show`, independent of source count.
The controller result reports both `authorizedCheckpoints` and
`checkpointSystemctlSubprocesses`; they must be equal.

Before scheduling downtime, run the installed controller and worker units
against a real, complete corpus snapshot with the production unit/mask set.
Do not use a generated fixture for this gate. Capture checkpoint request and
acknowledgement boottime fields plus controller/worker wall time, and require:

- `checkpointSystemctlSubprocesses == authorizedCheckpoints`;
- no `source-audit-batch` or `migration-page-scan` checkpoint artifacts;
- p99 request-to-authorization latency at most 100 ms and maximum at most
  250 ms;
- measured end-to-end time, projected from the real Stored/write ratio and
  corpus size, fits the approved downtime window with at least 25% reserve.

Also verify on that installed systemd version that the single batched
`systemctl show` returns an exact `Id=hashtree-pool-migrate@.service` block
with `LoadState=masked` and `UnitFileState=masked-runtime` for the bare legacy
template. A failed template query or any subprocess/latency mismatch blocks
the rollout.

For each stage, install fresh root-owned controller and worker environment
files using the same instance name, then start only the controller:

```bash
sudo systemctl daemon-reload
sudo systemctl start --no-block hashtree-pool-migration-controller@NAME.service
systemctl status hashtree-pool-migration-controller@NAME.service
systemctl status hashtree-pool-migration-worker@NAME.service
journalctl -u hashtree-pool-migration-controller@NAME.service \
  -u hashtree-pool-migration-worker@NAME.service
```

For the last online tranche, require
`online-target-audit-certification.json` and record its exact controller
result authority. For `final-stopped-source`, require a
controller-certified `source-terminal.json` and `source-complete` cursor.
Build a new `final-stopped-full` controller-state/environment from that exact
receipt; never edit or reuse the completed source attempt. For full-final,
require `terminal-audit.json`, the controller's terminal-publication receipt,
and the durable `complete` cursor before cutover.

If either unit fails or the machine reboots, keep all stores fenced. Do not
delete an attempt, request, acknowledgement, receipt, checkpoint, or cursor,
and do not start a second attempt over an unfinished terminal publication.
Restart the same installed controller invocation inputs so recovery can
revalidate current-boot masks/censuses and replay the exact terminal audit; a
prior-boot unfinished terminal attempt is not accepted. On the same boot,
source-final reconstructs an exact missing retention receipt from its
pre-acknowledgement terminal journal. Full-final reconstructs a missing
teardown intent from the terminal publication, launch-request digest, and
adopted lifecycle mounts, completes the non-lazy teardown, then re-audits
before publishing the cursor. A crash during reconstructed teardown replays
the durable step chain. Mixed mount disappearance fails closed and never
tears down the surviving mounts.

Retain every source, read-only mount authority, rollout input, mask record,
attempt, request, acknowledgement, receipt, checkpoint namespace, audit, and
cursor through the rollback window. Remove the temporary units and binaries
only after that window closes.
