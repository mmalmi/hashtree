import { finalizeEvent, generateSecretKey, getEventHash, getPublicKey, nip59, nip44, verifyEvent, } from 'nostr-tools';
export const SIGNALING_KIND = 25050;
export const HELLO_TAG = 'hello';
export const MAX_EVENT_AGE_SEC = 30;
const HELLO_EXPIRATION_SEC = 5 * 60;
const NIP59_SEAL_KIND = 13;
function getSince(nowMs, maxEventAgeSec) {
    return Math.floor((nowMs - maxEventAgeSec * 1000) / 1000);
}
function isExpired(event, nowSec, maxEventAgeSec) {
    const createdAt = event.created_at ?? 0;
    if (nowSec - createdAt > maxEventAgeSec) {
        return true;
    }
    const expirationTag = event.tags.find((tag) => tag[0] === 'expiration');
    if (expirationTag?.[1]) {
        const expiration = Number.parseInt(expirationTag[1], 10);
        if (Number.isFinite(expiration) && expiration < nowSec) {
            return true;
        }
    }
    return false;
}
function normalizePeerEndpoint(value, senderPubkey) {
    const trimmed = value.trim();
    if (!trimmed)
        return senderPubkey;
    return trimmed.includes(':') ? senderPubkey : trimmed;
}
function decodeHashGetTag(value) {
    if (value === undefined)
        return true;
    return !['0', 'false', 'FALSE', 'no', 'NO'].includes(value);
}
function isSignalingKind(kind) {
    return kind === undefined || kind === SIGNALING_KIND;
}
function hasCompleteEventFields(value) {
    if (!value || typeof value !== 'object')
        return false;
    const event = value;
    return (typeof event.id === 'string' &&
        typeof event.pubkey === 'string' &&
        typeof event.kind === 'number' &&
        typeof event.created_at === 'number' &&
        Array.isArray(event.tags) &&
        typeof event.content === 'string' &&
        typeof event.sig === 'string');
}
function verifyNostrEvent(value) {
    if (!hasCompleteEventFields(value))
        return null;
    const eventForVerification = {
        id: value.id,
        pubkey: value.pubkey,
        kind: value.kind,
        created_at: value.created_at,
        tags: value.tags,
        content: value.content,
        sig: value.sig,
    };
    if (!verifyEvent(eventForVerification))
        return null;
    return value;
}
function signedEventToSeal(value) {
    const event = verifyNostrEvent(value);
    if (!event || !isSignalingKind(event.kind))
        return null;
    return {
        pubkey: event.pubkey,
        kind: event.kind,
        content: event.content,
        tags: event.tags,
    };
}
function parseJson(value) {
    return JSON.parse(value);
}
function createRumor(innerEvent, senderPubkey, createdAt) {
    const rumor = {
        kind: innerEvent.kind,
        content: innerEvent.content,
        tags: innerEvent.tags,
        created_at: createdAt,
        pubkey: senderPubkey,
    };
    return {
        ...rumor,
        id: getEventHash(rumor),
    };
}
function createOuterGiftWrap(encryptedContent, recipientPubkey, createdAt, expirationSec, ephemeralSecretKey) {
    return finalizeEvent({
        kind: SIGNALING_KIND,
        created_at: createdAt,
        tags: [
            ['p', recipientPubkey],
            ['expiration', String(createdAt + expirationSec)],
        ],
        content: encryptedContent,
    }, ephemeralSecretKey);
}
function encryptOuterGiftWrapPayload(payload, recipientPubkey, ephemeralSecretKey) {
    const conversationKey = nip44.v2.utils.getConversationKey(ephemeralSecretKey, recipientPubkey);
    return nip44.v2.encrypt(JSON.stringify(payload), conversationKey);
}
function normalizeSignalingMessage(raw, senderPubkey) {
    if (!raw || typeof raw !== 'object' || !('type' in raw))
        return null;
    const msg = raw;
    if (typeof msg.type !== 'string')
        return null;
    if (typeof msg.peerId === 'string' && msg.type === 'hello') {
        return {
            ...msg,
            peerId: senderPubkey,
            hashGet: msg.hashGet !== false,
        };
    }
    if ('targetPeerId' in msg &&
        typeof msg.targetPeerId === 'string' &&
        typeof msg.peerId === 'string') {
        return {
            ...msg,
            peerId: senderPubkey,
            targetPeerId: normalizePeerEndpoint(msg.targetPeerId, senderPubkey),
        };
    }
    return null;
}
export function createSignalingFilters(myPubkey, nowMs = Date.now(), maxEventAgeSec = MAX_EVENT_AGE_SEC) {
    const since = getSince(nowMs, maxEventAgeSec);
    return {
        since,
        helloFilter: {
            kinds: [SIGNALING_KIND],
            '#l': [HELLO_TAG],
            since,
        },
        directedFilter: {
            kinds: [SIGNALING_KIND],
            '#p': [myPubkey],
            since,
        },
    };
}
export async function sendSignalingMessage({ msg, recipientPubkey, signEvent, giftWrap, publish, nowMs = () => Date.now(), }) {
    if (recipientPubkey) {
        const wrappedEvent = await giftWrap({
            kind: SIGNALING_KIND,
            content: JSON.stringify(msg),
            tags: [],
        }, recipientPubkey);
        await publish(wrappedEvent);
        return;
    }
    const createdAt = Math.floor(nowMs() / 1000);
    const event = await signEvent({
        kind: SIGNALING_KIND,
        created_at: createdAt,
        tags: [
            ['l', HELLO_TAG],
            ['peerId', msg.peerId],
            ['hashGet', msg.type === 'hello' && msg.hashGet === false ? '0' : '1'],
            ['expiration', String(createdAt + HELLO_EXPIRATION_SEC)],
        ],
        content: '',
    });
    await publish(event);
}
export function createSimplePoolSignalingSender({ signEvent, giftWrap, publishMode = 'require-one', publishMaxWaitMs, nowMs, }) {
    return ({ signalPool, relayUrls }) => {
        return async (msg, recipientPubkey) => {
            await sendSignalingMessage({
                msg,
                recipientPubkey,
                signEvent,
                giftWrap,
                publish: async (event) => {
                    const publishPromises = signalPool.publish(relayUrls, event);
                    if (publishMode === 'best-effort') {
                        await Promise.allSettled(publishPromises);
                        return;
                    }
                    const publishResult = Promise.any(publishPromises);
                    if (publishMaxWaitMs === undefined) {
                        await publishResult;
                        return;
                    }
                    await Promise.race([
                        publishResult,
                        new Promise((_, reject) => {
                            setTimeout(() => reject(new Error('Timed out publishing signaling event')), publishMaxWaitMs);
                        }),
                    ]);
                },
                nowMs,
            });
        };
    };
}
export function createSecretKeyEventSigner(secretKey) {
    return async (template) => finalizeEvent(template, secretKey);
}
export function createSecretKeyNip44GiftWrap(senderSecretKey, options = {}) {
    const nowMs = options.nowMs ?? (() => Date.now());
    const expirationSec = options.expirationSec ?? HELLO_EXPIRATION_SEC;
    return async (innerEvent, recipientPubkey) => {
        const senderPubkey = getPublicKey(senderSecretKey);
        const rumor = nip59.createRumor({
            kind: innerEvent.kind,
            content: innerEvent.content,
            tags: innerEvent.tags,
        }, senderSecretKey);
        const seal = nip59.createSeal(rumor, senderSecretKey, recipientPubkey);
        const legacyReadableSeal = {
            pubkey: senderPubkey,
            kind: innerEvent.kind,
            content: innerEvent.content,
            tags: innerEvent.tags,
            seal,
        };
        const ephemeralSecretKey = generateSecretKey();
        const createdAt = Math.floor(nowMs() / 1000);
        const encryptedContent = encryptOuterGiftWrapPayload(legacyReadableSeal, recipientPubkey, ephemeralSecretKey);
        return createOuterGiftWrap(encryptedContent, recipientPubkey, createdAt, expirationSec, ephemeralSecretKey);
    };
}
export function createAuthenticatedNip44GiftWrap({ senderPubkey, signEvent, encrypt, nowMs = () => Date.now(), expirationSec = HELLO_EXPIRATION_SEC, }) {
    return async (innerEvent, recipientPubkey) => {
        const createdAt = Math.floor(nowMs() / 1000);
        const rumor = createRumor(innerEvent, senderPubkey, createdAt);
        const seal = await signEvent({
            kind: NIP59_SEAL_KIND,
            created_at: createdAt,
            tags: [],
            content: await encrypt(recipientPubkey, JSON.stringify(rumor)),
        });
        const legacyReadableSeal = {
            pubkey: senderPubkey,
            kind: innerEvent.kind,
            content: innerEvent.content,
            tags: innerEvent.tags,
            seal,
        };
        const ephemeralSecretKey = generateSecretKey();
        const encryptedContent = encryptOuterGiftWrapPayload(legacyReadableSeal, recipientPubkey, ephemeralSecretKey);
        return createOuterGiftWrap(encryptedContent, recipientPubkey, createdAt, expirationSec, ephemeralSecretKey);
    };
}
export function createNip44GiftWrap(senderPubkey, options = {}) {
    const nowMs = options.nowMs ?? (() => Date.now());
    const expirationSec = options.expirationSec ?? HELLO_EXPIRATION_SEC;
    return async (innerEvent, recipientPubkey) => {
        const seal = {
            pubkey: senderPubkey,
            kind: innerEvent.kind,
            content: innerEvent.content,
            tags: innerEvent.tags,
        };
        const ephemeralSecretKey = generateSecretKey();
        const createdAt = Math.floor(nowMs() / 1000);
        const encryptedContent = encryptOuterGiftWrapPayload(seal, recipientPubkey, ephemeralSecretKey);
        return createOuterGiftWrap(encryptedContent, recipientPubkey, createdAt, expirationSec, ephemeralSecretKey);
    };
}
export function createDecryptingGiftUnwrapper(decrypt) {
    return async (event) => {
        try {
            const content = await decrypt(event.pubkey, event.content);
            const unwrapped = parseJson(content);
            const directSignedEvent = signedEventToSeal(unwrapped);
            if (directSignedEvent)
                return directSignedEvent;
            if (!unwrapped || typeof unwrapped !== 'object')
                return null;
            const seal = unwrapped;
            if ('event' in seal) {
                return signedEventToSeal(seal.event);
            }
            if ('seal' in seal) {
                const sealEvent = verifyNostrEvent(seal.seal);
                if (!sealEvent || sealEvent.kind !== NIP59_SEAL_KIND)
                    return null;
                const rumorPlaintext = await decrypt(sealEvent.pubkey, sealEvent.content);
                const rumor = parseJson(rumorPlaintext);
                if (!rumor || typeof rumor !== 'object')
                    return null;
                const rumorEvent = rumor;
                if (rumorEvent.pubkey !== sealEvent.pubkey ||
                    typeof rumorEvent.kind !== 'number' ||
                    typeof rumorEvent.content !== 'string' ||
                    !Array.isArray(rumorEvent.tags)) {
                    return null;
                }
                return {
                    pubkey: sealEvent.pubkey,
                    kind: rumorEvent.kind,
                    content: rumorEvent.content,
                    tags: rumorEvent.tags,
                };
            }
            if (typeof seal.pubkey === 'string' &&
                typeof seal.kind === 'number' &&
                typeof seal.content === 'string' &&
                Array.isArray(seal.tags)) {
                return {
                    pubkey: seal.pubkey,
                    kind: seal.kind,
                    content: seal.content,
                    tags: seal.tags,
                };
            }
            return null;
        }
        catch {
            return null;
        }
    };
}
export function createSecretKeyGiftUnwrapper(secretKey) {
    return createDecryptingGiftUnwrapper((senderPubkey, ciphertext) => {
        const conversationKey = nip44.v2.utils.getConversationKey(secretKey, senderPubkey);
        return nip44.v2.decrypt(ciphertext, conversationKey);
    });
}
export async function decodeSignalingEvent({ event, giftUnwrap, nowMs = () => Date.now(), maxEventAgeSec = MAX_EVENT_AGE_SEC, }) {
    if (hasCompleteEventFields(event)) {
        if (!verifyNostrEvent(event) || event.kind !== SIGNALING_KIND) {
            return null;
        }
    }
    else if (!isSignalingKind(event.kind)) {
        return null;
    }
    const nowSec = nowMs() / 1000;
    if (isExpired(event, nowSec, maxEventAgeSec)) {
        return null;
    }
    const isHello = event.tags.some((tag) => tag[0] === 'l' && tag[1] === HELLO_TAG);
    if (isHello) {
        const peerIdTag = event.tags.find((tag) => tag[0] === 'peerId');
        if (!peerIdTag?.[1])
            return null;
        const senderPeerId = normalizePeerEndpoint(event.pubkey, event.pubkey);
        if (normalizePeerEndpoint(peerIdTag[1], event.pubkey) !== senderPeerId) {
            return null;
        }
        return {
            senderPubkey: event.pubkey,
            message: {
                type: 'hello',
                peerId: senderPeerId,
                hashGet: decodeHashGetTag(event.tags.find((tag) => tag[0] === 'hashGet')?.[1]),
            },
        };
    }
    const seal = await giftUnwrap(event);
    if (!seal?.content) {
        return null;
    }
    try {
        const raw = JSON.parse(seal.content);
        const message = normalizeSignalingMessage(raw, seal.pubkey);
        if (!message)
            return null;
        return {
            senderPubkey: seal.pubkey,
            message,
        };
    }
    catch {
        return null;
    }
}
//# sourceMappingURL=signaling.js.map