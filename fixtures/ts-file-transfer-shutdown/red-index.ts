// Extracted from Iris Chat (MIT), Copyright Iris Chat contributors.
import { FipsTcpEndpoint, MarkerStatus, State, type ConnectionId, type FipsDatagramEndpoint } from '@fips/tcp'
import { ReceiveFiles } from './files.js'
import { handlePacket, prepareReceiving, pollWork } from './protocol.js'
import { cleanup, emit, newConnection, OFFER_MS, rememberEnded, saveClaim, terminalClaim, type Connection, type Runtime } from './state.js'
import { DIRECT_FILE_CHUNK_BYTES, directFileFrame, errorText, manifestDigest, normalizePeer,
  pendingBytes, pendingControl, randomSeed, validateClaim, validateFiles } from './wire.js'
import { verificationDeadline } from './work.js'
import type { DirectFileSource, DirectFileSpec, DirectFileTcpOptions, DirectFileTransferOptions } from './types.js'
export type * from './types.js'
export { DIRECT_FILE_CHUNK_BYTES, DirectFileReader, directFileFrame, manifestDigest, prefixDigest } from './wire.js'
export const DIRECT_FILE_PORT = 39512
const MAX_TRANSFERS = 16
const MEMORY_RECEIVE_BYTES = 64 * 1024 * 1024
const IDLE_MS = 60_000
class TransportFailure extends Error {}

/** Recipient-pulled v1/v2 file streams over an authenticated FIPS endpoint. */
export class DirectFileTcp {
  private readonly r: Runtime
  private readonly opening = new Set<string>()
  private readonly preparing = new Map<string, { c: Connection; controller: AbortController; deadline: number; promise: Promise<void> }>()
  private readonly timer: ReturnType<typeof setInterval>
  private operation: Promise<unknown> = Promise.resolve()
  private tickPending = false
  constructor(options: DirectFileTcpOptions) {
    const endpoint: FipsDatagramEndpoint = {
      registerService: (port, handler) => options.endpoint.registerService(port, context =>
        handler({ ...context, src: normalizePeer(context.src) })),
      sendDatagram: args => options.endpoint.sendDatagram({ ...args, dst: `02${normalizePeer(args.dst)}` }),
    }
    this.r = { options, localPeer: normalizePeer(options.localPeer),
      tcp: new FipsTcpEndpoint(endpoint, DIRECT_FILE_PORT, {
        mss: 1024, sendBuffer: 64 * 1024, receiveBuffer: 0xffff,
        maxConnections: MAX_TRANSFERS, maxConnectionsPerPeer: 4, maxReassemblySegments: 128,
        ...options.tcpConfig,
      }, options.isnSeed ?? randomSeed()),
      offers: new Map(), connections: new Map(), ended: new Set(), memoryReserved: 0,
      stopped: false, now: options.now ?? Date.now,
    }
    this.timer = setInterval(() => {
      if (this.tickPending || this.r.stopped) return
      this.tickPending = true
      void this.enqueue(() => this.tick()).catch(error => this.report(error))
        .finally(() => { this.tickPending = false })
    }, 10)
  }
  registerOffer(id: string, token: string, allowedPeers: string[], files: DirectFileSource[], options: DirectFileTransferOptions = {}): void {
    this.assertAvailable(id)
    validateClaim(id, token); validateFiles(files)
    const r = this.r, version = options.version ?? 1
    if (version === 2 && !r.options.store) throw new Error('Persistent file transfer storage is unavailable')
    if (r.offers.size >= MAX_TRANSFERS || allowedPeers.length > 128) throw new Error('Too many file transfers')
    const peers = new Set(allowedPeers.map(normalizePeer).filter(peer => peer !== r.localPeer))
    if (!peers.size) throw new Error('No receiving device is available')
    if (files.some(file => file.blob.size !== file.sizeBytes)) throw new Error('Selected file changed')
    r.offers.set(id, { token, peers, files: files.map(file => ({ ...file })),
      expires: Math.min(options.expires ?? r.now() + OFFER_MS, r.now() + OFFER_MS), version })
  }
  async announceOffer(id: string, token: string, peer: string, files: DirectFileSource[]): Promise<void> {
    this.registerOffer(id, token, [peer], files, { version: 2 })
    await this.resumeOffer(id, peer)
  }
  /** Reannounce a retained offer, also recovering a durable completed receipt. */
  async resumeOffer(id: string, peer: string): Promise<void> {
    const offer = this.r.offers.get(id)
    if (!offer || offer.version !== 2 || !offer.peers.has(normalizePeer(peer))) throw new Error('File offer is no longer available')
    const { token, files } = offer
    await this.enqueue(async () => {
      if ([...this.r.connections.values()].some(c => c.transferId === id)) throw new Error('File offer is already in use')
      const previous = await this.r.options.store!.load(`send/${id}`)
      if (previous && (previous.peer !== normalizePeer(peer) || ['cancelled', 'declined'].includes(previous.status))) throw new Error('File offer is no longer available')
      const stream = await this.r.tcp.connect(normalizePeer(peer))
      const c = newConnection(this.r, id, normalizePeer(peer), { kind: 'announce' })
      c.version = 2; c.token = token; c.manifest = manifestDigest(files)
      c.claim = previous ?? { peer: c.peer, token, manifest: c.manifest, expires: offer.expires, status: 'claimed' }
      c.pending = pendingControl({ type: 'offer', id, token, version: 2,
        files: files.map(f => ({ filename: f.filename, size_bytes: f.sizeBytes, sha256: f.sha256 })) })
      this.r.connections.set(stream, c)
    })
  }
  restrictOffer(id: string, allowedPeers: string[]): void {
    const r = this.r, peers = new Set(allowedPeers.map(normalizePeer)), offer = r.offers.get(id)
    if (offer) {
      for (const peer of offer.peers) if (!peers.has(peer)) offer.peers.delete(peer)
      if (!offer.peers.size) void this.cancel(id).catch(error => this.report(error))
    }
    for (const c of r.connections.values()) if (c.transferId === id && !peers.has(c.peer)) c.revoked = true
  }
  async receive(id: string, token: string, senderPeer: string, files: DirectFileSpec[], options: DirectFileTransferOptions = {}): Promise<void> {
    this.assertAvailable(id, options.version === 2)
    validateClaim(id, token); validateFiles(files)
    const r = this.r, peer = normalizePeer(senderPeer)
    if (peer === r.localPeer) throw new Error('Choose another device to receive these files')
    const reserved = r.options.createReceiveFile ? 0 : files.reduce((sum, file) => sum + file.sizeBytes, 0)
    if (reserved + r.memoryReserved > MEMORY_RECEIVE_BYTES) throw new Error('This browser can receive up to 64 MB without local file storage')
    r.memoryReserved += reserved; this.opening.add(id)
    const c = newConnection(r, id, peer, new ReceiveFiles(id, files.map(file => ({ ...file })), r.options))
    c.side = 'receive'; c.version = options.version ?? 1; c.token = token; c.manifest = manifestDigest(files); c.reserved = reserved
    c.expires = Math.min(options.expires ?? r.now() + OFFER_MS, r.now() + OFFER_MS)
    const controller = new AbortController()
    let settled!: () => void
    const completed = new Promise<void>(resolve => { settled = resolve })
    const preparation = prepareReceiving(r, c, controller.signal)
    this.preparing.set(id, { c, controller, deadline: verificationDeadline(r.now(), c.role.kind === 'receiving' ? c.role.total : 0, c.expires), promise: completed })
    try {
      await preparation
      await this.enqueue(async () => {
        controller.signal.throwIfAborted()
        if (r.stopped) throw new TransportFailure('File connection stopped')
        let stream: ConnectionId
        try { stream = await r.tcp.connect(peer) } catch (error) { throw new TransportFailure(errorText(error)) }
        c.lastActivity = r.now()
        r.connections.set(stream, c)
      })
    } catch (error) {
      await cleanup(r, c, c.version === 2 && error instanceof TransportFailure)
      emit(r, { type: c.cancelled ? 'cancelled' : c.version === 2 && error instanceof TransportFailure ? 'paused' : 'failed', transferId: id, peer, error: errorText(error) })
      throw error
    } finally { this.opening.delete(id); this.preparing.delete(id); settled() }
  }
  async decline(id: string, token: string, senderPeer: string): Promise<void> {
    this.assertAvailable(id)
    validateClaim(id, token)
    const peer = normalizePeer(senderPeer), r = this.r
    if (peer === r.localPeer) throw new Error('Choose another device to receive these files')
    this.opening.add(id)
    try {
      await this.enqueue(async () => {
        const stream = await r.tcp.connect(peer)
        const c = newConnection(r, id, peer, { kind: 'decline' })
        c.pending = pendingControl({ type: 'decline', id, token })
        r.connections.set(stream, c)
      })
    } finally { this.opening.delete(id) }
  }
  async cancel(id: string): Promise<void> {
    const r = this.r, offer = r.offers.get(id)
    const preparing = this.preparing.get(id)
    if (preparing) { preparing.c.cancelled = true; preparing.controller.abort(new Error('File transfer cancelled')) }
    const active = [...r.connections.values()].filter(c => c.transferId === id && !c.terminal && c.claim?.status !== 'completed')
    if (r.ended.has(id) && !offer && !active.length && !preparing) return
    for (const c of active) c.cancelled = true
    r.offers.delete(id); rememberEnded(r, id)
    await this.enqueue(async () => {
      for (const c of active) {
        await terminalClaim(r, c, 'cancelled'); await cleanup(r, c)
        c.role = { kind: 'finish' }; c.pending = undefined; c.doneSent = false; c.terminal = true
        emit(r, { type: 'cancelled', transferId: id, peer: c.peer })
      }
      if (offer?.version === 2) {
        const previous = await r.options.store!.load(`send/${id}`)
        if (previous?.status === 'completed') { r.offers.set(id, offer); return }
        await saveClaim(r, 'send', id, { peer: previous?.peer ?? '', token: offer.token,
          manifest: manifestDigest(offer.files), expires: offer.expires, status: 'cancelled' })
      }
      const received = await r.options.store?.load(`receive/${id}`)
      if (received && received.status !== 'completed') await saveClaim(r, 'receive', id, { ...received, status: 'cancelled' })
      if (!active.length) emit(r, { type: 'cancelled', transferId: id, peer: '' })
    })
    await preparing?.promise.catch(() => undefined)
  }
  async dispose(): Promise<void> {
    const r = this.r
    if (r.stopped) return
    r.stopped = true; clearInterval(this.timer)
    for (const job of this.preparing.values()) job.controller.abort(new TransportFailure('File connection stopped'))
    await Promise.allSettled([...this.preparing.values()].map(job => job.promise))
    await this.operation
    for (const [stream, c] of r.connections) await this.fail(stream, c, new TransportFailure('File connection stopped'))
    for (const [id, offer] of r.offers) if (!r.ended.has(id)) {
      emit(r, { type: offer.version === 2 ? 'paused' : 'failed', transferId: id, peer: '', error: 'File connection stopped' })
    }
    r.offers.clear(); r.ended.clear()
    await r.tcp.dispose()
  }
  private assertAvailable(id: string, resume = false): void {
    const r = this.r
    if (r.stopped) throw new Error('File connection stopped')
    if (r.offers.has(id) || this.opening.has(id) || (!resume && r.ended.has(id)) ||
      [...r.connections.values()].some(c => c.transferId === id)) throw new Error('File offer is already in use')
    if (r.connections.size + this.opening.size >= MAX_TRANSFERS) throw new Error('Too many file transfers')
  }
  private async tick(): Promise<void> {
    const r = this.r
    if (r.stopped) return
    for (const job of this.preparing.values()) if (r.now() > job.deadline) job.controller.abort(new TransportFailure('File verification timed out'))
    for (const [id, offer] of r.offers) if (r.now() >= offer.expires) {
      r.offers.delete(id); rememberEnded(r, id)
      emit(r, { type: 'failed', transferId: id, peer: '', error: 'File offer expired' })
    }
    await r.tcp.poll()
    for (;;) {
      const stream = await r.tcp.accept()
      if (stream === undefined) break
      const remote = await r.tcp.peer(stream)
      let peer: string | undefined
      try { if (remote) peer = normalizePeer(remote) } catch {}
      if (!peer || peer === r.localPeer || r.connections.size >= MAX_TRANSFERS) await r.tcp.abort(stream)
      else r.connections.set(stream, newConnection(r, '', peer, { kind: 'request' }))
    }
    for (const [stream, c] of [...r.connections]) {
      try {
        if (!await this.progress(stream, c)) {
          r.connections.delete(stream)
          if (c.version === 1 || c.terminal) rememberEnded(r, c.transferId)
          await cleanup(r, c)
          try { await r.tcp.close(stream) } catch {}
        }
      } catch (error) { await this.fail(stream, c, error) }
    }
  }
  private async progress(stream: ConnectionId, c: Connection): Promise<boolean> {
    const r = this.r
    if (c.revoked) { await terminalClaim(r, c, 'cancelled'); throw new Error('Device authorization changed') }
    if (c.cancelled && !c.terminal) {
      await terminalClaim(r, c, 'cancelled'); await cleanup(r, c)
      c.role = { kind: 'finish' }; c.doneSent = false; c.terminal = true
      emit(r, { type: 'cancelled', transferId: c.transferId, peer: c.peer })
    }
    const state = await r.tcp.state(stream)
    if (!state) throw new TransportFailure('File connection closed')
    const deadline = c.work?.deadline ?? c.waitUntil ?? c.lastActivity + IDLE_MS
    if (r.now() > deadline) throw new TransportFailure('File transfer timed out')
    await pollWork(r, c)
    if (state === State.SynSent || state === State.SynReceived) return true
    if (state !== State.Established && state !== State.CloseWait) throw new TransportFailure('File connection closed')
    const bytes = await r.tcp.read(stream, DIRECT_FILE_CHUNK_BYTES + 1024)
    if (bytes.length) {
      c.lastActivity = r.now()
      for (const packet of c.reader.push(bytes)) {
        if (!await handlePacket(r, c, packet)) return false
        if (c.cancelled || c.revoked) return true
      }
    }
    if (state === State.CloseWait && c.role.kind !== 'finish' && c.role.kind !== 'decline') throw new TransportFailure('File connection closed before completion')
    if (c.pending?.marker) {
      const status = await r.tcp.markerStatus(c.pending.marker)
      if (status === MarkerStatus.ConnectionGone) throw new TransportFailure('File connection closed')
      if (status === MarkerStatus.Acked && c.pending.offset === c.pending.bytes.length) { c.pending = undefined; c.lastActivity = r.now() }
    }
    if (!c.pending) {
      if (c.role.kind === 'sending') {
        const chunk = await c.role.nextChunk()
        if (c.cancelled || c.revoked) return true
        if (chunk) c.pending = pendingBytes(directFileFrame(1, chunk))
        else if (!c.doneSent) {
          c.pending = pendingControl({ type: 'done' }); c.doneSent = true
          c.waitUntil = verificationDeadline(r.now(), c.role.total, c.claim?.expires ?? c.expires, 2)
        }
      } else if (c.role.kind === 'finish') {
        if (c.doneSent) {
          if (c.completion) { emit(r, c.completion); c.terminal = true; c.completion = undefined }
          return false
        }
        c.pending = pendingControl({ type: 'cancel' }); c.doneSent = true
      } else if (c.role.kind === 'decline') {
        c.terminal = true; emit(r, { type: 'declined', transferId: c.transferId, peer: c.peer }); return false
      }
    }
    if (c.pending && c.pending.offset < c.pending.bytes.length) {
      if (c.revoked || (c.cancelled && !c.terminal)) return true
      const result = await r.tcp.writeWithMarker(stream, c.pending.bytes.subarray(c.pending.offset))
      if (result.accepted) { c.pending.offset += result.accepted; c.pending.marker = result.marker; c.lastActivity = r.now() }
    }
    if (r.now() - c.lastProgress >= 250 && (c.role.kind === 'sending' || c.role.kind === 'receiving')) {
      c.lastProgress = r.now()
      emit(r, { type: 'progress', transferId: c.transferId, peer: c.peer,
        transferredBytes: c.role.transferred, totalBytes: c.role.total })
    }
    return true
  }
  private async fail(stream: ConnectionId, c: Connection, error: unknown): Promise<void> {
    const r = this.r, paused = c.version === 2 && error instanceof TransportFailure && !c.cancelled && !c.revoked
    r.connections.delete(stream)
    if (!paused && c.claim && c.claim.status !== 'completed') await terminalClaim(r, c, 'cancelled')
    await cleanup(r, c, paused)
    if (!paused) rememberEnded(r, c.transferId)
    if (c.transferId && !c.terminal) {
      c.terminal = true
      emit(r, { type: paused ? 'paused' : 'failed', transferId: c.transferId, peer: c.peer, error: errorText(error) })
    }
    try { await r.tcp.abort(stream) } catch {}
  }
  private enqueue<T>(work: () => Promise<T>): Promise<T> {
    const result = this.operation.then(work, work)
    this.operation = result.catch(() => undefined)
    return result
  }
  private report(error: unknown): void { this.r.options.onError?.(error instanceof Error ? error : new Error(errorText(error))) }
}
