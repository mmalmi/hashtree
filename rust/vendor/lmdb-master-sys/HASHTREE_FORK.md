# Hashtree LMDB C fork

This package preserves lmdb-master-sys 0.2.6 behavior and adds an optional
caller-supplied expected device/inode identity for `data.mdb` and `lock.mdb`.
When enabled, LMDB opens without `O_CREAT` and validates each descriptor
inside C before lock-file truncation, mapping, or other environment setup.

It is published before `hashtree-heed` so downstream `hashtree-lmdb`
consumers retain the same enforcement used by workspace builds.

On 64-bit Linux it also keeps processes linked against glibc and against musl
off one lock file. Their process-shared mutexes have the same size on x86_64, so
LMDB's lock format cannot tell them apart, and a glibc process used to block
forever in transaction begin on a musl process's store while overwriting the
mutex type word. An attaching process now gets `MDB_VERSION_MISMATCH` when the
lock file's mutexes were set up by the other C library. musl builds also stamp
an unused mutex word so that glibc builds without this check, stock LMDB
included, get `EINVAL` from the lock call instead of waiting. The build script
defines `MDB_LIBC_MUSL` for musl targets; `-DMDB_MUTEX_ABI_GUARD=0` disables
both. `rust/scripts/run_lmdb_cross_libc_test_in_docker.sh` in the Hashtree
repository exercises the pairings.
