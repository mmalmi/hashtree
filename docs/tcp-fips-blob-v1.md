# TCP/FIPS Hashtree blob v1

Hashtree blob transfer uses TCP/FIPS service `39018`. TCP owns ordered byte
delivery, flow control, and segment retransmission. Hashtree retains one bounded
whole-session retry because TCP cannot recover an entirely reset FIPS session.

The client request is 35 bytes: magic `0x48` (`H`), version `0x01`, operation
`0x01` (get), then the 32-byte SHA-256 hash. The server response starts with a
7-byte header: the same magic and version, status `0x00` (missing) or `0x01`
(found), and a big-endian unsigned 32-bit payload length. A found header is
followed by exactly that many blob bytes. Implementations reject payloads above
16 MiB and verify the SHA-256 hash before caching or returning them.

Shared vector for hash bytes `00..1f`:

- request: `480101000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f`
- found response header for three bytes: `48010100000003`
