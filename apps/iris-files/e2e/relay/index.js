/**
 * Simple in-memory Nostr relay for e2e testing
 * Based on https://github.com/coracle-social/bucket
 *
 * This relay stores events in memory and broadcasts to subscribers.
 * Events are cleared every 30 seconds to prevent memory buildup during tests.
 */
import http from 'http'
import fs from 'fs'
import { matchFilters } from 'nostr-tools'
import { WebSocketServer } from 'ws'

const DEBUG = process.env.RELAY_DEBUG === '1'
const log = (...args) => {
  if (!DEBUG) return
  const msg = args.map(a => typeof a === 'object' ? JSON.stringify(a) : a).join(' ')
  const line = `${new Date().toISOString()} ${msg}\n`
  fs.appendFile('/tmp/relay-debug.log', line, () => {})
  console.log(...args)
}

const PORT = process.env.RELAY_PORT || 4736
const HANDSHAKE_DELAY_MS = Number(process.env.RELAY_HANDSHAKE_DELAY_MS || 0)

const server = http.createServer((req, res) => {
  if (req.url === '/' && req.headers.accept === 'application/nostr+json') {
    res.writeHead(200, {
      'Content-Type': 'application/nostr+json',
      'Access-Control-Allow-Origin': '*',
      'Access-Control-Allow-Headers': '*',
      'Access-Control-Allow-Methods': '*'
    });

    res.end(JSON.stringify({
      name: 'hashtree-test-relay',
      description: 'Local relay for e2e testing',
      software: 'https://github.com/coracle-social/bucket',
      supported_nips: [1, 11],
    }))
  } else {
    res.writeHead(200, {
      'Access-Control-Allow-Origin': '*',
      'Access-Control-Allow-Headers': '*',
      'Access-Control-Allow-Methods': '*'
    })
    res.end('hashtree-test-relay')
  }
})

const namespaces = new Map()

function getNamespace(url) {
  if (!url) return '/'
  let path = url.split('?')[0] || '/'
  if (path !== '/' && path.endsWith('/')) {
    path = path.replace(/\/+$/, '')
  }
  return path === '' ? '/' : path
}

function getNamespaceState(ns) {
  let state = namespaces.get(ns)
  if (!state) {
    state = { gsubs: new Map(), events: new Map() }
    namespaces.set(ns, state)
  }
  return state
}
const wss = new WebSocketServer({ noServer: true })

server.on('upgrade', (req, socket, head) => {
  const handleUpgrade = () => {
    wss.handleUpgrade(req, socket, head, (ws) => {
      wss.emit('connection', ws, req)
    })
  }

  if (HANDSHAKE_DELAY_MS > 0) {
    setTimeout(handleUpgrade, HANDSHAKE_DELAY_MS)
  } else {
    handleUpgrade()
  }
})

// Clear events every 5 minutes to prevent memory buildup
// (Longer interval keeps data available across longer e2e test runs)
setInterval(() => {
  for (const state of namespaces.values()) {
    state.events.clear()
  }
}, 300_000)

wss.on('connection', (socket, req) => {
  const ns = getNamespace(req?.url)
  const { gsubs, events } = getNamespaceState(ns)
  const conid = Math.random().toString().slice(2)
  const lsubs = new Map()
  log(`[relay] New connection: ${conid} ns=${ns}`)

  const send = msg => {
    try {
      socket.send(JSON.stringify(msg))
    } catch (e) {
      // Ignore send errors (socket may be closed)
    }
  }

  const makecb = (lsubid, filters, gsubid) => event => {
    const matches = matchFilters(filters, event)
    if (matches) {
      log(`[relay] MATCH sub=${lsubid} event.kind=${event.kind} id=${event.id?.slice(0,8)}`)
      send(['EVENT', lsubid, event])
    }
  }

  socket.on('message', msg => {
    try {
      const message = JSON.parse(msg)

      if (message[0] === 'EVENT') {
        const event = message[1]
        log(`[relay] EVENT kind=${event.kind} pubkey=${event.pubkey?.slice(0,8)} tags=${JSON.stringify(event.tags?.slice(0,3))}`)

        events.set(event.id, event)

        let matchCount = 0
        for (const [gsubid, cb] of gsubs.entries()) {
          cb(event)
          matchCount++
        }
        log(`[relay] Broadcast to ${matchCount} subscribers`)

        send(['OK', event.id, true, ''])
      }

      if (message[0] === 'REQ') {
        const lsubid = message[1]
        const gsubid = `${conid}:${lsubid}`
        const filters = message.slice(2)
        log(`[relay] REQ sub=${lsubid} filters=${JSON.stringify(filters).slice(0,200)}`)

        lsubs.set(lsubid, gsubid)
        gsubs.set(gsubid, makecb(lsubid, filters, gsubid))

        let matchedExisting = 0
        for (const event of events.values()) {
          if (matchFilters(filters, event)) {
            log(`[relay] EXISTING MATCH sub=${lsubid} event.kind=${event.kind} pubkey=${event.pubkey?.slice(0,8)}`)
            send(['EVENT', lsubid, event])
            matchedExisting++
          }
        }
        log(`[relay] EOSE sub=${lsubid} matched=${matchedExisting} total_events=${events.size}`)

        send(['EOSE', lsubid])
      }

      if (message[0] === 'CLOSE') {
        const lsubid = message[1]
        const gsubid = `${conid}:${lsubid}`

        lsubs.delete(lsubid)
        gsubs.delete(gsubid)
      }
    } catch (e) {
      // Ignore parse errors
    }
  })

  socket.on('close', () => {
    for (const [subid, gsubid] of lsubs.entries()) {
      gsubs.delete(gsubid)
    }

    lsubs.clear()
  })
})

const HOST = process.env.TEST_RELAY_HOST || '127.0.0.1'
server.listen(PORT, HOST, () => {
  log(`[test-relay] Running on ws://localhost:${PORT}`)
})
