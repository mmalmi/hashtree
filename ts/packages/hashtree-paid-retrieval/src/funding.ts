/** The client's payment step: resend, top up, reclaim. */

import { lossMsat, unresolvedMsat, wirePayment, type PendingPayment } from './account.js';
import type { BuyerCore, Terms } from './buyerCore.js';
import { withDeadline } from './deadline.js';
import { METHOD_CASHU, PAYMENT_ID_BYTES, bytesEqual, type Quote, type Reply } from './message.js';
import { U64_MAX, satCeil, saturate } from './price.js';
import { WalletError, type WalletPayment } from './wallet.js';

/** Unicode White_Space, as Rust's `str::trim` removes it. */
function isWhiteSpace(code: number): boolean {
  return (code >= 0x09 && code <= 0x0d) || code === 0x20 || code === 0x85 || code === 0xa0
    || code === 0x1680 || (code >= 0x2000 && code <= 0x200a) || code === 0x2028
    || code === 0x2029 || code === 0x202f || code === 0x205f || code === 0x3000;
}

export function normalizeMint(mintUrl: string): string {
  let start = 0;
  let end = mintUrl.length;
  while (start < end && isWhiteSpace(mintUrl.charCodeAt(start))) start += 1;
  while (end > start && isWhiteSpace(mintUrl.charCodeAt(end - 1))) end -= 1;
  while (end > start && mintUrl[end - 1] === '/') end -= 1;
  return mintUrl.slice(start, end);
}

/** Answers of "not yet" after which a payment stops holding up new ones. */
const MAX_ATTEMPTS = 3;
/** Tokens of unknown fate kept per provider; beyond it nothing more is paid. */
const MAX_UNRESOLVED = 8;

/** The provider's mints that the client policy allows; any of them when it names none. */
function acceptedMints(own: string[], offered: string[]): string[] {
  const allowed = own.map(normalizeMint);
  return offered.filter((mint) => allowed.length === 0 || allowed.includes(normalizeMint(mint)));
}

function isAmount(value: unknown): value is bigint {
  return typeof value === 'bigint' && value >= 0n && value <= U64_MAX;
}

function checked(payment: WalletPayment): WalletPayment {
  if (
    typeof payment?.mintUrl !== 'string' || typeof payment.token !== 'string'
    || !isAmount(payment.amountSat) || !isAmount(payment.feeSat)
  ) {
    throw WalletError.failed('the wallet returned an invalid payment');
  }
  return payment;
}

/**
 * Make the account able to pay for `quote` (or, without one, settle what is
 * outstanding). Resolves to `false` when the client declines to pay.
 *
 * With `carry`, a new token is left pending for the caller to carry in its
 * next request instead of being sent on its own.
 *
 * A read that reaches its deadline stops waiting, but the step itself runs
 * on: a token the wallet already created is never dropped.
 */
export function runFunding(
  core: BuyerCore,
  quote: Quote | null,
  carry: boolean,
  deadlineMs: number,
  signal?: AbortSignal,
): Promise<boolean> {
  return withDeadline(
    core.payingOnly(() => fund(core, quote, carry)),
    deadlineMs,
    signal,
    'paid route payment is still in progress; availability is unknown',
  );
}

/** What became of one token handed to the provider. */
type Sent =
  | { outcome: 'credited' }
  /** Refused for good, and the wallet took the token back. */
  | { outcome: 'reclaimed'; refusal: string }
  /** Refused for good, and the wallet could not take it back. */
  | { outcome: 'refused'; refusal: string }
  /** The provider cannot take it yet. */
  | { outcome: 'not-yet'; detail: string };

async function fund(core: BuyerCore, quote: Quote | null, carry: boolean): Promise<boolean> {
  await recoverIntent(core);
  await settlePending(core);
  await retryUnresolved(core);

  const { policy, account, stats } = core;
  // Requests still in flight may explain part of a low balance, but never
  // more than they authorised.
  const allowed = saturate(policy.maxLossMsat + core.inFlightMsat);
  const loss = lossMsat(account);
  if (account.blocked === null && !core.resync && loss > allowed) core.block(loss);
  if (account.blocked !== null) return false;

  const terms = core.terms;
  const balance = account.balanceMsat;
  const need = (quote?.priceMsat ?? 0n) - balance - (quote?.graceMsat ?? 0n);
  // Already covered, perhaps by a concurrent read's top-up.
  if (need <= 0n || need > U64_MAX || terms === null) return true;

  // Tokens of unknown fate may still be redeemed by the provider, so they
  // count as money it holds.
  const maxSat = satCeil(saturate(policy.maxPrepaidMsat - balance - unresolvedMsat(account)));
  const needSat = satCeil(need);
  const minSat = needSat > terms.minTopupSat ? needSat : terms.minTopupSat;
  if (minSat > maxSat) {
    stats.declinedLimit += 1;
    return false;
  }
  const mints = acceptedMints(policy.mints, terms.mints);
  if (mints.length === 0 || !terms.methods.includes(METHOD_CASHU)) {
    stats.declinedMint += 1;
    return false;
  }

  const token = await requestToken(core, terms, mints, minSat, maxSat);
  if (token === null) return false;
  const { id, payment } = token;
  await adopt(core, id, payment);
  if (!carry) await settlePending(core);
  return true;
}

/**
 * Ask the wallet for a token of the preferred top-up, or the bare minimum if
 * that does not fit. The intent is saved first. Resolves to `null` when the
 * wallet declines.
 */
async function requestToken(
  core: BuyerCore,
  terms: Terms,
  mints: string[],
  minSat: bigint,
  maxSat: bigint,
): Promise<{ id: Uint8Array; payment: WalletPayment } | null> {
  const { account, policy, stats } = core;
  const id = crypto.getRandomValues(new Uint8Array(PAYMENT_ID_BYTES));
  let amountSat = policy.topupSat;
  if (amountSat < minSat) amountSat = minSat;
  if (amountSat > maxSat) amountSat = maxSat;
  for (;;) {
    // The intent is saved before the wallet is asked: a crash after this
    // point is followed by asking the wallet about this id.
    account.intent = { id: id.slice(), amountSat, mints: [...mints] };
    try {
      await core.persist();
    } catch (error) {
      account.intent = null;
      throw error;
    }
    try {
      const payment = checked(await core.wallet.pay({
        provider: core.provider,
        mints: [...mints],
        amountSat,
        maxFeeSat: policy.maxTokenFeeSat,
        lock: terms.lock === null ? null : terms.lock.slice(),
        paymentId: id.slice(),
      }));
      return { id, payment };
    } catch (thrown) {
      const error = WalletError.from(thrown);
      // The wallet may have created a token all the same: the intent stays,
      // and the wallet is asked about it next time.
      if (!error.isDecline) throw new Error(`paid route ${error.message}`);
      if (amountSat > minSat) {
        // The preferred top-up did not fit; the bare minimum may.
        amountSat = minSat;
        continue;
      }
      account.intent = null;
      if (error.kind === 'no-accepted-mint') stats.declinedMint += 1;
      else stats.declinedLimit += 1;
      await core.persistIfPossible();
      return null;
    }
  }
}

/**
 * Make a token the wallet produced the pending payment, and save it before
 * it is sent anywhere.
 */
async function adopt(core: BuyerCore, id: Uint8Array, payment: WalletPayment): Promise<void> {
  const { account, stats } = core;
  stats.paidSat = saturate(stats.paidSat + payment.amountSat);
  stats.feeSat = saturate(stats.feeSat + payment.feeSat);
  account.intent = null;
  account.pending = { id, payment, attempts: 0 };
  // If this save fails the older record still names the intent, and the
  // wallet is asked for the token again after a restart.
  await core.persistIfPossible();
}

/**
 * After a restart: a payment the wallet was asked for may exist without this
 * account knowing its token.
 */
async function recoverIntent(core: BuyerCore): Promise<void> {
  const { account, wallet } = core;
  const intent = account.intent;
  if (intent === null || account.pending !== null) return;
  let found: WalletPayment | null;
  try {
    const recovered = wallet.recover ? await wallet.recover(intent.id.slice()) : null;
    found = recovered ? checked(recovered) : null;
  } catch (error) {
    throw new Error(`paid route ${WalletError.from(error).message}`);
  }
  if (found) {
    await adopt(core, intent.id, found);
    return;
  }
  // The wallet made no token it can hand over; if it made one at all, it is
  // in the wallet's own records.
  account.intent = null;
  await core.persistIfPossible();
}

/**
 * Hand the pending token to the provider. It stays pending until the provider
 * credits it, the wallet takes it back, or it is set aside as unresolved; it
 * is never dropped.
 */
async function settlePending(core: BuyerCore): Promise<void> {
  const pending = core.account.pending;
  if (pending === null) return;
  await conclude(core, pending, await send(core, pending));
}

/**
 * A payment carried by a `get`: settle it from the reply, if the reply says
 * what became of it. Resolves to whether it was credited. The caller holds the
 * payment lock from before the request was sent.
 */
export async function settleCarried(
  core: BuyerCore,
  pending: PendingPayment,
  reply: Reply,
): Promise<boolean> {
  const credited = reply.type === 'data'
    ? { credit: reply.credit, balanceMsat: reply.balanceMsat }
    : reply.type === 'quote'
      ? { credit: reply.quote.credit, balanceMsat: reply.quote.balanceMsat }
      : null;
  let answer: Reply;
  if (credited?.credit && bytesEqual(credited.credit.payment, pending.id)) {
    answer = {
      type: 'paid',
      payment: credited.credit.payment,
      creditedMsat: credited.credit.creditedMsat,
      balanceMsat: credited.balanceMsat,
    };
  } else if (reply.type === 'err' && reply.payment && bytesEqual(reply.payment, pending.id)) {
    answer = reply;
  } else {
    return false;
  }
  // A concurrent request may have settled the same token already.
  const current = core.account.pending;
  if (current === null || !bytesEqual(current.id, pending.id)) return false;
  const sent = await outcome(core, pending, answer);
  await conclude(core, pending, sent);
  return sent.outcome === 'credited';
}

/** Record what became of the pending token; rejects unless it was credited or set aside. */
async function conclude(core: BuyerCore, pending: PendingPayment, sent: Sent): Promise<void> {
  const { account } = core;
  const hasRoom = account.unresolved.length < MAX_UNRESOLVED;
  const setAside = (payment: PendingPayment): void => {
    account.pending = null;
    account.unresolved.push(payment);
  };
  let failure: string | null = null;
  switch (sent.outcome) {
    case 'credited':
      account.pending = null;
      break;
    case 'reclaimed':
      account.pending = null;
      failure = sent.refusal;
      break;
    case 'refused':
      // Neither credited nor back in the wallet. A provider that refuses a
      // token may still redeem it.
      if (hasRoom) setAside(pending);
      failure = sent.refusal;
      break;
    case 'not-yet': {
      const attempts = Math.min(pending.attempts + 1, 0xffff_ffff);
      if (attempts >= MAX_ATTEMPTS && hasRoom) {
        // Stop holding up new payments; this one is tried again later and
        // still counts as money held.
        setAside({ ...pending, attempts });
      } else {
        if (account.pending !== null) account.pending.attempts = attempts;
        failure = `paid route provider could not take the payment yet: ${sent.detail}`;
      }
      break;
    }
  }
  await core.persistIfPossible();
  if (failure !== null) throw new Error(failure);
}

/** Try the oldest unresolved token once more. Failures leave it in place. */
async function retryUnresolved(core: BuyerCore): Promise<void> {
  const { unresolved } = core.account;
  const oldest = unresolved[0];
  if (oldest === undefined) return;
  const result = await send(core, oldest).then((sent) => sent.outcome, () => 'failed' as const);
  const index = unresolved.indexOf(oldest);
  if (index < 0) return;
  unresolved.splice(index, 1);
  if (result !== 'credited' && result !== 'reclaimed') unresolved.push(oldest);
  await core.persistIfPossible();
}

/** One `pay` exchange for one token, with its accounting. */
async function send(core: BuyerCore, pending: PendingPayment): Promise<Sent> {
  const reply = await core.exchange(
    { type: 'pay', account: core.wireAccount(), payment: wirePayment(pending) },
    Date.now() + core.policy.requestTimeoutMs,
  );
  return outcome(core, pending, reply);
}

/** What one reply says about one token handed to the provider, with its accounting. */
async function outcome(core: BuyerCore, pending: PendingPayment, reply: Reply): Promise<Sent> {
  const { account, stats, policy } = core;
  const { payment } = pending;
  if (reply.type === 'paid' && bytesEqual(reply.payment, pending.id)) {
    // A provider may keep a little of a token for its mint's redemption fee.
    // Anything beyond the allowance is owed: it is added here, missing from
    // the reported balance, and so counted as a loss.
    const shortfall = saturate(saturate(payment.amountSat * 1000n) - reply.creditedMsat);
    const fee = shortfall < policy.maxRedeemFeeMsat ? shortfall : policy.maxRedeemFeeMsat;
    stats.redeemFeeMsat = saturate(stats.redeemFeeMsat + fee);
    account.creditedMsat = saturate(
      saturate(account.creditedMsat + reply.creditedMsat) + (shortfall - fee),
    );
    core.credits += 1;
    core.noteBalance(reply.balanceMsat, null);
    return { outcome: 'credited' };
  }
  if (reply.type !== 'err') {
    throw new Error('paid route provider sent an unexpected reply to a payment');
  }
  if (reply.code === 'retry') return { outcome: 'not-yet', detail: reply.detail };
  const refusal = `paid route provider refused the payment (${reply.code}): ${reply.detail}`;
  let reclaimed = false;
  if (core.wallet.reclaim) {
    reclaimed = await core.wallet.reclaim(payment).then(() => true, () => false);
  }
  stats.rejectedPayments += 1;
  if (!reclaimed) return { outcome: 'refused', refusal };
  stats.paidSat = saturate(stats.paidSat - payment.amountSat);
  return { outcome: 'reclaimed', refusal };
}
