# Hashtree LMDB C fork

This package preserves lmdb-master-sys 0.2.6 behavior and adds an optional
caller-supplied expected device/inode identity for `data.mdb` and `lock.mdb`.
When enabled, LMDB opens without `O_CREAT` and validates each descriptor
inside C before lock-file truncation, mapping, or other environment setup.

It is published before `hashtree-heed` so downstream `hashtree-lmdb`
consumers retain the same enforcement used by workspace builds.
