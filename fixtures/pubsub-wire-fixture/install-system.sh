#!/bin/sh
set -eu
umask 022
sha256sum -c /stage/procedure/system-checksums.txt
test -f /stage/evidence/system-inputs-verified.json
dpkg --unpack /stage/system-inputs/libclang-common-14-dev_14.0.6-12_all.deb /stage/system-inputs/libclang1-14_14.0.6-12_amd64.deb '/stage/system-inputs/libdbus-1-3_1.14.10-1~deb12u1_amd64.deb' '/stage/system-inputs/libdbus-1-dev_1.14.10-1~deb12u1_amd64.deb' /stage/system-inputs/libllvm14_14.0.6-12_amd64.deb /stage/system-inputs/libz3-4_4.8.12-3.1_amd64.deb /stage/system-inputs/sgml-base_1.31_all.deb /stage/system-inputs/xml-core_0.18+nmu1_all.deb
dpkg --configure --pending
dpkg --audit > /stage/setup-output/derived-dpkg-audit.txt
test ! -s /stage/setup-output/derived-dpkg-audit.txt
dpkg-query -W -f '${Package}\t${Version}\t${Architecture}\t${Status}\n' > /stage/setup-output/derived-system-packages.tsv
pkg-config --modversion dbus-1 > /stage/setup-output/dbus-version.txt
test -r /usr/lib/llvm-14/lib/libclang.so.1
test -r /usr/lib/llvm-14/lib/clang/14.0.6/include/stdint.h
sha256sum "$(readlink -f /usr/lib/llvm-14/lib/libclang.so.1)" > /stage/setup-output/libclang-sha256.txt
ldd /usr/lib/llvm-14/lib/libclang.so.1 > /stage/setup-output/libclang-linkage.txt
if grep -q 'not found' /stage/setup-output/libclang-linkage.txt; then exit 1; fi
cat /etc/os-release > /stage/setup-output/derived-os-release.txt
