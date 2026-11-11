#!/bin/sh
# Runs inside a glibc Linux container that also has the musl toolchain.
# Builds the vendored LMDB with both C libraries and checks pairings of a store
# holder and a second-process reader: compatible pairs must work, and
# incompatible pairs must be refused at once instead of blocking forever.
set -eu

LMDB_DIR="${LMDB_DIR:-/lmdb}"
SRC_DIR="${SRC_DIR:-/src}"
READER_TIMEOUT_SECONDS="${READER_TIMEOUT_SECONDS:-10}"
WORK="$(mktemp -d)"
cd "$WORK"

build() { # build <name> <compiler> <extra flags...>
    name="$1"; compiler="$2"; shift 2
    for program in holder reader; do
        "$compiler" "$@" -O1 -w -I"$LMDB_DIR" -o "${program}_${name}" \
            "$SRC_DIR/$program.c" "$LMDB_DIR/mdb.c" "$LMDB_DIR/midl.c" -lpthread
    done
}

# MDB_LIBC_MUSL is what the crate's build script defines for musl targets.
# The "plain" builds switch the guard off: they behave like stock LMDB and like
# Hashtree builds from before the guard existed.
build glibc gcc
build musl musl-gcc -static -DMDB_LIBC_MUSL
build glibc_plain gcc -DMDB_MUTEX_ABI_GUARD=0
build musl_plain musl-gcc -static -DMDB_MUTEX_ABI_GUARD=0

failures=0
holder_pid=""

wait_for_line() { # wait_for_line <file> <text>
    tries=0
    until grep -q "$2" "$1" 2>/dev/null; do
        tries=$((tries + 1))
        if [ "$tries" -gt $((READER_TIMEOUT_SECONDS * 20)) ]; then
            echo "no '$2' in $1: $(cat "${1%.out}.err" 2>/dev/null)"
            return 1
        fi
        sleep 0.05
    done
}

wait_for_holder() { wait_for_line holder.out "$1"; }

start_holder() { # start_holder <build> <dir>
    mkdir -p "$2"
    rm -f holder.in
    mkfifo holder.in
    "./holder_$1" "$2" <holder.in >holder.out 2>holder.err &
    holder_pid=$!
    exec 3>holder.in
    wait_for_holder ready
}

stop_holder() {
    exec 3>&-
    wait "$holder_pid" 2>/dev/null || true
}

run_reader() { # run_reader <build> <dir>; sets reader_result
    if timeout "$READER_TIMEOUT_SECONDS" "./reader_$1" "$2" >reader.out 2>reader.err; then
        reader_result="ok"
    else
        status=$?
        if [ "$status" -eq 124 ]; then
            reader_result="blocked for ${READER_TIMEOUT_SECONDS}s"
        else
            reader_result="refused ($(tr '\n' ' ' <reader.err))"
        fi
    fi
}

# expect <holder> <reader> <ok|mismatch|refused>
#   ok        the reader opens the store and begins a read transaction
#   mismatch  the reader is refused with MDB_VERSION_MISMATCH
#   refused   the reader is refused with any error (a build without the guard)
# In every case the holder must still commit afterwards, and a reader built
# like the holder must still work: a refused reader may not damage the locks.
expect() {
    holder="$1"; reader="$2"; want="$3"
    dir="$WORK/db_${holder}_${reader}"
    start_holder "$holder" "$dir"
    run_reader "$reader" "$dir"
    got="$reader_result"
    case "$want:$got" in
        ok:ok) verdict=pass ;;
        mismatch:refused*MDB_VERSION_MISMATCH*) verdict=pass ;;
        refused:refused*) verdict=pass ;;
        *) verdict=FAIL ;;
    esac
    : >holder.out
    echo write >&3
    wait_for_holder wrote || verdict=FAIL
    run_reader "$holder" "$dir"
    if [ "$reader_result" != ok ]; then
        verdict=FAIL
        got="$got; afterwards a $holder reader: $reader_result"
    fi
    stop_holder
    echo "$verdict: holder=$holder reader=$reader want=$want got=$got"
    [ "$verdict" = pass ] || failures=$((failures + 1))
}

echo "machine: $(uname -m)"

# Same C library on both sides, with and without the guard on either side:
# stores opened by older builds stay usable.
expect glibc glibc ok
expect musl musl ok
expect glibc_plain glibc ok
expect glibc glibc_plain ok
expect musl_plain musl ok
expect musl musl_plain ok

# Different C libraries: refused at open by a guarded reader.
expect musl glibc mismatch
expect glibc musl mismatch
expect musl_plain glibc mismatch
expect glibc_plain musl mismatch

# A reader without the guard (stock LMDB) on a guarded musl store: this is the
# pairing that used to block forever in read-transaction begin.
expect musl glibc_plain refused

# The stamp must not cost musl its dead-owner recovery: a writer killed while
# it holds the writer lock may not block the next writer on the same store.
recovers_from_dead_writer() { # recovers_from_dead_writer <build>
    build="$1"; dir="$WORK/db_dead_${build}"; verdict=pass
    start_holder "$build" "$dir"
    first_pid="$holder_pid"
    # A second writer attaches while the first is alive, so the lock file is
    # not reinitialized when the first one dies.
    rm -f second.in
    mkfifo second.in
    "./holder_$build" "$dir" <second.in >second.out 2>second.err &
    second_pid=$!
    exec 4>second.in
    wait_for_line second.out ready || verdict=FAIL
    : >holder.out
    echo hold >&3
    wait_for_holder holding || verdict=FAIL
    kill -9 "$first_pid"
    exec 3>&-
    wait "$first_pid" 2>/dev/null || true
    echo write >&4
    wait_for_line second.out wrote || verdict=FAIL
    exec 4>&-
    kill "$second_pid" 2>/dev/null || true
    wait "$second_pid" 2>/dev/null || true
    echo "$verdict: $build writer takes over the lock of a killed writer"
    [ "$verdict" = pass ] || failures=$((failures + 1))
}
recovers_from_dead_writer musl
recovers_from_dead_writer glibc

if [ "$failures" -ne 0 ]; then
    echo "$failures pairing(s) failed"
    exit 1
fi
echo "all pairings behaved"
