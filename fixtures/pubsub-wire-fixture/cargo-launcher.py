#!/usr/bin/env python3
"""Explicit test harness substitution: only original cargo fixture invocation is admitted."""
from pathlib import Path
import hashlib,json,os,sys
EXPECTED=['run','--quiet','--package','nostr-pubsub-fips','--example','fips-pubsub-wire-stdio']
if sys.argv[1:]!=EXPECTED or Path.cwd()!=Path('/stage/workspace'):
 print('Unsupported fixture invocation',file=sys.stderr);raise SystemExit(64)
receipt=json.loads(Path('/stage/evidence/wire-binary.json').read_text());binary=Path(receipt['path'])
if binary.parent.name!='examples' or binary.name!='fips-pubsub-wire-stdio' or not binary.is_relative_to('/stage/cargo-target'):
 raise SystemExit(65)
if hashlib.sha256(binary.read_bytes()).hexdigest()!=receipt['sha256']:raise SystemExit(66)
with Path('/stage/evidence/fixture-invocations.jsonl').open('a') as output:
 output.write(json.dumps({'argv':EXPECTED,'cwd':'/stage/workspace','binarySha256':receipt['sha256'],'substitution':'exact cargo fixture command to sealed original Rust executable'})+'\n')
os.execv(str(binary),[str(binary)])
