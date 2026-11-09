// Passive, bounded observation of the sender's first file-control frame only.
import assert from 'node:assert/strict';
export class ReadyProof {
  constructor(peer) {
    this.peer = peer; this.buffer = new Uint8Array(4096); this.seen = new Uint8Array(4096);
    this.contiguous = 0; this.packets = 0; this.complete = false;
  }
  observe(context, segment) {
    if (this.complete) return;
    assert(++this.packets <= 128, 'first-control observation packet bound');
    assert.equal(context.src.length === 66 ? context.src.slice(2) : context.src, this.peer);
    assert.equal(context.srcPort, 39512); assert.equal(context.dstPort, 39512);
    const { syn, ack, seq, srcPort, dstPort, payload } = segment;
    if (syn) {
      assert(ack && payload.length === 0, 'expected sender SYN-ACK without payload');
      const binding = { base: (seq + 1) >>> 0, srcPort, dstPort };
      if (this.binding) assert.deepEqual(binding, this.binding); else this.binding = binding;
      return;
    }
    if (!payload.length) return;
    assert(this.binding, 'sender SYN-ACK must precede file control');
    assert.equal(srcPort, this.binding.srcPort); assert.equal(dstPort, this.binding.dstPort);
    const offset = (seq - this.binding.base) >>> 0;
    for (let i = 0; i < payload.length; i++) {
      const at = offset + i; assert(at < this.buffer.length, 'first-control byte bound');
      if (this.seen[at]) assert.equal(this.buffer[at], payload[i], 'conflicting retransmission');
      else { this.buffer[at] = payload[i]; this.seen[at] = 1; }
      while (this.contiguous < this.buffer.length && this.seen[this.contiguous]) this.contiguous++;
      if (this.contiguous >= 4) {
        const size = new DataView(this.buffer.buffer).getUint32(0, false);
        assert(size > 1 && size <= 4092, 'bounded first control frame');
        if (this.contiguous >= size + 4) {
          this.complete = true;
          return this.buffer.slice(0, size + 4);
        }
      }
    }
  }
}
export function resumeEvidence(rustSender, rustEvents, tsEvents, id, prefix, rustPeer) {
  assert(Number.isSafeInteger(prefix) && prefix > 0);
  const sender = rustSender ? rustEvents : tsEvents;
  const accepted = sender.find(e => e.event === 'accepted' && e.id === id && (rustSender || e.bytes === prefix));
  assert(accepted, 'actual sender must accept the resumed transfer');
  const ready = rustSender ? tsEvents.find(e => e.event === 'sender-ready' && e.id === id &&
    e.peer === rustPeer && e.bytes === prefix) : undefined;
  if (rustSender) assert(ready, 'Rust sender Ready must acknowledge the saved offset');
  return { sender: rustSender ? 'rust' : 'ts', accepted: true, acceptedOffset: prefix,
    receiverSavedOffset: prefix, senderEvent: accepted, readyEvent: ready,
    method: rustSender ? 'sender-event-and-authenticated-ready' : 'sender-event-offset' };
}
