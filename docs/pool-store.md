# PoolStore

`PoolStore` is one application-owned content-addressed `Store` composed from
opaque local LMDB members. Applications still make one explicit write. The
pool owns placement, relocation, pins, and deletion; `BlobRouter` sees the
whole pool as one read route.

Members are not labelled NVMe, SSD, HDD, or archive. Placement learns bounded
in-memory read/write outcomes and uses observed successful latency and
reliability. Adding or draining a member does not add a write router, daemon,
or mirroring requirement.

## Automatic temperature balancing

Each process samples one successful read in every 64 by default. Samples enter
a bounded CLOCK queue and are flushed in one LMDB transaction by the periodic
worker. Reads never update catalog metadata individually. Persisted heat uses
integer half-life decay and is advisory: losing every heat record changes only
placement quality, never blob correctness or availability.

Each cycle:

1. flushes a bounded sample batch;
2. scans a bounded slice of the location catalog after a persisted cursor;
3. keeps bounded hot and cold candidate CLOCK queues;
4. resumes persisted interrupted moves before planning new ones;
5. promotes hot candidates only to a measurably faster member with sufficient
   headroom; and
6. when a member crosses its high watermark, demotes cold candidates toward
   higher-capacity members until the low watermark is reached.

The default cycle is limited by item, byte, and concurrency budgets. It pauses
new batches when foreground member load reaches the configured threshold.
Minimum residence time, distinct high/low watermarks, and a required measured
performance gain prevent oscillation. A short persisted lease gives one
same-host process ownership of relocation while every process may batch its own
read samples. The owner heartbeats the lease during long streamed moves;
process death still makes ownership expire.

No catalog-wide scan runs at startup. Catalog size may be tens of millions of
locations while startup and each cycle remain bounded by configuration.

## Move invariant

A move is:

```text
persist Moving(source, target)
  -> stream source into target staging storage
  -> verify size and SHA-256
  -> commit target storage
  -> atomically commit Stored(target) and residence metadata
  -> delete source
```

The persisted move record is committed with `Moving`. A crash before the
location switch leaves the source authoritative and the move resumable; a
crash after the target write verifies and reuses the target. Readers try both
members while a move is active. Target bytes never become canonical before
hash verification, and source deletion never precedes the atomic location
switch.

Large external blobs are copied and hashed in configured chunks without
materializing the blob. Inline LMDB targets still require an addressable value,
so deployments accepting very large individual blobs should configure external
blob storage for those members.

## Overrides

`PoolTemperatureConfig` controls the interval, sample rate, heat half-life,
sample flush and cursor scan bounds, candidate capacity, move/byte/concurrency
budgets, minimum residence, promotion hysteresis, foreground-load threshold,
lease duration, and stream chunk size. Defaults are conservative and automatic;
applications can disable the worker or override any bound when opening the
pool.

Each `PoolMemberConfig` has independent low/high fill watermarks (70%/85% by
default). New writes prefer members that remain below their high watermark when
another member has room; the limit is soft so a pool where every member is
above its watermark remains writable up to explicit capacity. The CLI exposes
watermarks on `storage pool add` and `storage pool
configure`. `storage pool balance-temperature` runs one explicit bounded cycle
and accepts temporary move, byte, and concurrency overrides.

Capacity, concurrency, watermarks, and external-blob thresholds are explicit
operator limits. They are not storage-tier identities and do not force
mirroring.
