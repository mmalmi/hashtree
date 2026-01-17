/**
 * Maps store - manages places, annotations, and audience filter state
 */
import { cid, fromHex, toHex, LinkType, type CID } from 'hashtree';
import { getTree } from '../store';
import { getRefResolver } from '../refResolver';
import { updateLocalRootCache } from '../treeRootCache';
import { nostrStore } from '../nostr';
import { getWorkerAdapter } from '../lib/workerInit';

const MAP_VIEW_KEY = 'iris-maps-view';
const DEFAULT_CENTER: [number, number] = [20, 0]; // World view
const DEFAULT_ZOOM = 2;
const MAPS_TREE_NAME = 'maps';
const MAPS_DATA_FILE = 'maps.json';
const MAPS_DATA_VERSION = 1;
const MAPS_META_FILE = 'meta.json';
const MAPS_META_VERSION = 1;
const MAPS_ROOT_STORAGE_KEY = 'iris-maps-root:maps';

interface MapsPersistedData {
  version: number;
  places: Place[];
  annotations: Annotation[];
}

interface MapsMeta {
  version: number;
  createdAt: number;
  updatedAt: number;
  providers: string[];
}

export interface MapView {
  center: [number, number]; // [lat, lng]
  zoom: number;
  bounds?: [number, number, number, number]; // [south, north, west, east]
}

function loadMapView(): MapView {
  try {
    const saved = localStorage.getItem(MAP_VIEW_KEY);
    if (saved) {
      const parsed = JSON.parse(saved) as Partial<MapView>;
      if (Array.isArray(parsed.center) && typeof parsed.zoom === 'number') {
        const bounds = Array.isArray(parsed.bounds) && parsed.bounds.length === 4
          ? parsed.bounds as [number, number, number, number]
          : undefined;
        return { center: parsed.center as [number, number], zoom: parsed.zoom, bounds };
      }
    }
  } catch {
    // Ignore parse errors
  }
  return { center: DEFAULT_CENTER, zoom: DEFAULT_ZOOM };
}

function saveMapView(view: MapView) {
  try {
    localStorage.setItem(MAP_VIEW_KEY, JSON.stringify(view));
  } catch {
    // Ignore storage errors
  }
}

function normalizeSearchText(value: string): string {
  return value
    .toLowerCase()
    .normalize('NFD')
    .replace(/[\u0300-\u036f]/g, '')
    .replace(/[^a-z0-9]+/g, ' ')
    .trim();
}

function normalizeLanguage(value: string | undefined): string {
  if (!value) return 'en';
  const primary = value.split(',')[0]?.trim();
  if (!primary) return 'en';
  return primary.split('-')[0]?.toLowerCase() || 'en';
}


export interface SearchResult {
  name: string;
  lat: number;
  lng: number;
  type: string;
  class: string;
  displayName: string;
  boundingBox?: [number, number, number, number]; // [south, north, west, east]
  placeRank?: number;
  importance?: number;
  addressType?: string;
}

export function getSearchResultKey(result: SearchResult): string {
  const name = result.displayName || result.name;
  return `${name.toLowerCase()}-${result.lat.toFixed(5)}-${result.lng.toFixed(5)}-${result.class}-${result.type}`;
}

export interface ActivePlace {
  name: string;
  lat: number;
  lng: number;
  type?: string;
  class?: string;
  displayName?: string;
  boundingBox?: [number, number, number, number];
  placeRank?: number;
  source: 'search' | 'place';
}

export type PlaceCategory =
  | 'restaurant'
  | 'cafe'
  | 'bar'
  | 'shop'
  | 'service'
  | 'attraction'
  | 'accommodation'
  | 'transport'
  | 'other';

export interface Place {
  id: string;
  name: string;
  category: PlaceCategory;
  lat: number;
  lng: number;
  description?: string;
  contact?: string;
  hours?: string;
  tags?: string[];
  createdAt: number;
  updatedAt: number;
  owner: string; // npub of owner
}

export interface Annotation {
  id: string;
  type: 'point' | 'line' | 'polygon';
  coordinates: [number, number][] | [number, number]; // [lng, lat] or array of them
  name?: string;
  description?: string;
  color?: string;
  createdAt: number;
  updatedAt: number;
  owner: string;
}

export type AudienceFilter = 'own' | 'follows' | 'fof';

interface MapsState {
  places: Place[];
  annotations: Annotation[];
  audienceFilter: AudienceFilter;
  selectedPlace: Place | null;
  isAddPlaceModalOpen: boolean;
  pendingPlaceLocation: { lat: number; lng: number } | null;
  mapView: MapView;
  searchQuery: string;
  searchResults: SearchResult[];
  isSearching: boolean;
  activePlace: ActivePlace | null;
}

const savedView = loadMapView();

const initialState: MapsState = {
  places: [],
  annotations: [],
  audienceFilter: 'follows',
  selectedPlace: null,
  isAddPlaceModalOpen: false,
  pendingPlaceLocation: null,
  mapView: savedView,
  searchQuery: '',
  searchResults: [],
  isSearching: false,
  activePlace: null,
};

let state = $state<MapsState>({ ...initialState });

let activeNpub: string | null = null;
let rootCid: CID | null = null;
let resolverUnsubscribe: (() => void) | null = null;
let nostrUnsubscribe: (() => void) | null = null;
let applyingRemote = false;
let persistChain: Promise<void> = Promise.resolve();

function resetLocalState() {
  state.places = [];
  state.annotations = [];
  state.selectedPlace = null;
  rootCid = null;
}

function applyRemoteData(data: MapsPersistedData) {
  applyingRemote = true;
  state.places = data.places;
  state.annotations = data.annotations;
  state.selectedPlace = null;
  applyingRemote = false;
}

function normalizePersistedData(raw: unknown): MapsPersistedData | null {
  if (!raw || typeof raw !== 'object') return null;
  const record = raw as Partial<MapsPersistedData>;
  if (!Array.isArray(record.places) || !Array.isArray(record.annotations)) return null;
  return {
    version: typeof record.version === 'number' ? record.version : MAPS_DATA_VERSION,
    places: record.places as Place[],
    annotations: record.annotations as Annotation[],
  };
}

function getRootStorageKey(npub: string): string {
  return `${MAPS_ROOT_STORAGE_KEY}:${npub}`;
}

function loadMapsRoot(npub: string): CID | null {
  try {
    const stored = localStorage.getItem(getRootStorageKey(npub));
    if (!stored) return null;
    const parsed = JSON.parse(stored) as { hash?: string; key?: string };
    if (parsed?.hash) {
      return cid(fromHex(parsed.hash), parsed.key ? fromHex(parsed.key) : undefined);
    }
  } catch {
    // Ignore storage errors
  }
  return null;
}

function saveMapsRoot(npub: string, root: CID) {
  try {
    localStorage.setItem(
      getRootStorageKey(npub),
      JSON.stringify({ hash: toHex(root.hash), key: root.key ? toHex(root.key) : undefined })
    );
  } catch {
    // Ignore storage errors
  }
}

async function waitForWorkerReady(maxWaitMs = 5000): Promise<boolean> {
  const start = Date.now();
  while (!getWorkerAdapter()) {
    if (Date.now() - start > maxWaitMs) {
      return false;
    }
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  return true;
}

let workerReadyPromise: Promise<boolean> | null = null;

async function ensureWorkerReady(maxWaitMs = 5000): Promise<boolean> {
  if (!workerReadyPromise) {
    workerReadyPromise = waitForWorkerReady(maxWaitMs);
  }
  const ready = await workerReadyPromise;
  if (!ready) {
    workerReadyPromise = null;
  }
  return ready;
}

async function ensureMapsMeta(root: CID): Promise<CID> {
  const tree = getTree();
  const existing = await tree.resolvePath(root, [MAPS_META_FILE]);
  if (existing) return root;
  const meta: MapsMeta = {
    version: MAPS_META_VERSION,
    createdAt: Date.now(),
    updatedAt: Date.now(),
    providers: [],
  };
  const encoded = new TextEncoder().encode(JSON.stringify(meta));
  const { cid: fileCid, size } = await tree.putFile(encoded);
  return tree.setEntry(root, [], MAPS_META_FILE, fileCid, size, LinkType.File);
}


function queueRootUpdate(action: (root: CID) => Promise<CID>): Promise<void> {
  const npub = activeNpub ?? nostrStore.getState().npub;
  if (applyingRemote || !npub) return Promise.resolve();
  activeNpub = npub;
  persistChain = persistChain.then(async () => {
    const ready = await ensureWorkerReady();
    if (!ready) return;
    const tree = getTree();
    let currentRoot = rootCid ?? loadMapsRoot(npub);
    if (!currentRoot) {
      const { cid: emptyRoot } = await tree.putDirectory([]);
      currentRoot = await ensureMapsMeta(emptyRoot);
    } else {
      currentRoot = await ensureMapsMeta(currentRoot);
    }
    const nextRoot = await action(currentRoot);
    rootCid = nextRoot;
    saveMapsRoot(npub, nextRoot);
    updateLocalRootCache(npub, MAPS_TREE_NAME, nextRoot.hash, nextRoot.key, 'public');
  }).catch(() => {});
  return persistChain;
}

async function loadMapsData(cid: CID) {
  const ready = await waitForWorkerReady();
  if (!ready) return;
  const tree = getTree();
  const entry = await tree.resolvePath(cid, [MAPS_DATA_FILE]);
  if (!entry) {
    applyRemoteData({ version: MAPS_DATA_VERSION, places: [], annotations: [] });
    return;
  }
  const data = await tree.readFile(entry.cid);
  if (!data) return;
  try {
    const decoded = new TextDecoder().decode(data);
    const parsed = JSON.parse(decoded);
    const normalized = normalizePersistedData(parsed);
    if (normalized) {
      applyRemoteData(normalized);
    }
  } catch {
    // Ignore invalid data
  }
}

function dedupeResults(items: SearchResult[]): SearchResult[] {
  const deduped = new Map<string, SearchResult>();
  for (const item of items) {
    const key = getSearchResultKey(item);
    if (!deduped.has(key)) {
      deduped.set(key, item);
    }
  }
  return Array.from(deduped.values());
}

function distanceKm(aLat: number, aLng: number, bLat: number, bLng: number) {
  const toRad = (value: number) => value * Math.PI / 180;
  const dLat = toRad(bLat - aLat);
  const dLng = toRad(bLng - aLng);
  const lat1 = toRad(aLat);
  const lat2 = toRad(bLat);
  const sinDLat = Math.sin(dLat / 2);
  const sinDLng = Math.sin(dLng / 2);
  const a = sinDLat * sinDLat + Math.cos(lat1) * Math.cos(lat2) * sinDLng * sinDLng;
  return 6371 * 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a));
}

function areaKm2(box?: [number, number, number, number]) {
  if (!box) return 0;
  const [south, north, west, east] = box;
  const height = distanceKm(south, west, north, west);
  const midLat = (south + north) / 2;
  const width = distanceKm(midLat, west, midLat, east);
  return Math.max(0, height * width);
}

function isWithinBounds(lat: number, lng: number, box?: [number, number, number, number]) {
  if (!box) return false;
  const [south, north, west, east] = box;
  const withinLat = lat >= south && lat <= north;
  const withinLng = west <= east
    ? lng >= west && lng <= east
    : lng >= west || lng <= east;
  return withinLat && withinLng;
}

function isSettlementResult(result: SearchResult) {
  return result.class === 'place'
    || ['city', 'town', 'village', 'hamlet'].includes(result.type);
}

function isBoundaryResult(result: SearchResult) {
  return result.class === 'boundary' || result.type === 'administrative';
}

function isAddressResult(result: SearchResult) {
  return result.addressType === 'house'
    || result.addressType === 'building'
    || result.class === 'building'
    || result.type === 'house';
}

function isCountryResult(result: SearchResult) {
  return result.addressType === 'country'
    || (isBoundaryResult(result) && (result.placeRank ?? 99) <= 4);
}

function isRegionResult(result: SearchResult) {
  return result.addressType === 'state'
    || (isBoundaryResult(result) && (result.placeRank ?? 99) <= 8);
}

function matchScore(result: SearchResult, normalizedQuery: string) {
  const nameText = normalizeSearchText(result.name);
  const displayText = normalizeSearchText(result.displayName);
  if (!normalizedQuery) return 0;
  if (nameText === normalizedQuery) return 1000;
  if (displayText === normalizedQuery) return 950;
  if (nameText.startsWith(normalizedQuery)) return 900;
  const nameTokens = nameText.split(' ');
  const displayTokens = displayText.split(' ');
  if (nameTokens.includes(normalizedQuery) || displayTokens.includes(normalizedQuery)) return 850;
  if (nameTokens.some(token => token.startsWith(normalizedQuery))) return 780;
  if (displayTokens.some(token => token.startsWith(normalizedQuery))) return 730;
  if (displayText.startsWith(normalizedQuery)) return 700;
  if (nameText.includes(normalizedQuery)) return 620;
  if (displayText.includes(normalizedQuery)) return 520;
  return 0;
}

function rankSearchResults(
  results: SearchResult[],
  normalizedQuery: string,
  origin: { lat: number; lng: number },
  bounds?: [number, number, number, number]
): SearchResult[] {
  return results
    .map(result => {
      const distance = distanceKm(origin.lat, origin.lng, result.lat, result.lng);
      const within = isWithinBounds(result.lat, result.lng, bounds);
      const importance = result.importance ?? 0;
      const isHighway = result.class === 'highway' || result.type === 'road';
      const isBoundary = isBoundaryResult(result);
      const isSettlement = isSettlementResult(result);
      const isAddress = isAddressResult(result);
      const isCountry = isCountryResult(result);
      const match = matchScore(result, normalizedQuery);
      const queryLength = normalizedQuery.length;
      const placeRank = result.placeRank ?? 30;
      const placeRankBoost = Math.max(0, 24 - placeRank) * 12;
      const placeRankPenalty = placeRank > 26 ? (placeRank - 26) * 18 : 0;
      const cityBoost = result.type === 'city' ? 120
        : result.type === 'town' ? 60
          : result.type === 'village' ? 20
            : 0;
      const highwayPenalty = (isHighway && queryLength <= 10 && match < 850) ? 180 : 0;
      const boundaryPenalty = (isBoundary && queryLength <= 10 && match < 850) ? 60 : 0;
      const addressPenalty = (isAddress && queryLength <= 16 && match < 900) ? 260 : 0;
      const settlementBoost = (isSettlement && queryLength <= 12) ? 80 : 0;
      const area = areaKm2(result.boundingBox);
      const areaBoost = area > 0 ? Math.min(200, Math.log10(area + 1) * 18) : 0;
      const adminBoost = isCountry ? 320 : isRegionResult(result) ? 160 : 0;
      const withinBoost = within ? (match >= 850 ? 40 : 120) : 0;
      const distancePenalty = isCountry
        ? (match >= 850 ? distance / 200 : distance / 80)
        : match >= 850
          ? distance / 120
          : match >= 700
            ? distance / 30
            : distance / 12;
      const score = withinBoost
        + (importance * 60)
        + match
        + adminBoost
        + areaBoost
        + settlementBoost
        + cityBoost
        + placeRankBoost
        - distancePenalty
        - highwayPenalty
        - boundaryPenalty
        - addressPenalty
        - placeRankPenalty;
      return { result, score };
    })
    .sort((a, b) => b.score - a.score)
    .slice(0, 10)
    .map(entry => entry.result);
}

function queuePersist(): Promise<void> {
  return queueRootUpdate(persistMapsData);
}

async function persistMapsData(currentRoot: CID): Promise<CID> {
  const payload: MapsPersistedData = {
    version: MAPS_DATA_VERSION,
    places: state.places,
    annotations: state.annotations,
  };
  const encoded = new TextEncoder().encode(JSON.stringify(payload));
  const tree = getTree();
  const { cid: fileCid, size } = await tree.putFile(encoded);
  return tree.setEntry(currentRoot, [], MAPS_DATA_FILE, fileCid, size, LinkType.File);
}

function subscribeToMapsTree(npub: string) {
  if (resolverUnsubscribe) {
    resolverUnsubscribe();
    resolverUnsubscribe = null;
  }

  const resolver = getRefResolver();
  const key = `${npub}/${MAPS_TREE_NAME}`;
  resolverUnsubscribe = resolver.subscribe?.(key, (cid) => {
    if (!cid) {
      resetLocalState();
      return;
    }
    rootCid = cid;
    loadMapsData(cid).catch(() => {});
  }) ?? null;
}

function handleNostrChange() {
  const nextNpub = nostrStore.getState().npub;
  if (nextNpub === activeNpub) return;
  activeNpub = nextNpub;
  if (!activeNpub) {
    resetLocalState();
    if (resolverUnsubscribe) {
      resolverUnsubscribe();
      resolverUnsubscribe = null;
    }
    return;
  }
  const cachedRoot = loadMapsRoot(activeNpub);
  if (cachedRoot) {
    rootCid = cachedRoot;
    loadMapsData(cachedRoot).catch(() => {});
  }
  subscribeToMapsTree(activeNpub);
}

export const mapsStore = {
  get places() { return state.places; },
  get annotations() { return state.annotations; },
  get audienceFilter() { return state.audienceFilter; },
  get selectedPlace() { return state.selectedPlace; },
  get isAddPlaceModalOpen() { return state.isAddPlaceModalOpen; },
  get pendingPlaceLocation() { return state.pendingPlaceLocation; },
  get mapView() { return state.mapView; },
  get searchQuery() { return state.searchQuery; },
  get searchResults() { return state.searchResults; },
  get isSearching() { return state.isSearching; },
  get activePlace() { return state.activePlace; },
  init() {
    if (nostrUnsubscribe) return;
    handleNostrChange();
    nostrUnsubscribe = nostrStore.subscribe(() => {
      handleNostrChange();
    });
  },
  setActivePlace(place: ActivePlace | null) {
    state.activePlace = place;
  },
  destroy() {
    if (nostrUnsubscribe) {
      nostrUnsubscribe();
      nostrUnsubscribe = null;
    }
    if (resolverUnsubscribe) {
      resolverUnsubscribe();
      resolverUnsubscribe = null;
    }
  },

  setMapView(center: [number, number], zoom: number, bounds?: [number, number, number, number]) {
    state.mapView = { center, zoom, bounds };
    saveMapView(state.mapView);
  },

  async search(query: string) {
    state.searchQuery = query;
    const trimmed = query.trim();
    if (!trimmed || trimmed.length < 2) {
      state.searchResults = [];
      return;
    }

    state.isSearching = true;
    const normalizedQuery = normalizeSearchText(trimmed);
    const isAsciiQuery = /^[a-z0-9 ]+$/.test(normalizedQuery);
    try {
      const viewBounds = state.mapView.bounds;
      const normalizeLng = (lng: number) => {
        const wrapped = ((lng + 180) % 360 + 360) % 360 - 180;
        return Number.isFinite(wrapped) ? wrapped : lng;
      };
      const clampLat = (lat: number) => Math.max(-90, Math.min(90, lat));
      const origin = state.activePlace
        ? { lat: clampLat(state.activePlace.lat), lng: normalizeLng(state.activePlace.lng) }
        : { lat: clampLat(state.mapView.center[0]), lng: normalizeLng(state.mapView.center[1]) };
      const language = typeof navigator !== 'undefined' ? navigator.language : '';
      const languageCode = normalizeLanguage(language);

      const fetchNominatim = async (
        searchQuery = trimmed,
        languageOverride?: string,
        useLocation = true
      ) => {
        const params = new URLSearchParams({
          format: 'jsonv2',
          q: searchQuery,
          limit: '50',
        });
        if (useLocation) {
          params.set('bounded', '0');
          params.set('lat', origin.lat.toString());
          params.set('lon', origin.lng.toString());
          if (viewBounds) {
            const [south, north, west, east] = viewBounds;
            params.set('viewbox', `${west},${north},${east},${south}`);
          }
        }
        const requestLanguage = languageOverride ?? language;
        if (requestLanguage) {
          params.set('accept-language', requestLanguage);
        }
        const response = await fetch(
          `https://nominatim.openstreetmap.org/search?${params.toString()}`,
          { headers: { 'User-Agent': 'IrisMaps/1.0' } }
        );
        const data = await response.json();
        return data.map((item: {
          display_name: string;
          lat: string;
          lon: string;
          type: string;
          class: string;
          name?: string;
          boundingbox?: string[];
          place_rank?: number;
          importance?: number;
          addresstype?: string;
        }) => ({
          name: item.name || item.display_name.split(',')[0],
          lat: parseFloat(item.lat),
          lng: parseFloat(item.lon),
          type: item.type,
          class: item.class,
          displayName: item.display_name,
          boundingBox: item.boundingbox ? [
            parseFloat(item.boundingbox[0]), // south
            parseFloat(item.boundingbox[1]), // north
            parseFloat(item.boundingbox[2]), // west
            parseFloat(item.boundingbox[3]), // east
          ] as [number, number, number, number] : undefined,
          placeRank: typeof item.place_rank === 'number' ? item.place_rank : undefined,
          importance: typeof item.importance === 'number' ? item.importance : undefined,
          addressType: item.addresstype,
        })) as SearchResult[];
      };

      const fetchPhoton = async (useLocation = true) => {
        const params = new URLSearchParams({
          q: trimmed,
          limit: '50',
        });
        if (useLocation) {
          params.set('lat', origin.lat.toString());
          params.set('lon', origin.lng.toString());
        }
        if (language) {
          const lang = language.split('-')[0]?.toLowerCase();
          if (lang === 'en' || lang === 'de' || lang === 'fr') {
            params.set('lang', lang);
          }
        }
        const response = await fetch(
          `https://photon.komoot.io/api/?${params.toString()}`,
          { headers: { 'User-Agent': 'IrisMaps/1.0' } }
        );
        const data = await response.json();
        return (data?.features ?? []).map((item: {
          geometry: { coordinates: [number, number] };
          properties: {
            name?: string;
            type?: string;
            osm_key?: string;
            city?: string;
            state?: string;
            country?: string;
            extent?: [number, number, number, number];
          };
        }) => {
          const [lng, lat] = item.geometry.coordinates;
          const name = item.properties.name ?? '';
          const parts = [name, item.properties.city, item.properties.state, item.properties.country]
            .filter(Boolean);
          const extent = item.properties.extent;
          return {
            name: name || parts[0] || 'Unknown',
            lat,
            lng,
            type: item.properties.type ?? 'place',
            class: item.properties.osm_key ?? 'place',
            displayName: parts.join(', '),
            boundingBox: extent ? [
              extent[1], // south
              extent[3], // north
              extent[0], // west
              extent[2], // east
            ] as [number, number, number, number] : undefined,
          } as SearchResult;
        });
      };

      let primaryResults: SearchResult[] = [];
      let fetchedNominatim = false;
      try {
        primaryResults = await fetchNominatim();
        fetchedNominatim = true;
      } catch {
        primaryResults = [];
      }

      let results = primaryResults;
      const shouldFetchPhoton = trimmed.length <= 6 || primaryResults.length < 5;
      if (shouldFetchPhoton) {
        try {
          const photonResults = await fetchPhoton();
          results = dedupeResults([...results, ...photonResults]);
        } catch {
          // Ignore photon errors
        }
      }

      const isLowQualityMatch = (result: SearchResult, score: number) => {
        if (score < 850) return true;
        if (isAddressResult(result)) return true;
        if (result.class === 'highway' || result.type === 'road') return true;
        return false;
      };

      let hasQualityMatch = results.some((result) => {
        const score = matchScore(result, normalizedQuery);
        return !isLowQualityMatch(result, score);
      });

      let usedGlobalFallback = false;
      if (!hasQualityMatch) {
        usedGlobalFallback = true;
        try {
          const globalNominatim = await fetchNominatim(trimmed, undefined, false);
          results = dedupeResults([...results, ...globalNominatim]);
        } catch {
          // Ignore global fetch errors
        }

        const shouldFetchPhotonGlobal = shouldFetchPhoton || results.length < 5;
        if (shouldFetchPhotonGlobal) {
          try {
            const globalPhoton = await fetchPhoton(false);
            results = dedupeResults([...results, ...globalPhoton]);
          } catch {
            // Ignore global photon errors
          }
        }
      }

      if (!results.some(isSettlementResult) && trimmed.length >= 5) {
        const lastChar = normalizedQuery[normalizedQuery.length - 1];
        const isVowel = ['a', 'e', 'i', 'o', 'u', 'y', 'ä', 'ö'].includes(lastChar);
        const fallbackQueries: string[] = [];
        if (!isVowel) {
          fallbackQueries.push(`${trimmed}i`);
        } else if (lastChar === 'e') {
          fallbackQueries.push(`${trimmed}mi`);
        } else if (lastChar === 'i') {
          fallbackQueries.push(`${trimmed}emi`);
        }

        if (fallbackQueries.length > 0) {
          try {
            const fallbackUseLocation = !usedGlobalFallback && fetchedNominatim;
            const fallbackResults = await Promise.all(
              fallbackQueries.map(queryValue => fetchNominatim(queryValue, undefined, fallbackUseLocation))
            );
            const flattened = fallbackResults.flat();
            primaryResults = dedupeResults([...primaryResults, ...flattened]);
            results = dedupeResults([...results, ...flattened]);
          } catch {
            // Ignore fallback errors
          }
        }
      }

      hasQualityMatch = results.some((result) => {
        const score = matchScore(result, normalizedQuery);
        return !isLowQualityMatch(result, score);
      });
      if (!hasQualityMatch && isAsciiQuery && languageCode !== 'en') {
        try {
          const englishResults = await fetchNominatim(trimmed, 'en', !usedGlobalFallback);
          results = dedupeResults([...results, ...englishResults]);
        } catch {
          // Ignore fallback errors
        }
      }

      state.searchResults = rankSearchResults(results, normalizedQuery, origin, viewBounds);
    } catch {
      state.searchResults = [];
    } finally {
      state.isSearching = false;
    }
  },

  clearSearch() {
    state.searchQuery = '';
    state.searchResults = [];
  },

  setAudienceFilter(filter: AudienceFilter) {
    state.audienceFilter = filter;
  },

  openAddPlaceModal() {
    state.isAddPlaceModalOpen = true;
  },

  closeAddPlaceModal() {
    state.isAddPlaceModalOpen = false;
    state.pendingPlaceLocation = null;
  },

  setPendingPlaceLocation(lat: number, lng: number) {
    state.pendingPlaceLocation = { lat, lng };
  },

  async addPlace(place: Omit<Place, 'id' | 'createdAt' | 'updatedAt'>) {
    const now = Date.now();
    const newPlace: Place = {
      ...place,
      id: crypto.randomUUID(),
      createdAt: now,
      updatedAt: now,
    };
    state.places = [...state.places, newPlace];
    await queuePersist();
    return newPlace;
  },

  updatePlace(id: string, updates: Partial<Omit<Place, 'id' | 'createdAt' | 'owner'>>) {
    state.places = state.places.map(p =>
      p.id === id ? { ...p, ...updates, updatedAt: Date.now() } : p
    );
    queuePersist();
  },

  deletePlace(id: string) {
    state.places = state.places.filter(p => p.id !== id);
    queuePersist();
  },

  selectPlace(place: Place | null) {
    state.selectedPlace = place;
  },

  addAnnotation(annotation: Omit<Annotation, 'id' | 'createdAt' | 'updatedAt'>) {
    const now = Date.now();
    const newAnnotation: Annotation = {
      ...annotation,
      id: crypto.randomUUID(),
      createdAt: now,
      updatedAt: now,
    };
    state.annotations = [...state.annotations, newAnnotation];
    queuePersist();
    return newAnnotation;
  },

  deleteAnnotation(id: string) {
    state.annotations = state.annotations.filter(a => a.id !== id);
    queuePersist();
  },

  reset() {
    state = { ...initialState };
  },
};
