// Keep the overlay, faults and sparse placements aligned with the Rust mesh-scale test.
export const NODE_COUNT = 10_000;
export const HOP_LIMIT = 5;
export const TIMEOUT_MS = 700;
export const PAYLOAD_BYTES = 4096;
export const PROFILES = ['steady', 'churn', 'adversarial'];
export const FRESH_SEEDS = [2281, 2333, 2377, 2423, 2473, 2521, 2579, 2621, 2671, 2729, 2777, 2833];

function mix(value) {
  value = BigInt.asUintN(64, (value ^ (value >> 30n)) * 0xbf58476d1ce4e5b9n);
  value = BigInt.asUintN(64, (value ^ (value >> 27n)) * 0x94d049bb133111ebn);
  return value ^ (value >> 31n);
}

export function createLayout(seedNumber, profile, direct) {
  const seed = BigInt(seedNumber);
  const size = BigInt(NODE_COUNT);
  const adjacency = Array.from({ length: NODE_COUNT }, () => new Set());
  for (let node = 0; node < NODE_COUNT; node++) {
    const shortcut = Number(mix(seed ^ BigInt(node)) % size);
    for (const other of [(node + 1) % NODE_COUNT, (node + 2) % NODE_COUNT, shortcut]) {
      if (node === other) continue;
      adjacency[node].add(other);
      adjacency[other].add(node);
    }
  }
  const churn = profile === 'churn';
  const edges = adjacency.map((peers, node) => {
    const link = (peer) => mix((seed << 32n) ^ (BigInt(node) << 16n) ^ BigInt(peer));
    const candidates = [...peers].map((to) => ({ to, sample: link(to) }));
    candidates.sort((a, b) => a.sample < b.sample ? -1 : 1);
    return (direct ? candidates.slice(0, 4) : candidates).map(({ to, sample }) => ({
      to,
      delay: 6 + Number(sample % 19n) + (churn && sample % 11n === 0n ? 120 : 0),
      stalled: churn && sample % 23n === 0n,
    }));
  });
  const peers = adjacency.map((_, node) => ({
    departure: churn && mix(seed ^ BigInt(node)) % 8n === 0n,
    behavior: profile === 'adversarial' ? Number(mix(seed ^ 0xa700n ^ BigInt(node)) % 10n) : 9,
  }));
  const online = (node, batch) => batch < 4 || !peers[node].departure;
  const honest = (node, batch) => peers[node].behavior > 1 && online(node, batch);

  const reachable = (reader, holders, batch) => {
    if (!holders.length) return false;
    let frontier = [reader];
    const seen = new Set(frontier);
    for (let hop = 0; hop < (direct ? 1 : HOP_LIMIT); hop++) {
      const next = [];
      for (const node of frontier) {
        for (const edge of edges[node]) {
          if (edge.stalled || seen.has(edge.to) || !honest(edge.to, batch)) continue;
          if (holders.includes(edge.to)) return true;
          seen.add(edge.to);
          next.push(edge.to);
        }
      }
      frontier = next;
    }
    return false;
  };

  const batchReads = (batch) => Array.from({ length: 8 }, (_, slot) => {
    const burst = batch === 2 || batch === 6;
    const id = batch * 8 + (burst ? 0 : slot);
    const sample = mix(BigInt.asUintN(64, seed * 10_007n) ^ BigInt(id));
    let reader = Number(sample % size);
    while (!honest(reader, batch)) reader = (reader + 1) % NODE_COUNT;
    const replicas = burst || slot >= 4 ? 1 : slot === 0 ? 0 : 3;
    const holders = [];
    for (let replica = 0; replica < replicas; replica++) {
      let holder = Number(mix(BigInt.asUintN(64, sample + BigInt(replica))) % size);
      // The browser case also needs hits at its adjacent cache. Use the same
      // sparse placement in the other half of its reads.
      if (direct && replica === 0 && id % 2 === 0) {
        const adjacent = edges[reader][Number(sample % BigInt(edges[reader].length))];
        holder = adjacent.to;
      }
      while (holder === reader || holders.includes(holder)) holder = (holder + 1) % NODE_COUNT;
      holders.push(holder);
    }
    return { id, reader, holders, reachable: reachable(reader, holders, batch) };
  });
  return { edges, peers, online, batchReads };
}
