import animals from './data/animals.json';
import adjectives from './data/adjectives.json';

function capitalize(s: string): string {
  if (typeof s !== 'string') return '';
  return s.charAt(0).toUpperCase() + s.slice(1);
}

// Simple deterministic hash from string - just needs to be consistent, not cryptographic
function simpleHash(str: string): [number, number] {
  let h1 = 0;
  let h2 = 0;
  for (let i = 0; i < str.length; i++) {
    const c = str.charCodeAt(i);
    h1 = (h1 * 31 + c) >>> 0;
    h2 = (h2 * 37 + c) >>> 0;
  }
  return [h1 & 0xff, h2 & 0xff];
}

/**
 * Deterministically create adjective + animal names from a seed (pubkey)
 */
export function animalName(seed: string): string {
  if (!seed) {
    throw new Error('No seed provided');
  }
  const [h1, h2] = simpleHash(seed);
  const adjective = adjectives[h1 % adjectives.length];
  const animal = animals[h2 % animals.length];
  return `${capitalize(adjective)} ${capitalize(animal)}`;
}
