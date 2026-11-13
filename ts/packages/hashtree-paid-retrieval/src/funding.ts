/** The client's payment step: resend, top up, reclaim. */

import { lossMsat } from './account.js';
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

async function fund(core: BuyerCore, quote: Quote | null): Promise<boolean> {
  await sendPending(core);

  const { policy, account, stats } = core;
  // Requests still in flight may explain part of a low balance, but never
  // more than they authorised.
  const allowed = saturate(policy.maxLossMsat + core.inFlightMsat);
  const loss = lossMsat(account);
  if (account.blocked === null && loss > allowed) core.block(loss);
  if (account.blocked !== null) return false;

  const terms = core.terms;
  const balance = account.balanceMsat;
  const need = (quote?.priceMsat ?? 0n) - balance - (quote?.graceMsat ?? 0n);
  // Already covered, perhaps by a concurrent read's top-up.
  if (need <= 0n || need > U64_MAX || terms === null) return true;

  const maxSat = satCeil(saturate(policy.maxPrepaidMsat - balance));
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

  let amountSat = policy.topupSat;
  if (amountSat < minSat) amountSat = minSat;
  if (amountSat > maxSat) amountSat = maxSat;
  let payment: WalletPayment;
  for (;;) {
    try {
      payment = checked(await core.wallet.pay({
        provider: core.provider,
        mints: [...mints],
        amountSat,
      }));
      break;
    } catch (thrown) {
      const error = WalletError.from(thrown);
      if (error.kind === 'no-accepted-mint') {
        stats.declinedMint += 1;
        return false;
      }
      if (!error.isDecline) throw new Error(`paid route ${error.message}`);
      if (amountSat <= minSat) {
        stats.declinedLimit += 1;
        return false;
      }
      // The preferred top-up did not fit; the bare minimum may.
      amountSat = minSat;
    }
  }
  stats.paidSat = saturate(stats.paidSat + payment.amountSat);
  stats.feeSat = saturate(stats.feeSat + payment.feeSat);
  account.pending = {
    id: crypto.getRandomValues(new Uint8Array(PAYMENT_ID_BYTES)),
    payment,
  };
  await sendPending(core);
  return true;
}

/**
 * Hand the pending token to the provider. The token stays pending until the
 * provider credits it or refuses it for good.
 */
async function sendPending(core: BuyerCore): Promise<void> {
  const { account, stats } = core;
  const pending = account.pending;
  if (pending === null) return;
  const reply = await core.exchange(
    {
      type: 'pay',
      account: account.account,
      payment: pending.id,
      method: METHOD_CASHU,
      mint: pending.payment.mintUrl,
      token: pending.payment.token,
    },
    Date.now() + core.policy.requestTimeoutMs,
  );
  if (reply.type === 'paid' && bytesEqual(reply.payment, pending.id)) {
    account.pending = null;
    account.creditedMsat = saturate(account.creditedMsat + reply.creditedMsat);
    core.credits += 1;
    core.noteBalance(reply.balanceMsat, null);
    return;
  }
  if (reply.type !== 'err') {
    throw new Error('paid route provider sent an unexpected reply to a payment');
  }
  if (reply.code === 'retry') {
    throw new Error(`paid route provider could not take the payment yet: ${reply.detail}`);
  }
  // Refused for good: the token is ours again if the wallet can take it
  // back. Cleared only afterwards, so an interrupted reclaim is repeated.
  let reclaimed = false;
  if (core.wallet.reclaim) {
    reclaimed = await core.wallet.reclaim(pending.payment).then(() => true, () => false);
  }
  account.pending = null;
  stats.rejectedPayments += 1;
  if (reclaimed) stats.paidSat = saturate(stats.paidSat - pending.payment.amountSat);
  throw new Error(`paid route provider refused the payment (${reply.code}): ${reply.detail}`);
}
