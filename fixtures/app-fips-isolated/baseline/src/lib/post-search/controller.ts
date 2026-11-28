import type { RawEvent } from '../nostr';
import { collectFilterHints, planPostSearch, type SearchPlan } from './query';
import {
  cachedLexicalMatch,
  postSubstanceFactor,
  rankCandidates,
  searchPostText,
} from './ranking';
import { createPostScorer, type SearchSettings } from './scorer';

export type SemanticSearchSnapshot = {
  ordered: RawEvent[];
  primary: RawEvent[];
  status: string;
  busy: boolean;
  relatedTerms: string[];
  relatedTags: string[];
  scoredCount: number;
};

type PostSearchRelevanceControllerOptions = {
  getAuthorSignal: (pubkey: string) => number;
  getKnownTags: () => string[];
  createScorer?: typeof createPostScorer;
  onChange: (snapshot: SemanticSearchSnapshot) => void;
  onExpansion?: (expansion: { query: string; terms: string[]; tags: string[] }) => void;
};

const TERM_PATTERN = /^[\p{L}\p{N}][\p{L}\p{N}_ -]{1,38}$/u;

/**
 * Coordinates instant lexical+graph ranking alongside progressive local AI
 * planning and scoring for a single active post search view.
 */
export class PostSearchRelevanceController {
  private query = '';
  private plan: SearchPlan | null = null;
  private settings: SearchSettings | null = null;
  private events: RawEvent[] = [];
  private scores = new Map<string, number>();
  private planned = false;
  private relatedTerms: string[] = [];
  private relatedTags: string[] = [];
  private status = '';
  private capacity = 4;
  private localModel = false;
  private extraBudget = 0;
  private controller: AbortController | null = null;
  private scorer: ((posts: string[]) => Promise<number[]>) | null = null;
  private running = false;
  private kickTimer: ReturnType<typeof setTimeout> | undefined;
  private kickDeadline = Infinity;
  private lexical: ((event: RawEvent) => number) | null = null;
  private discovery: ((event: RawEvent) => number) | null = null;
  private resolveSample: (() => void) | null = null;
  private lastSnapshot: SemanticSearchSnapshot = {
    ordered: [],
    primary: [],
    status: '',
    busy: false,
    relatedTerms: [],
    relatedTags: [],
    scoredCount: 0,
  };

  constructor(private readonly options: PostSearchRelevanceControllerOptions) {}

  configure(query: string, settings: SearchSettings | null): void {
    const trimmed = query.trim();
    const sameSettings = this.settings?.model === settings?.model
      && this.settings?.ollamaModel === settings?.ollamaModel;
    if (trimmed === this.query && sameSettings) { return; }
    this.stop();
    this.query = trimmed;
    this.settings = settings ? { ...settings } : null;
    this.plan = trimmed.length >= 2 ? planPostSearch(trimmed) : null;
    this.lexical = this.plan ? cachedLexicalMatch(this.plan) : null;
    this.discovery = null;
    this.scores = new Map();
    this.planned = false;
    this.relatedTerms = [];
    this.relatedTags = [];
    this.status = '';
    this.capacity = 4;
    this.localModel = settings?.model === 'minilm';
    this.extraBudget = 0;

    if (this.plan && this.plan.terms.length > 0 && this.settings) {
      this.startScorer(this.plan, this.settings);
    }
    this.recompute();
    this.kick();
  }

  setCandidates(events: RawEvent[]): void {
    this.events = events;
    this.recompute();
    if (this.events.length > 0) {
      const broad = Boolean(this.plan?.browse && this.plan.terms.length === 1);
      if (this.events.length >= (broad ? 12 : 1)) {
        this.resolveSample?.();
        this.resolveSample = null;
      }
    }
    this.kick();
  }

  increaseBudget(): void {
    this.extraBudget += this.pageSize();
    this.kick();
  }

  refreshRanking(): void {
    this.recompute();
  }

  stop(): void {
    clearTimeout(this.kickTimer);
    this.kickTimer = undefined;
    this.kickDeadline = Infinity;
    this.resolveSample?.();
    this.resolveSample = null;
    this.controller?.abort();
    this.controller = null;
    this.scorer = null;
    this.running = false;
  }

  getSnapshot(): SemanticSearchSnapshot {
    return this.lastSnapshot;
  }

  private pageSize(): number {
    if (!this.localModel) { return 16; }
    return this.capacity > 4 ? 128 : 64;
  }

  private budget(): number {
    const page = this.pageSize();
    return page + this.extraBudget;
  }

  private batchLimit(): number {
    const visible = this.lastSnapshot.primary.length;
    if (!visible) { return 4; }
    if (visible < 16) { return Math.min(this.capacity, 16); }
    return this.capacity;
  }

  private startScorer(plan: SearchPlan, settings: SearchSettings): void {
    const controller = new AbortController();
    this.controller = controller;
    const broad = plan.browse && plan.terms.length === 1;
    const sampleReady = new Promise<void>((resolve) => {
      if (this.events.length >= (broad ? 12 : 1)) {
        resolve();
        return;
      }
      this.resolveSample = resolve;
    });
    const makeScorer = this.options.createScorer ?? createPostScorer;
    this.scorer = makeScorer(
      settings,
      plan.text,
      controller.signal,
      (terms, tags = []) => {
        if (controller.signal.aborted) { return; }
        const excluded = new Set(
          [plan.relayQuery, ...plan.terms, ...plan.anchors, ...plan.hashtags].map((t) => t.toLowerCase()),
        );
        const validTerms = [...new Set(
          terms
            .filter((term): term is string => typeof term === 'string')
            .map((term) => term.trim().toLowerCase())
            .filter((term) => TERM_PATTERN.test(term) && !excluded.has(term)),
        )].slice(0, 2);
        const validTags = [...new Set(tags)].slice(0, 6);
        this.relatedTerms = validTerms;
        this.relatedTags = validTags;
        this.updateDiscoveryMatcher();
        this.recompute();
        if (validTerms.length > 0 || validTags.length > 0) {
          this.options.onExpansion?.({ query: this.query, terms: validTerms, tags: validTags });
        }
      },
      (batchSize = 1) => {
        this.localModel = true;
        this.capacity = Math.max(4, batchSize);
      },
      async () => {
        const known = this.options.getKnownTags();
        let timer: ReturnType<typeof setTimeout> | undefined;
        await Promise.race([
          known.length ? Promise.resolve() : sampleReady,
          new Promise<void>((resolve) => {
            timer = setTimeout(resolve, broad ? 550 : 300);
          }),
        ]);
        clearTimeout(timer);
        return collectFilterHints(this.shortlist(), [...known, ...this.options.getKnownTags()]);
      },
    );
  }

  private updateDiscoveryMatcher(): void {
    if (!this.relatedTags.length && !this.relatedTerms.length) {
      this.discovery = null;
      return;
    }
    const tags = new Set(this.relatedTags.map((tag) => tag.toLowerCase()));
    const match = this.relatedTerms.length
      ? cachedLexicalMatch(planPostSearch(this.relatedTerms.join(' ')))
      : null;
    this.discovery = (event) => (
      event.tags.some(([key, tag]) => key === 't' && tag && tags.has(tag.toLowerCase()))
        ? 1
        : (match?.(event) ?? 0)
    );
  }

  private scoredCandidates() {
    if (!this.plan || !this.lexical) { return []; }
    const lexical = this.lexical;
    const plan = this.plan;
    const discovery = this.discovery;
    return this.events.flatMap((event) => {
      const match = lexical(event);
      if (match < 0) { return []; }
      return [{
        event,
        lexical: match,
        author: this.options.getAuthorSignal(event.pubkey),
        substance: postSubstanceFactor(plan, event),
        discovery: discovery?.(event) ?? 0,
      }];
    });
  }

  private shortlist(): RawEvent[] {
    if (!this.plan?.terms.length) { return []; }
    const ordered = this.scoredCandidates().sort((left, right) => (
      Math.max(right.lexical, right.discovery * 0.75) - Math.max(left.lexical, left.discovery * 0.75)
      || right.author - left.author
      || (right.event.created_at ?? 0) - (left.event.created_at ?? 0)
    ));
    const buckets: RawEvent[][] = [[], [], [], []];
    const seen = new Set<string>();
    for (const { event, lexical, discovery } of ordered) {
      const index = (seen.has(event.pubkey) ? 2 : 0) + (lexical || discovery ? 0 : 1);
      buckets[index]!.push(event);
      seen.add(event.pubkey);
    }
    const maxItems = this.budget();
    const result: RawEvent[] = [];
    for (const pair of [buckets.slice(0, 2), buckets.slice(2)]) {
      const longest = Math.max(pair[0]!.length, pair[1]!.length);
      for (let i = 0; i < longest && result.length < maxItems; i += 1) {
        for (const bucket of pair) {
          if (bucket[i] && result.length < maxItems) { result.push(bucket[i]!); }
        }
      }
    }
    return result;
  }

  private hasPendingScoring(): boolean {
    if (!this.controller || this.controller.signal.aborted || !this.scorer) { return false; }
    if (!this.planned) { return true; }
    if (this.scores.size >= this.budget()) { return false; }
    return this.shortlist().some((event) => !this.scores.has(event.id));
  }

  private recompute(): void {
    const ranked = rankCandidates(this.scoredCandidates(), this.scores, Date.now() / 1_000);
    this.lastSnapshot = {
      ordered: ranked.ordered,
      primary: ranked.primary,
      status: this.status,
      busy: this.status === 'Ranking posts…' || this.hasPendingScoring(),
      relatedTerms: this.relatedTerms,
      relatedTags: this.relatedTags,
      scoredCount: this.scores.size,
    };
    this.options.onChange(this.lastSnapshot);
  }

  private kick(): void {
    const controller = this.controller;
    if (!controller || controller.signal.aborted || this.running || !this.scorer) { return; }
    const waiting = this.shortlist().filter((event) => !this.scores.has(event.id)).length;
    if (this.planned && (!waiting || this.scores.size >= this.budget())) { return; }
    const urgent = !this.planned
      || this.lastSnapshot.primary.length === 0
      || waiting >= Math.min(this.batchLimit(), 16);
    const delay = urgent ? 0 : (this.capacity > 4 ? 16 : 60);
    const next = performance.now() + delay;
    if (this.kickTimer !== undefined && next >= this.kickDeadline) { return; }
    clearTimeout(this.kickTimer);
    this.kickDeadline = next;
    this.kickTimer = setTimeout(() => {
      this.kickTimer = undefined;
      this.kickDeadline = Infinity;
      void this.run(controller);
    }, delay);
  }

  private async run(controller: AbortController): Promise<void> {
    if (this.running || controller.signal.aborted || !this.scorer) { return; }
    this.running = true;
    try {
      if (!this.planned) {
        this.status = 'Ranking posts…';
        this.recompute();
        await this.scorer([]);
        if (controller.signal.aborted) { return; }
        this.planned = true;
      }
      while (!controller.signal.aborted) {
        const remaining = this.budget() - this.scores.size;
        const batch = this.shortlist()
          .filter((event) => !this.scores.has(event.id))
          .slice(0, Math.max(0, Math.min(this.batchLimit(), remaining)));
        if (!batch.length) { break; }
        this.status = 'Ranking posts…';
        this.recompute();
        const results = await this.scorer(batch.map(searchPostText));
        if (controller.signal.aborted) { return; }
        if (
          results.length !== batch.length
          || !results.every((value) => Number.isFinite(value) && value >= 0 && value <= 1)
        ) {
          throw new Error('Invalid scores');
        }
        batch.forEach((event, index) => this.scores.set(event.id, results[index]!));
        this.recompute();
      }
      if (!controller.signal.aborted) {
        this.status = '';
        this.recompute();
      }
    } catch {
      if (!controller.signal.aborted) {
        this.status = this.settings?.model === 'ollama'
          ? 'Ollama unavailable · using text and network relevance'
          : this.settings?.model === 'browser'
            ? 'Browser AI unavailable · using text and network relevance'
            : 'AI unavailable · using text and network relevance';
        controller.abort();
        this.recompute();
      }
    } finally {
      this.running = false;
    }
  }
}
