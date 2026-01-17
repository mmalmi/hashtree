import type { ActivePlace, SearchResult } from '../stores/mapsStore.svelte';
import { getQueryParamsFromHash } from './router.svelte';

type PlaceLike = Pick<ActivePlace, 'name' | 'lat' | 'lng' | 'type' | 'class' | 'displayName' | 'boundingBox' | 'placeRank'>;

export function parseActivePlaceFromHash(hash: string): ActivePlace | null {
  const placeParam = getQueryParamsFromHash(hash).get('place');
  if (!placeParam) return null;
  try {
    const parsed = JSON.parse(placeParam) as PlaceLike;
    if (typeof parsed.lat !== 'number' || typeof parsed.lng !== 'number' || !parsed.name) {
      return null;
    }
    return {
      name: parsed.name,
      lat: parsed.lat,
      lng: parsed.lng,
      type: parsed.type,
      class: parsed.class,
      displayName: parsed.displayName,
      boundingBox: Array.isArray(parsed.boundingBox) && parsed.boundingBox.length === 4
        ? parsed.boundingBox
        : undefined,
      placeRank: typeof parsed.placeRank === 'number' ? parsed.placeRank : undefined,
      source: 'search',
    };
  } catch {
    return null;
  }
}

export function setActivePlaceInHash(place: SearchResult | PlaceLike | null) {
  const hash = window.location.hash.slice(1) || '/';
  const [path] = hash.split('?');
  const params = getQueryParamsFromHash(hash);
  if (place) {
    params.set('place', JSON.stringify({
      name: place.name,
      lat: place.lat,
      lng: place.lng,
      type: place.type,
      class: place.class,
      displayName: place.displayName,
      boundingBox: place.boundingBox,
      placeRank: place.placeRank,
    }));
  } else {
    params.delete('place');
  }
  const nextHash = params.toString() ? `${path}?${params.toString()}` : path;
  if (window.location.hash.slice(1) !== nextHash) {
    window.location.hash = `#${nextHash}`;
  }
}
