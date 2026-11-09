#!/bin/sh
set -eu
sha256sum -c /stage/procedure/system-checksums.txt
sha256sum /usr/share/keyrings/debian-archive-keyring.gpg > /stage/evidence/debian-keyring-sha256.txt
gpgv --keyring /usr/share/keyrings/debian-archive-keyring.gpg --status-fd 1 --output /stage/evidence/signed-Release /stage/system-inputs/InRelease > /stage/evidence/debian-gpg-status.txt 2> /stage/evidence/debian-gpg.log
dpkg-query -W -f '${Package}\t${Version}\t${Architecture}\t${Status}\n' > /stage/evidence/base-system-packages.tsv
dpkg --audit > /stage/evidence/base-dpkg-audit.txt
test ! -s /stage/evidence/base-dpkg-audit.txt
dpkg --compare-versions '1.14.10-1~deb12u1' = '1.14.10-1~deb12u1'
dpkg --compare-versions 0.18+nmu1 '>=' 0.14
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libc6)" '>=' 2.34
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libgcc-s1)" '>=' 3.0
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libstdc++6)" '>=' 11
dpkg --compare-versions 1:14.0.6-12 '>=' 1:14.0.6-12
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libc6)" '>=' 2.34
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' sed)" '>=' 4.1.2-8
dpkg --compare-versions 1.31 '>=' 1.28
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libc6)" '>=' 2.36
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libedit2)" '>=' 2.11-20080614-0
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libffi8)" '>=' 3.4
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libgcc-s1)" '>=' 3.3
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libstdc++6)" '>=' 12
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libtinfo6)" '>=' 6
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libxml2)" '>=' 2.7.4
dpkg --compare-versions 4.8.12-3.1 '>=' 4.8.12
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' zlib1g)" '>=' 1:1.2.0
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' dpkg)" '>=' 1.16.4
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libc6)" '>=' 2.34
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libgcc-s1)" '>=' 3.4
dpkg --compare-versions "$(dpkg-query -W -f '${Version}' libstdc++6)" '>=' 12
