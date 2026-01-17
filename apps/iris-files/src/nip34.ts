/**
 * NIP-34: Git Repositories
 * Handles pull requests, issues, and their statuses for git repositories
 * Uses htree://npub/reponame style repository addresses
 */
import { ndk, nostrStore, pubkeyToNpub, npubToPubkey } from './nostr';
import { NDKEvent, type NDKFilter } from 'ndk';
import { nip19 } from 'nostr-tools';
import {
  KIND_REPO_ANNOUNCEMENT,
  KIND_PULL_REQUEST,
  KIND_ISSUE,
  KIND_STATUS_OPEN,
  KIND_STATUS_APPLIED,
  KIND_STATUS_CLOSED,
  KIND_STATUS_DRAFT,
} from './utils/constants';

/**
 * Fetch events using subscribe (fetchEvents hangs because main thread NDK has no relays).
 * Collects events until EOSE or timeout.
 */
async function fetchEventsViaSubscribe(filter: NDKFilter, timeoutMs = 5000): Promise<NDKEvent[]> {
  return new Promise((resolve) => {
    const events: NDKEvent[] = [];
    const sub = ndk.subscribe(filter, { closeOnEose: true });

    const timeout = setTimeout(() => {
      sub.stop();
      resolve(events);
    }, timeoutMs);

    sub.on('event', (event: NDKEvent) => {
      events.push(event);
    });

    sub.on('eose', () => {
      clearTimeout(timeout);
      sub.stop();
      resolve(events);
    });
  });
}

// Status types for PRs and Issues
export type ItemStatus = 'open' | 'merged' | 'closed' | 'draft';

export interface PullRequest {
  id: string; // Event ID
  eventId: string; // Same as id for Nostr compatibility
  title: string;
  description: string;
  author: string; // npub
  authorPubkey: string; // hex pubkey
  status: ItemStatus;
  branch?: string; // Source branch
  targetBranch?: string; // Target branch (usually main)
  commitTip?: string; // Commit hash at tip
  cloneUrl?: string; // Where to clone from
  created_at: number;
  updated_at: number;
  labels: string[];
}

export interface Issue {
  id: string; // Event ID
  eventId: string; // Same as id for Nostr compatibility
  title: string;
  description: string;
  author: string; // npub
  authorPubkey: string; // hex pubkey
  status: ItemStatus;
  created_at: number;
  updated_at: number;
  labels: string[];
}

export interface Comment {
  id: string;
  content: string;
  author: string; // npub
  authorPubkey: string; // hex pubkey
  created_at: number;
  replyTo?: string; // Event ID this is replying to
}

/**
 * Build the NIP-34 repository address tag (a-tag)
 * Format: 30617:<owner-pubkey>:<repo-name>
 */
export function buildRepoAddress(npub: string, repoName: string): string {
  const pubkey = npubToPubkey(npub);
  if (!pubkey) throw new Error('Invalid npub');
  return `${KIND_REPO_ANNOUNCEMENT}:${pubkey}:${repoName}`;
}

/**
 * Build htree:// clone URL for the repository
 * @param npub - Owner's npub
 * @param repoName - Repository name
 * @param options - Optional visibility settings
 * @param options.visibility - Tree visibility: 'public', 'link-visible', or 'private'
 * @param options.linkKey - Link key for link-visible trees (hex string)
 */
export function buildHtreeUrl(
  npub: string,
  repoName: string,
  options?: {
    visibility?: 'public' | 'link-visible' | 'private';
    linkKey?: string;
  }
): string {
  let url = `htree://${npub}/${repoName}`;

  if (options?.visibility === 'private') {
    url += '#private';
  } else if (options?.visibility === 'link-visible' && options.linkKey) {
    url += `#k=${options.linkKey}`;
  }

  return url;
}

/**
 * Parse an a-tag to extract owner pubkey and repo name
 */
export function parseRepoAddress(aTag: string): { pubkey: string; repoName: string } | null {
  const parts = aTag.split(':');
  if (parts.length !== 3 || parts[0] !== String(KIND_REPO_ANNOUNCEMENT)) {
    return null;
  }
  return { pubkey: parts[1], repoName: parts[2] };
}

/**
 * Get the status kind from a status string
 */
function statusToKind(status: ItemStatus): number {
  switch (status) {
    case 'open': return KIND_STATUS_OPEN;
    case 'merged': return KIND_STATUS_APPLIED;
    case 'closed': return KIND_STATUS_CLOSED;
    case 'draft': return KIND_STATUS_DRAFT;
    default: return KIND_STATUS_OPEN;
  }
}

/**
 * Get status string from a kind number
 */
function kindToStatus(kind: number): ItemStatus {
  switch (kind) {
    case KIND_STATUS_OPEN: return 'open';
    case KIND_STATUS_APPLIED: return 'merged';
    case KIND_STATUS_CLOSED: return 'closed';
    case KIND_STATUS_DRAFT: return 'draft';
    default: return 'open';
  }
}

/**
 * Fetch pull requests for a repository from Nostr
 */
export async function fetchPullRequests(npub: string, repoName: string): Promise<PullRequest[]> {
  const pubkey = npubToPubkey(npub);
  if (!pubkey) return [];

  const repoAddress = buildRepoAddress(npub, repoName);

  // Fetch PR events that reference this repo
  const filter: NDKFilter = {
    kinds: [KIND_PULL_REQUEST as number],
    '#a': [repoAddress],
  };

  const events = await fetchEventsViaSubscribe(filter);
  const prs: PullRequest[] = [];

  for (const event of events) {
    const pr = parsePullRequestEvent(event);
    if (pr) {
      // Fetch latest status for this PR
      pr.status = await fetchLatestStatus(pr.eventId, pubkey);
      prs.push(pr);
    }
  }

  // Sort by created_at descending (newest first)
  return prs.sort((a, b) => b.created_at - a.created_at);
}

/**
 * Fetch issues for a repository from Nostr
 */
export async function fetchIssues(npub: string, repoName: string): Promise<Issue[]> {
  const pubkey = npubToPubkey(npub);
  if (!pubkey) return [];

  const repoAddress = buildRepoAddress(npub, repoName);

  // Fetch issue events that reference this repo
  const filter: NDKFilter = {
    kinds: [KIND_ISSUE as number],
    '#a': [repoAddress],
  };

  const events = await fetchEventsViaSubscribe(filter);
  const issues: Issue[] = [];

  for (const event of events) {
    const issue = parseIssueEvent(event);
    if (issue) {
      // Fetch latest status for this issue
      issue.status = await fetchLatestStatus(issue.eventId, pubkey);
      issues.push(issue);
    }
  }

  // Sort by created_at descending (newest first)
  return issues.sort((a, b) => b.created_at - a.created_at);
}

/**
 * Fetch the latest status for a PR or issue
 */
async function fetchLatestStatus(targetEventId: string, repoOwnerPubkey: string): Promise<ItemStatus> {
  const filter: NDKFilter = {
    kinds: [KIND_STATUS_OPEN, KIND_STATUS_APPLIED, KIND_STATUS_CLOSED, KIND_STATUS_DRAFT],
    '#e': [targetEventId],
  };

  const events = await fetchEventsViaSubscribe(filter, 3000);

  // Find the most recent valid status event
  // Valid = from repo maintainer OR the issue/PR author
  let latestStatus: { kind: number; created_at: number } | null = null;

  for (const event of events) {
    const isFromMaintainer = event.pubkey === repoOwnerPubkey;
    // TODO: Check if from PR/issue author
    const isValidSource = isFromMaintainer;

    if (isValidSource && (!latestStatus || event.created_at! > latestStatus.created_at)) {
      latestStatus = { kind: event.kind!, created_at: event.created_at! };
    }
  }

  return latestStatus ? kindToStatus(latestStatus.kind) : 'open';
}

/**
 * Parse a PR event into a PullRequest object
 */
function parsePullRequestEvent(event: NDKEvent): PullRequest | null {
  if (!event.id || !event.pubkey) return null;

  const tags = event.tags;
  const title = tags.find(t => t[0] === 'subject')?.[1] || tags.find(t => t[0] === 'title')?.[1] || 'Untitled PR';
  const branch = tags.find(t => t[0] === 'branch')?.[1];
  const targetBranch = tags.find(t => t[0] === 'target-branch')?.[1] || 'main';
  const commitTip = tags.find(t => t[0] === 'c')?.[1]; // commit tip
  const cloneUrl = tags.find(t => t[0] === 'clone')?.[1];
  const labels = tags.filter(t => t[0] === 't').map(t => t[1]);

  return {
    id: event.id,
    eventId: event.id,
    title,
    description: event.content || '',
    author: pubkeyToNpub(event.pubkey),
    authorPubkey: event.pubkey,
    status: 'open', // Will be updated by status fetch
    branch,
    targetBranch,
    commitTip,
    cloneUrl,
    created_at: event.created_at || 0,
    updated_at: event.created_at || 0, // TODO: track updates
    labels,
  };
}

/**
 * Parse an issue event into an Issue object
 */
function parseIssueEvent(event: NDKEvent): Issue | null {
  if (!event.id || !event.pubkey) return null;

  const tags = event.tags;
  const title = tags.find(t => t[0] === 'subject')?.[1] || tags.find(t => t[0] === 'title')?.[1] || 'Untitled Issue';
  const labels = tags.filter(t => t[0] === 't').map(t => t[1]);

  return {
    id: event.id,
    eventId: event.id,
    title,
    description: event.content || '',
    author: pubkeyToNpub(event.pubkey),
    authorPubkey: event.pubkey,
    status: 'open', // Will be updated by status fetch
    created_at: event.created_at || 0,
    updated_at: event.created_at || 0,
    labels,
  };
}

/**
 * Create a new pull request
 */
export async function createPullRequest(
  npub: string,
  repoName: string,
  title: string,
  description: string,
  options: {
    branch?: string;
    targetBranch?: string;
    commitTip?: string;
    labels?: string[];
    /** Custom clone URL for cross-repo PRs (npub/path or nhash format) */
    cloneUrl?: string;
  } = {}
): Promise<PullRequest | null> {
  const state = nostrStore.getState();
  if (!state.pubkey || !ndk.signer) return null;

  const repoAddress = buildRepoAddress(npub, repoName);
  // Use custom clone URL if provided (for cross-repo PRs), otherwise default to target repo
  const cloneUrl = options.cloneUrl || buildHtreeUrl(npub, repoName);

  const event = new NDKEvent(ndk);
  event.kind = KIND_PULL_REQUEST;
  event.content = description;
  event.tags = [
    ['a', repoAddress],
    ['p', npubToPubkey(npub)!], // Notify repo owner
    ['subject', title],
    ['clone', cloneUrl],
  ];

  if (options.branch) {
    event.tags.push(['branch', options.branch]);
  }
  if (options.targetBranch) {
    event.tags.push(['target-branch', options.targetBranch]);
  }
  if (options.commitTip) {
    event.tags.push(['c', options.commitTip]);
  }
  for (const label of options.labels || []) {
    event.tags.push(['t', label]);
  }

  try {
    await event.publish();

    return {
      id: event.id!,
      eventId: event.id!,
      title,
      description,
      author: state.npub!,
      authorPubkey: state.pubkey,
      status: 'open',
      branch: options.branch,
      targetBranch: options.targetBranch || 'main',
      commitTip: options.commitTip,
      cloneUrl,
      created_at: event.created_at || Math.floor(Date.now() / 1000),
      updated_at: event.created_at || Math.floor(Date.now() / 1000),
      labels: options.labels || [],
    };
  } catch (e) {
    console.error('Failed to create pull request:', e);
    return null;
  }
}

/**
 * Create a new issue
 */
export async function createIssue(
  npub: string,
  repoName: string,
  title: string,
  description: string,
  options: {
    labels?: string[];
  } = {}
): Promise<Issue | null> {
  const state = nostrStore.getState();
  if (!state.pubkey || !ndk.signer) return null;

  const repoAddress = buildRepoAddress(npub, repoName);

  const event = new NDKEvent(ndk);
  event.kind = KIND_ISSUE;
  event.content = description;
  event.tags = [
    ['a', repoAddress],
    ['p', npubToPubkey(npub)!], // Notify repo owner
    ['subject', title],
  ];

  for (const label of options.labels || []) {
    event.tags.push(['t', label]);
  }

  try {
    await event.publish();

    return {
      id: event.id!,
      eventId: event.id!,
      title,
      description,
      author: state.npub!,
      authorPubkey: state.pubkey,
      status: 'open',
      created_at: event.created_at || Math.floor(Date.now() / 1000),
      updated_at: event.created_at || Math.floor(Date.now() / 1000),
      labels: options.labels || [],
    };
  } catch (e) {
    console.error('Failed to create issue:', e);
    return null;
  }
}

/**
 * Update the status of a PR or issue
 */
export async function updateStatus(
  targetEventId: string,
  targetAuthorPubkey: string,
  newStatus: ItemStatus
): Promise<boolean> {
  const state = nostrStore.getState();
  if (!state.pubkey || !ndk.signer) return false;

  const statusKind = statusToKind(newStatus);

  const event = new NDKEvent(ndk);
  event.kind = statusKind;
  event.content = '';
  event.tags = [
    ['e', targetEventId],
    ['p', targetAuthorPubkey],
  ];

  try {
    await event.publish();
    return true;
  } catch (e) {
    console.error('Failed to update status:', e);
    return false;
  }
}

/**
 * Fetch comments for a PR or issue
 */
export async function fetchComments(targetEventId: string): Promise<Comment[]> {
  // Comments are kind 1 events that reply to the PR/issue
  const filter: NDKFilter = {
    kinds: [1],
    '#e': [targetEventId],
  };

  const events = await fetchEventsViaSubscribe(filter);
  const comments: Comment[] = [];

  for (const event of events) {
    if (!event.id || !event.pubkey) continue;

    const replyTo = event.tags.find(t => t[0] === 'e' && t[3] === 'reply')?.[1] || targetEventId;

    comments.push({
      id: event.id,
      content: event.content || '',
      author: pubkeyToNpub(event.pubkey),
      authorPubkey: event.pubkey,
      created_at: event.created_at || 0,
      replyTo,
    });
  }

  return comments.sort((a, b) => a.created_at - b.created_at);
}

/**
 * Add a comment to a PR or issue
 */
export async function addComment(
  targetEventId: string,
  targetAuthorPubkey: string,
  content: string,
  repoAddress: string
): Promise<Comment | null> {
  const state = nostrStore.getState();
  if (!state.pubkey || !ndk.signer) return null;

  const event = new NDKEvent(ndk);
  event.kind = 1;
  event.content = content;
  event.tags = [
    ['e', targetEventId, '', 'root'],
    ['p', targetAuthorPubkey],
    ['a', repoAddress],
  ];

  try {
    await event.publish();

    return {
      id: event.id!,
      content,
      author: state.npub!,
      authorPubkey: state.pubkey,
      created_at: event.created_at || Math.floor(Date.now() / 1000),
      replyTo: targetEventId,
    };
  } catch (e) {
    console.error('Failed to add comment:', e);
    return null;
  }
}

/**
 * Publish a repository announcement (kind 30617)
 * This announces that a repo exists and can receive PRs/issues
 */
export async function publishRepoAnnouncement(
  repoName: string,
  options: {
    description?: string;
    webUrl?: string;
    maintainers?: string[]; // npubs of additional maintainers
  } = {}
): Promise<boolean> {
  const state = nostrStore.getState();
  if (!state.pubkey || !state.npub || !ndk.signer) return false;

  const htreeUrl = buildHtreeUrl(state.npub, repoName);

  const event = new NDKEvent(ndk);
  event.kind = KIND_REPO_ANNOUNCEMENT;
  event.content = options.description || '';
  event.tags = [
    ['d', repoName],
    ['name', repoName],
    ['clone', htreeUrl],
  ];

  if (options.webUrl) {
    event.tags.push(['web', options.webUrl]);
  }

  // Add maintainers
  for (const maintainerNpub of options.maintainers || []) {
    const maintainerPubkey = npubToPubkey(maintainerNpub);
    if (maintainerPubkey) {
      event.tags.push(['p', maintainerPubkey, '', 'maintainer']);
    }
  }

  try {
    await event.publish();
    return true;
  } catch (e) {
    console.error('Failed to publish repo announcement:', e);
    return false;
  }
}

/**
 * Encode an event ID to nevent bech32 format for URLs
 * This is more user-friendly and includes relay hints
 */
export function encodeEventId(eventId: string, relays?: string[]): string {
  try {
    return nip19.neventEncode({
      id: eventId,
      relays: relays || [],
    });
  } catch {
    // Fallback to note encoding if nevent fails
    try {
      return nip19.noteEncode(eventId);
    } catch {
      return eventId; // Return hex as last resort
    }
  }
}

/**
 * Decode a nevent/note bech32 string to hex event ID
 */
export function decodeEventId(encoded: string): string | null {
  try {
    // Try nevent first
    if (encoded.startsWith('nevent')) {
      const decoded = nip19.decode(encoded);
      if (decoded.type === 'nevent') {
        return decoded.data.id;
      }
    }
    // Try note
    if (encoded.startsWith('note')) {
      const decoded = nip19.decode(encoded);
      if (decoded.type === 'note') {
        return decoded.data;
      }
    }
    // Assume hex if no prefix
    if (/^[0-9a-f]{64}$/i.test(encoded)) {
      return encoded;
    }
    return null;
  } catch {
    return null;
  }
}
