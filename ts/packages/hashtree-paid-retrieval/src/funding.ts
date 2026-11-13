/** The client's payment step: resend, top up, reclaim. */

import { lossMsat, unresolvedMsat, type PendingPayment } from './account.js';
import type { BuyerCore } from './buyerCore.js';
import { withDeadline } from './deadline.js';
import { METHOD_CASHU, PAYMENT_ID_BYTES, bytesEqual, type Quote } from './message.js';
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
 * A read that reaches its deadline stops waiting, but the step itself runs
 * on: a token the wallet already created is never dropped.
 */
export function runFunding(
  core: BuyerCore,
  quote: Quote | null,
  deadlineMs: number,
  signal?: AbortSignal,
): Promise<boolean> {
  return withDeadline(
    core.payingOnly(() => fund(core, quote)),
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

async function fund(core: BuyerCore, quote: Quote | null): Promise<boolean> {
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
  const own = policy.mints.map(normalizeMint);
  const mints = terms.mints.filter(
    (mint) => own.length === 0 || own.includes(normalizeMint(mint)),
  );
  if (mints.length === 0 || !terms.methods.includes(METHOD_CASHU)) {
    stats.declinedMint += 1;
    return false;
  }

  const id = crypto.getRandomValues(new Uint8Array(PAYMENT_ID_BYTES));
  let amountSat = policy.topupSat;
  if (amountSat < minSat) amountSat = minSat;
  if (amountSat > maxSat) amountSat = maxSat;
  let payment: WalletPayment;
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
      payment = checked(await core.wallet.pay({
        provider: core.provider,
        mints: [...mints],
        amountSat,
        maxFeeSat: policy.maxTokenFeeSat,
        paymentId: id.slice(),
      }));
      break;
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
      return false;
    }
  }
  await adopt(core, id, payment);
  await settlePending(core);
  return true;
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
  const { account } = core;
  const pending = account.pending;
  if (pending === null) return;
  const sent = await send(core, pending);
  const hasRoom = account.unresolved.length < MAX_UNRESOLVED;
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
      if (hasRoom) {
        account.pending = null;
        account.unresolved.push(pending);
      }
      failure = sent.refusal;
      break;
    case 'not-yet':
      pending.attempts = Math.min(pending.attempts + 1, 0xffff_ffff);
      if (pending.attempts >= MAX_ATTEMPTS && hasRoom) {
        // Stop holding up new payments; this one is tried again later and
        // still counts as money held.
        account.pending = null;
        account.unresolved.push(pending);
      } else {
        failure = `paid route provider could not take the payment yet: ${sent.detail}`;
      }
      break;
  }
  await core.persistIfPossible();
  if (failure !== null) throw new Error(failure);
}

/** Try the oldest unresolved token once more. Failures leave it in place. */
async function retryUnresolved(core: BuyerCore): Promise<void> {
  const { unresolved } = core.account;
  const oldest = unresolved[0];
  if (oldest === undefined) return;
  const outcome = await send(core, oldest).then((sent) => sent.outcome, () => 'failed' as const);
  const index = unresolved.indexOf(oldest);
  if (index < 0) return;
  unresolved.splice(index, 1);
  if (outcome !== 'credited' && outcome !== 'reclaimed') unresolved.push(oldest);
  await core.persistIfPossible();
}

/** One `pay` exchange for one token, with its accounting. */
async function send(core: BuyerCore, pending: PendingPayment): Promise<Sent> {
  const { account, stats, policy } = core;
  const { payment } = pending;
  const reply = await core.exchange(
    {
      type: 'pay',
      account: account.account,
      payment: pending.id,
      method: METHOD_CASHU,
      mint: payment.mintUrl,
      token: payment.token,
    },
    Date.now() + policy.requestTimeoutMs,
  );
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
