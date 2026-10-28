#!/usr/bin/env python3
"""Instrument a disposable copy of registry client 0.1.5, preserving its graph."""
import hashlib
import json
import os
from pathlib import Path
import io
import tarfile
import subprocess
import sys
import tomllib

repo = Path(sys.argv[1]).resolve()
manifest = repo / 'Cargo.toml'
lock = repo / 'Cargo.lock'
before = tomllib.loads(lock.read_text())
metadata = json.loads(subprocess.check_output([
    'cargo', 'metadata', '--locked', '--filter-platform', 'aarch64-apple-darwin',
    '--format-version', '1', '--manifest-path', str(manifest)], text=True))
package, = [p for p in metadata['packages'] if p['name'] == 'hashtree-client']
assert package['version'] == '0.1.5' and package['source'].startswith('registry+')
original = Path(package['manifest_path']).parent
archive = original.parent.parent.parent / 'cache' / original.parent.name / (original.name + '.crate')
archive_bytes = archive.read_bytes()
digest = hashlib.sha256(archive_bytes).hexdigest()
assert digest == '24d997547bb950fbb85e7f7d09bf6a062b9579ef7496751f37ab2467f9ceb58d'
copy_root = Path(os.environ['RUNNER_TEMP']) / 'diagnostic-client'
copy_root.mkdir()
with tarfile.open(fileobj=io.BytesIO(archive_bytes), mode='r:gz') as package_archive:
    package_archive.extractall(copy_root, filter='data')
copy = copy_root / 'hashtree-client-0.1.5'
checksums = dict(package=digest, files={'src/store.rs': hashlib.sha256((copy/'src/store.rs').read_bytes()).hexdigest()})
file = copy / 'src/store.rs'
source = file.read_text()
source = source.replace('self.fetch_once(server, hash).await', 'self.fetch_once(server, hash, 1).await', 1)
source = source.replace('self.fetch_once(server, hash).await', 'self.fetch_once(server, hash, 2).await', 1)
old = '    async fn fetch_once(&self, server: &str, hash: &Hash) -> Result<Option<Vec<u8>>> {'
assert old in source
source = source.replace(old, '''    async fn fetch_once(&self, server: &str, hash: &Hash, attempt: u8) -> Result<Option<Vec<u8>>> {
        let started = std::time::Instant::now();
        let origin = reqwest::Url::parse(server).unwrap().origin().ascii_serialization();
        let mut last_progress = started;
        let mut chunks = 0usize;''')
old = '''            .send()
            .await
            .map_err(reqwest::Error::without_url)?;'''
assert old in source
source = source.replace(old, '''            .send()
            .await
            .map_err(|error| {
                eprintln!("HTREE_DOWNLOAD phase=headers_error block={} origin={} attempt={} elapsed_ms={} timeout={}",
                    hex::encode(hash), origin, attempt, started.elapsed().as_millis(), error.is_timeout());
                error.without_url()
            })?;''', 1)
old = '        let mut body = response.bytes_stream();'
assert old in source
source = source.replace(old, '''        let headers_ms = started.elapsed().as_millis();
        let protocol = response.version();
        let mut body = response.bytes_stream();''', 1)
old = '            let part = part.map_err(reqwest::Error::without_url)?;'
assert old in source
source = source.replace(old, '''            let part = part.map_err(|error| {
                eprintln!("HTREE_DOWNLOAD phase=body_error block={} origin={} attempt={} headers_ms={} elapsed_ms={} idle_ms={} bytes={} chunks={} protocol={:?} timeout={}",
                    hex::encode(hash), origin, attempt, headers_ms, started.elapsed().as_millis(),
                    last_progress.elapsed().as_millis(), bytes.len(), chunks, protocol, error.is_timeout());
                error.without_url()
            })?;
            last_progress = std::time::Instant::now();
            chunks += 1;''', 1)
old = '''        verify(hash, &bytes)?;
        Ok(Some(bytes))'''
assert old in source
source = source.replace(old, '''        verify(hash, &bytes)?;
        eprintln!("HTREE_DOWNLOAD phase=complete block={} origin={} attempt={} headers_ms={} elapsed_ms={} bytes={} chunks={} protocol={:?}",
            hex::encode(hash), origin, attempt, headers_ms, started.elapsed().as_millis(), bytes.len(), chunks, protocol);
        Ok(Some(bytes))''', 1)
file.write_text(source)
assert '[patch.crates-io]' not in manifest.read_text()
with manifest.open('a') as f:
    f.write('\n[patch.crates-io]\nhashtree-client = { path = ' + json.dumps(str(copy)) + ' }\n')
subprocess.run(['cargo', 'update', '--offline', '--manifest-path', str(manifest),
                '-p', 'hashtree-client@0.1.5'], check=True)
after = tomllib.loads(lock.read_text())
def remaining(data):
    return [p for p in data['package'] if p['name'] != 'hashtree-client']
assert remaining(before) == remaining(after), 'instrumentation changed other dependency resolutions'
old_client, = [p for p in before['package'] if p['name'] == 'hashtree-client']
new_client, = [p for p in after['package'] if p['name'] == 'hashtree-client']
assert {k: v for k, v in old_client.items() if k not in ('source', 'checksum')} == new_client
print(json.dumps(dict(diagnostic_only=True, registry_archive_sha256=checksums['package'],
    original_store_sha256=checksums['files']['src/store.rs'],
    instrumented_store_sha256=hashlib.sha256(file.read_bytes()).hexdigest(),
    unchanged_other_dependency_resolutions=True)), flush=True)
