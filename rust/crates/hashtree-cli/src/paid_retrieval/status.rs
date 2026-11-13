//! What the daemon reports about paid retrieval. Balances, counts and
//! reasons only: never a token and never an account id.

use serde_json::{json, Value};

use super::{BuyingSide, ClientRoute, DaemonPaidRetrieval, SellingSide};

pub(super) fn status(paid: &DaemonPaidRetrieval) -> Value {
    json!({
        "provider": paid.selling.as_ref().map(selling),
        "client": paid.buying.as_ref().map(buying),
    })
}

fn selling(side: &SellingSide) -> Value {
    let config = side.provider.seller().config();
    let accounts = side.provider.accounts();
    let held_msat: i64 = accounts
        .iter()
        .map(|account| account.balance_msat().max(0))
        .fold(0, i64::saturating_add);
    json!({
        "tariff": {
            "msat": config.tariff.rate.msat,
            "per_bytes": config.tariff.rate.per_bytes,
            "min_msat": config.tariff.min_msat,
        },
        "mints": config.mints,
        "min_topup_sat": config.min_topup_sat,
        "grace_msat": config.grace_msat,
        "lock": config.lock.map(hex::encode),
        "paying_only": side.paying_only,
        "http": side.http.is_some(),
        "accounts": accounts.len(),
        "held_msat": held_msat,
    })
}

fn buying(side: &BuyingSide) -> Value {
    json!({
        "spending_limit_sat": side.limit.limit_sat(),
        "spent_sat": side.limit.spent_sat(),
        "remaining_sat": side.limit.remaining_sat(),
        "pays_for_http_reads": side.pays_for_http_reads,
        "providers": side.routes.iter().map(provider).collect::<Vec<_>>(),
    })
}

fn provider(entry: &ClientRoute) -> Value {
    let account = entry.route.account_state();
    let stats = entry.route.stats();
    json!({
        "name": entry.name,
        "address": entry.place.label(),
        "balance_msat": account.balance_msat,
        // Counted since the daemon started.
        "paid_sat": stats.paid_sat,
        "fee_sat": stats.fee_sat,
        "blobs": stats.blobs,
        "bytes": stats.bytes,
        "declined": {
            "price": stats.declined_price,
            "limit": stats.declined_limit,
            "mint": stats.declined_mint,
            "blocked": stats.declined_blocked,
        },
        "rejected_payments": stats.rejected_payments,
        "unresolved_tokens": account.unresolved.len(),
        "unresolved_sat": account.unresolved_msat() / 1_000,
        "payment_pending": account.pending.is_some() || account.intent.is_some(),
        "blocked": account.blocked,
    })
}

/// The lines `htree status` prints for the `paid_retrieval` section.
pub fn format_paid_status(paid: &Value) -> Vec<String> {
    let number = |value: &Value, key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    let sats = |msat: i64| format!("{:.3} sat", msat as f64 / 1_000.0);
    let mut lines = Vec::new();
    if let Some(provider) = paid.get("provider").filter(|value| !value.is_null()) {
        let tariff = &provider["tariff"];
        lines.push(format!(
            "  Selling: {} msat per {} bytes, at least {} msat per blob{}",
            number(tariff, "msat"),
            number(tariff, "per_bytes"),
            number(tariff, "min_msat"),
            match (
                provider["paying_only"].as_bool(),
                provider["http"].as_bool()
            ) {
                (Some(true), Some(true)) => ", paying only, also over HTTP",
                (Some(true), _) => ", paying only",
                (_, Some(true)) => ", also over HTTP",
                _ => "",
            }
        ));
        // Only a running daemon has its ledger open.
        if provider.get("accounts").is_some() {
            lines.push(format!(
                "  Accounts: {}, holding {}",
                number(provider, "accounts"),
                sats(provider["held_msat"].as_i64().unwrap_or(0))
            ));
        }
    }
    if let Some(client) = paid.get("client").filter(|value| !value.is_null()) {
        lines.push(format!(
            "  Spending limit: {} sat, {} spent, {} remaining",
            number(client, "spending_limit_sat"),
            number(client, "spent_sat"),
            number(client, "remaining_sat")
        ));
        let providers = client["providers"].as_array();
        for provider in providers.into_iter().flatten() {
            let name = provider["name"].as_str().unwrap_or("?");
            let balance = sats(provider["balance_msat"].as_i64().unwrap_or(0));
            // Counts since the start exist only in a running daemon.
            match provider.get("declined") {
                Some(declined) => {
                    lines.push(format!(
                        "  {name}: balance {balance}, paid {} sat, {} blob(s)",
                        number(provider, "paid_sat"),
                        number(provider, "blobs")
                    ));
                    lines.push(format!(
                        "    Declined: {} price, {} limit, {} mint, {} blocked",
                        number(declined, "price"),
                        number(declined, "limit"),
                        number(declined, "mint"),
                        number(declined, "blocked"),
                    ));
                }
                None => lines.push(format!("  {name}: balance {balance}")),
            }
            lines.push(format!(
                "    Unresolved tokens: {} ({} sat){}",
                number(provider, "unresolved_tokens"),
                number(provider, "unresolved_sat"),
                match provider["payment_pending"].as_bool() {
                    Some(true) => "; a payment awaits acknowledgement",
                    _ => "",
                }
            ));
            if let Some(reason) = provider["blocked"].as_str() {
                lines.push(format!("    Blocked: {reason}"));
            }
        }
    }
    lines
}
