import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { openSync, closeSync } from 'node:fs';
export const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
export async function until(check, label, ms = 30000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) { const value = await check(); if (value) return value; await delay(20); }
  throw new Error(`${label}: bounded wait expired`);
}
export class Peer {
  constructor(executable, args, log) {
    const fd = openSync(log, 'wx', 0o600);
    try { this.child = spawn(executable, args, { stdio: ['pipe', 'pipe', fd], env: { ...process.env, NODE_OPTIONS: '', RUST_LOG: 'off', RUST_BACKTRACE: '0' } }); }
    finally { closeSync(fd); }
    this.events = []; this.pending = new Map(); this.sequence = 0; this.bytes = 0; this.failure = undefined;
    this.done = new Promise(resolve => { this.child.once('close', (code, signal) => resolve({ code, signal })); this.child.once('error', error => { this.failure = error; resolve({ code: null, signal: 'spawn-error' }); }); });
    const reader = createInterface({ input: this.child.stdout });
    reader.on('line', line => {
      try {
        this.bytes += line.length; assert(this.bytes <= 1024 * 1024);
        const value = JSON.parse(line);
        if (value.reply !== undefined) {
          const request = this.pending.get(value.reply); assert(request); this.pending.delete(value.reply);
          value.error ? request.reject(new Error(value.error)) : request.resolve(value.value);
        } else { assert(this.events.length < 4096); this.events.push(value); }
      } catch (error) { this.failure = error; }
    });
  }
  live() {
    if (this.failure) throw this.failure;
    assert(this.child.exitCode === null && this.child.signalCode === null, 'peer exited unexpectedly');
    assert(!this.events.some(e => ['failed', 'cancelled', 'declined', 'harness-error'].includes(e.event)), 'unexpected terminal failure');
  }
  async ready() { return until(() => { this.live(); return this.events.find(e => e.event === 'ready'); }, 'peer readiness', 20000); }
  async command(value) {
    this.live(); const request = ++this.sequence;
    const result = new Promise((resolve, reject) => this.pending.set(request, { resolve, reject }));
    this.child.stdin.write(`${JSON.stringify({ ...value, request })}\n`);
    let timer;
    try { return await Promise.race([result, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('peer command deadline')), 15000); }), this.done.then(() => { throw new Error('peer exited during command'); })]); }
    finally { clearTimeout(timer); this.pending.delete(request); }
  }
  stop() {
    this.stopPromise ??= (async () => {
      if (this.child.exitCode === null && this.child.signalCode === null) this.child.stdin.end(`${JSON.stringify({ op: 'stop' })}\n`);
      let timer; const forced = new Promise(resolve => { timer = setTimeout(() => { this.child.kill('SIGKILL'); resolve(); }, 15000); });
      try { const result = await Promise.race([this.done, forced.then(() => this.done)]);
        assert.deepEqual(result, { code: 0, signal: null });
        assert(this.events.some(e => e.event === 'stopped' && e.graceful));
        assert(!this.events.some(e => e.event === 'stopped' && e.aborts > 0), 'shutdown erased retained output');
        assert(!this.failure);
      } finally { clearTimeout(timer); }
    })();
    return this.stopPromise;
  }
}
