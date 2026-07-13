import NDK from "@nostr-dev-kit/ndk";
import fs from "fs";
import throttle from "lodash/throttle";
import { SocialGraph, NostrEvent } from "../src";
import {
  SOCIAL_GRAPH_ROOT,
  DATA_DIR,
  SOCIAL_GRAPH_LARGE_BIN,
  FUSE_INDEX_FILE,
  DATA_FILE,
  RELAY_URLS,
  PROFILE_PICTURE_URL_MAX_LENGTH,
  PROFILE_SEARCH_INDEX_DIR,
  PROFILE_SEARCH_INDEX_ROOT,
} from "../src/constants";
import WebSocket from "ws";
import Fuse from "fuse.js";
import { FileStore } from "./hashtreeStore";
import {
  ProfileSearchIndex,
  deserializeCid,
  serializeCid,
  type ProfileSearchRecord,
  profileSearchIndexNhash,
} from "./profileSearchIndex";
import { canonicalizeProfile } from "./profileCanonicalize";
import { parseCrawlDistance } from "./crawlDistance";
import type { CID } from "@hashtree/core";
import type { SearchOptions, SearchResult } from "@hashtree/index";

console.log('Starting profile indexer...');

global.WebSocket = WebSocket as any;

type Profile = {
  name: string;
  pubKey: string;
  nip05?: string;
  aliases?: string[];
};

const DEFAULT_PROFILE_CRAWL_BATCH_SIZE = 100;

function parseProfileCrawlLimit(rawValue?: string): number | undefined {
  if (!rawValue) {
    return undefined;
  }
  const parsed = Number.parseInt(rawValue, 10);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    return undefined;
  }
  return parsed;
}

export class ProfileIndexer {
  private socialGraph: SocialGraph;
  private ndk: NDK;
  private fuse: Fuse<Profile>;
  private data: Map<string, string[]>;
  private latestProfileTimestamps: Map<string, number>;
  private profileSearchRecords: Map<string, ProfileSearchRecord>;
  private searchIndex: ProfileSearchIndex;
  private searchIndexRoot: CID | null;
  private profileCrawlDistance: number | undefined;
  private profileCrawlLimit: number | undefined;
  private profileUpdateQueue: Promise<void>;
  private throttledSave: any;

  constructor(socialGraph: SocialGraph, ndk: NDK) {
    console.log('Creating profile indexer instance...');
    this.socialGraph = socialGraph;
    this.ndk = ndk;
    this.latestProfileTimestamps = new Map<string, number>();
    this.data = new Map<string, string[]>();
    this.profileSearchRecords = new Map<string, ProfileSearchRecord>();
    this.profileCrawlDistance = parseCrawlDistance(
      process.env.PROFILE_CRAWL_DISTANCE ?? process.env.SOCIAL_GRAPH_CRAWL_DISTANCE
    );
    this.profileCrawlLimit = parseProfileCrawlLimit(process.env.PROFILE_CRAWL_LIMIT);
    this.profileUpdateQueue = Promise.resolve();
    this.searchIndex = new ProfileSearchIndex(new FileStore(PROFILE_SEARCH_INDEX_DIR));
    this.searchIndexRoot = this.loadSearchIndexRoot();

    if (this.profileCrawlLimit) {
      console.log(`Profile crawl limit set to ${this.profileCrawlLimit}`);
    }

    // Initialize data and Fuse index
    if (fs.existsSync(DATA_FILE) && fs.existsSync(FUSE_INDEX_FILE)) {
      try {
        console.log('Loading existing profile data and index...');
        const rawData = JSON.parse(fs.readFileSync(DATA_FILE, 'utf-8'));
        // Convert array to Map
        this.data = new Map(rawData.map((item: string[]) => [item[0], item]));
        const fuseIndex = JSON.parse(fs.readFileSync(FUSE_INDEX_FILE, 'utf-8'));
        
        // Convert data to Profile objects for Fuse
        const profiles: Profile[] = Array.from(this.data.values()).map(item => {
          const record = this.profileRecordFromItem(item);
          if (record) {
            this.profileSearchRecords.set(record.pubKey, record);
          }
          return {
            name: item[1],
            pubKey: item[0],
            nip05: item[2] || undefined,
            aliases: [],
          };
        });
        
        this.fuse = new Fuse<Profile>(profiles, { keys: ["name", "pubKey", "nip05", "aliases"] });
        console.log(`Loaded ${this.data.size} profiles and Fuse index`);
      } catch (e) {
        console.error('Failed to load existing data:', e);
        this.fuse = new Fuse<Profile>([], { keys: ["name", "pubKey", "nip05", "aliases"] });
        this.data = new Map();
      }
    } else {
      this.fuse = new Fuse<Profile>([], { keys: ["name", "pubKey", "nip05", "aliases"] });
      this.data = new Map();
    }

    this.throttledSave = throttle(async () => {
      try {
        if (!fs.existsSync(DATA_DIR)) {
          fs.mkdirSync(DATA_DIR);
        }
        
        // Save Fuse index
        const fuseIndex = this.fuse.getIndex();
        fs.writeFileSync(FUSE_INDEX_FILE, JSON.stringify(fuseIndex));
        console.log("Saved Fuse index");
        
        // Save profile data
        const dataArray = Array.from(this.data.values());
        fs.writeFileSync(DATA_FILE, JSON.stringify(dataArray));
        console.log("Saved profile data of size", this.data.size);

        if (this.searchIndexRoot) {
          const serializedRoot = serializeCid(this.searchIndexRoot);
          fs.writeFileSync(PROFILE_SEARCH_INDEX_ROOT, JSON.stringify(serializedRoot));
          console.log("Saved profile search index root");
        }
      } catch (e) {
        console.error("Failed to save data:", e);
        console.log("social graph size", this.socialGraph.size());
        console.log("profile data size", this.data.size);
      }
    }, 30000); // 30 seconds throttle
  }

  private loadSearchIndexRoot(): CID | null {
    if (!fs.existsSync(PROFILE_SEARCH_INDEX_ROOT)) {
      return null;
    }
    try {
      const raw = JSON.parse(fs.readFileSync(PROFILE_SEARCH_INDEX_ROOT, 'utf-8'));
      return deserializeCid(raw);
    } catch (e) {
      console.error('Failed to load profile search index root:', e);
      return null;
    }
  }

  private async ensureSearchIndex() {
    const shouldRebuild = process.env.REBUILD_PROFILE_SEARCH_INDEX === 'true';
    if (!shouldRebuild && this.searchIndexRoot) {
      return;
    }

    if (this.data.size === 0) {
      return;
    }

    console.log('Rebuilding profile search index...');
    let root: CID | null = null;
    for (const item of this.data.values()) {
      const pubKey = item[0];
      const record = (pubKey ? this.profileSearchRecords.get(pubKey) : undefined) ?? this.profileRecordFromItem(item);
      if (!record) continue;
      root = await this.searchIndex.indexProfile(root, record);
    }
    this.searchIndexRoot = root;
    console.log(`Rebuilt profile search index for ${this.data.size} profiles`);
  }

  private profileRecordFromItem(item: string[]): ProfileSearchRecord | undefined {
    if (!item[0] || !item[1]) {
      return undefined;
    }
    return {
      pubKey: item[0],
      name: item[1],
      nip05: item[2] ? item[2] : undefined,
      aliases: [],
    };
  }

  private queueProfileUpdate(event: NostrEvent): Promise<void> {
    this.profileUpdateQueue = this.profileUpdateQueue
      .then(() => this.handleProfileEvent(event))
      .catch((error) => {
        console.error('Failed to handle profile event:', error);
      });
    return this.profileUpdateQueue;
  }

  async initialize() {
    console.log('Initializing profile indexer...');
    try {
      console.log('Connecting to NDK...');
      await this.ndk.connect(5000); // 5 second timeout
      console.log('ndk connected');
    } catch (e) {
      console.error('Failed to connect to NDK:', e);
      return;
    }

    await this.ensureSearchIndex();

    // Start indexing profiles
    await this.fetchProfilesInBatches(this.socialGraph.userIterator(this.profileCrawlDistance));
  }

  listen() {
    const sub = this.ndk.subscribe({
      kinds: [0],
    });
    sub.on("event", (event) => {
      const distance = this.socialGraph.getFollowDistance(event.pubkey);
      if (distance < 1000 && (this.profileCrawlDistance === undefined || distance <= this.profileCrawlDistance)) {
        void this.queueProfileUpdate(event as NostrEvent);
      }
    });
  }

  private async fetchProfilesInBatches(iterator: IterableIterator<string>) {
    const batchSize = DEFAULT_PROFILE_CRAWL_BATCH_SIZE;
    let batch: string[] = [];
    let processed = 0;
    const maxProfiles = this.profileCrawlLimit;
    
    for (const pubkey of iterator) {
      if (maxProfiles !== undefined && processed >= maxProfiles) {
        break;
      }
      batch.push(pubkey);
      processed += 1;
      
      if (batch.length >= batchSize) {
        await this.fetchProfiles(batch);
        this.throttledSave();
        batch = [];
      }
    }
    
    if (batch.length > 0) {
      await this.fetchProfiles(batch);
      this.throttledSave();
    }

    if (maxProfiles !== undefined && processed >= maxProfiles) {
      console.log(`Profile crawl limit reached (${processed} users)`);
    }
  }

  private async fetchProfiles(pubkeys: string[]) {
    try {
      const events = await this.ndk.fetchEvents({
        kinds: [0],
        authors: pubkeys,
      });

      for (const event of events) {
        try {
          await this.queueProfileUpdate(event as NostrEvent);
        } catch (e) {
          console.error('Failed to parse profile content:', e);
        }
      }
    } catch (e) {
      console.error('Failed to fetch profiles:', e);
    }
  }

  private async handleProfileEvent(event: NostrEvent) {
    const currentTimestamp = this.latestProfileTimestamps.get(event.pubkey);
    if (currentTimestamp && event.created_at <= currentTimestamp) {
      return;
    }
    this.latestProfileTimestamps.set(event.pubkey, event.created_at);
    try {
      const profile = JSON.parse(event.content);
      const pubKey = event.pubkey;
      const canonical = canonicalizeProfile(profile as Record<string, unknown>);
      const name = canonical.primaryName;
      if (!name) return;
      const nip05 = canonical.nip05;
      const aliases = canonical.names.filter((candidate) => candidate !== name);
    
      console.log(`Handling profile event for ${name} (${pubKey})`);
      this.fuse.remove((profile) => profile.pubKey === pubKey);
      this.fuse.add({ name, pubKey, nip05, aliases });
      
      const previousRecord = this.profileSearchRecords.get(pubKey);
      const nextRecord: ProfileSearchRecord = { pubKey, name, nip05, aliases };
      if (
        !previousRecord ||
        previousRecord.name !== nextRecord.name ||
        previousRecord.nip05 !== nextRecord.nip05 ||
        previousRecord.aliases?.join("|") !== nextRecord.aliases?.join("|")
      ) {
        this.searchIndexRoot = await this.searchIndex.updateProfile(
          this.searchIndexRoot,
          previousRecord,
          nextRecord
        );
        this.profileSearchRecords.set(pubKey, nextRecord);
      }

      const item = [pubKey, name];
      const hasPicture = profile.picture && profile.picture.length < PROFILE_PICTURE_URL_MAX_LENGTH;
      if (nip05) {
        item.push(nip05);
      } else if (hasPicture) {
        item.push('');
      }
      if (hasPicture) {
        item.push(profile.picture.trim().replace(/^https:\/\//, ''));
      }
      this.data.set(pubKey, item);
      this.throttledSave();
    } catch (e) {
      console.error('Failed to parse profile event:', e);
      // Silently skip invalid profiles
    }
  }

  getFuse() {
    return this.fuse;
  }

  async reindex() {
    console.log("Starting profile re-indexing for all users in graph...");
    await this.ensureSearchIndex();
    await this.fetchProfilesInBatches(this.socialGraph.userIterator(this.profileCrawlDistance));
    console.log("Profile re-indexing complete. Total profiles:", this.data.size);
  }

  getData(maxBytes?: number, noPictures?: boolean) {
    let data = Array.from(this.data.values());
    
    if (noPictures) {
      data = data.map(item => {
        // Get first three items [pubKey, name, nip05]
        const baseItems = item.slice(0, 3);
        // Find the last non-empty item
        let lastNonEmptyIndex = baseItems.length - 1;
        while (lastNonEmptyIndex >= 0 && !baseItems[lastNonEmptyIndex]) {
          lastNonEmptyIndex--;
        }
        return baseItems.slice(0, lastNonEmptyIndex + 1);
      });
    }

    if (!maxBytes) {
      return data;
    }

    let currentSize = 2; // Start with '[' and will end with ']'
    const result: string[][] = [];
    
    for (const item of data) {
      // Calculate size of this item: comma + array brackets + string lengths + quotes
      const itemSize = (result.length ? 1 : 0) + // comma if not first
        2 + // array brackets
        item.reduce((sum, str) => sum + 2 + str.length, 0); // quotes + string length
      
      if (currentSize + itemSize > maxBytes) {
        break;
      }
      currentSize += itemSize;
      result.push(item);
    }
    
    return result;
  }

  async searchProfiles(query: string, options?: SearchOptions): Promise<string[][]> {
    if (!query.trim()) {
      return [];
    }
    const results: SearchResult[] = await this.searchIndex.search(
      this.searchIndexRoot,
      query,
      options
    );
    const matches: string[][] = [];
    for (const result of results) {
      const item = this.data.get(result.id);
      if (item) {
        matches.push(item);
      }
    }
    return matches;
  }

  getProfileCount(): number {
    return this.data.size;
  }

  getSearchIndexRoot(): CID | null {
    return this.searchIndexRoot;
  }

  getSearchIndexNhash(): string | null {
    return profileSearchIndexNhash(this.searchIndexRoot);
  }

  async flush(): Promise<void> {
    await this.profileUpdateQueue;
    if (typeof this.throttledSave?.flush === 'function') {
      this.throttledSave.flush();
    }
  }
}

// Only run if called directly
if (process.argv.includes('--once')) {
  (async () => {
    let socialGraph: SocialGraph;
    if (fs.existsSync(SOCIAL_GRAPH_LARGE_BIN)) {
      try {
        const socialGraphData = fs.readFileSync(SOCIAL_GRAPH_LARGE_BIN);
        socialGraph = await SocialGraph.fromBinary(SOCIAL_GRAPH_ROOT, new Uint8Array(socialGraphData));
        console.log("Loaded social graph of size", socialGraph.size());
      } catch (e) {
        console.error("Error deserializing social graph:", e);
        socialGraph = new SocialGraph(SOCIAL_GRAPH_ROOT);
      }
    } else {
      socialGraph = new SocialGraph(SOCIAL_GRAPH_ROOT);
      console.log("Created new social graph");
    }
    const ndk = new NDK({
      explicitRelayUrls: RELAY_URLS,
    });
    const indexer = new ProfileIndexer(socialGraph, ndk);
    indexer.initialize();
  })();
}
