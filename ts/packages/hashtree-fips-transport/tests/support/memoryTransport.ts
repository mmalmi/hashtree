import {
  toHex,
  transportAddressKey,
  type Transport,
  type TransportAddress,
  type TransportContext,
} from '@fips/core';

export class MemoryHub {
  private readonly peers = new Map<string, MemoryTransport>();

  register(peerId: string, transport: MemoryTransport): void {
    this.peers.set(peerId, transport);
  }

  unregister(peerId: string): void {
    this.peers.delete(peerId);
  }

  resolve(peerId: string): MemoryTransport | undefined {
    return this.peers.get(peerId);
  }
}

export class MemoryTransport implements Transport {
  readonly type = 'memory';
  readonly mtu = 65_535;
  private context?: TransportContext;
  private localPeerId = '';
  private readonly connected = new Set<string>();

  constructor(private readonly hub: MemoryHub) {}

  async start(context: TransportContext): Promise<void> {
    this.context = context;
    this.localPeerId = toHex(context.localIdentity.publicKey);
    this.hub.register(this.localPeerId, this);
  }

  async stop(): Promise<void> {
    this.hub.unregister(this.localPeerId);
    for (const key of this.connected) {
      this.context?.onConnectionState?.({ remoteAddr: parseAddress(key), state: 'disconnected' });
    }
    this.connected.clear();
    this.context = undefined;
  }

  async connect(remoteAddr: TransportAddress): Promise<void> {
    const remote = this.hub.resolve(remoteAddr.addr);
    if (!remote) throw new Error(`unknown memory peer ${remoteAddr.addr}`);
    this.connected.add(transportAddressKey(remoteAddr));
    this.context?.onConnectionState?.({ remoteAddr, state: 'connected' });

    const localAddr = { transport: this.type, addr: this.localPeerId };
    remote.connected.add(transportAddressKey(localAddr));
    remote.context?.onConnectionState?.({ remoteAddr: localAddr, state: 'connected' });
  }

  async send(remoteAddr: TransportAddress, packet: Uint8Array): Promise<void> {
    const remote = this.hub.resolve(remoteAddr.addr);
    if (!remote?.context) throw new Error(`memory peer offline ${remoteAddr.addr}`);
    const source = { transport: this.type, addr: this.localPeerId };
    const data = packet.slice();
    queueMicrotask(() => {
      remote.context?.onPacket({
        transportType: this.type,
        remoteAddr: source,
        data,
        receivedAtMs: Date.now(),
      });
    });
  }

  async close(remoteAddr: TransportAddress): Promise<void> {
    if (this.connected.delete(transportAddressKey(remoteAddr))) {
      this.context?.onConnectionState?.({ remoteAddr, state: 'disconnected' });
    }
  }
}

function parseAddress(key: string): TransportAddress {
  const separator = key.indexOf(':');
  return {
    transport: key.slice(0, separator),
    addr: key.slice(separator + 1),
  };
}
