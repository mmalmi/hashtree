//! `htree paid` against real account files, with a fake mint and a gateway
//! on loopback HTTP. No network beyond loopback, no real funds.

use hashtree_paid_retrieval::test_support::{FakeMint, FakeReceiver, FakeWallet, MINT};
use hashtree_paid_retrieval::{AccountState, AccountStore, PendingPayment, WalletPayment};

use super::tests::serve_http;
use super::*;

/// A daemon configuration that buys from one gateway, named "gateway".
fn buying_config(gateway: &str) -> Config {
    toml::from_str(&format!(
        r#"[cashu]
accepted_mints = ["{MINT}"]

[paid_retrieval.client]
[[paid_retrieval.client.providers]]
name = "gateway"
url = "{gateway}""#
    ))
    .unwrap()
}

/// The account file of the configured gateway.
fn account_store(config: &Config, data_dir: &Path) -> FileAccountStore {
    let settings = commands_settings(config, data_dir);
    let accounts = data_dir.join(PAID_RETRIEVAL_DIR).join("accounts");
    std::fs::create_dir_all(&accounts).unwrap();
    FileAccountStore::new(accounts.join(account_file(&settings.providers[0].place)))
}

fn commands_settings(config: &Config, data_dir: &Path) -> ClientSettings {
    let mints = &config.cashu.accepted_mints;
    let settings = PaidRetrievalSettings::from_config(&config.paid_retrieval, mints, data_dir);
    settings.unwrap().unwrap().client.unwrap()
}

#[test]
fn clear_block_writes_off_the_loss_and_pays_the_provider_again() {
    let dir = tempfile::tempdir().unwrap();
    let config = buying_config("http://127.0.0.1:1");
    let store = account_store(&config, dir.path());
    let mut account = AccountState::new();
    account.credited_msat = 5_000;
    account.charged_msat = 1_000;
    account.balance_msat = 1_000;
    account.blocked = Some("provider balance is 3000 msat lower".into());
    store.save(&account).unwrap();

    let cleared = clear_block(&config, dir.path(), "gateway").unwrap();

    assert_eq!(cleared.written_off_msat, 3_000);
    assert_eq!(cleared.reason, "provider balance is 3000 msat lower");
    let saved = store.load().unwrap().unwrap();
    assert_eq!(saved.blocked, None);
    assert_eq!(saved.charged_msat, 4_000);
    assert_eq!(saved.loss_msat(), 0);
    assert_eq!(saved.account, account.account);

    let again = clear_block(&config, dir.path(), "gateway").unwrap_err();
    assert!(
        again.to_string().contains("gateway is not blocked"),
        "{again}"
    );
    let unknown = clear_block(&config, dir.path(), "elsewhere").unwrap_err();
    assert!(unknown.to_string().contains("no provider named elsewhere"));
}

#[tokio::test]
async fn the_commands_refuse_while_the_daemon_holds_the_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let config = buying_config("http://127.0.0.1:1");
    let store = Arc::new(HashtreeStore::new(dir.path().join("store")).unwrap());
    let mint = Arc::new(FakeMint::default());
    let settings = PaidRetrievalSettings::from_config(
        &config.paid_retrieval,
        &config.cashu.accepted_mints,
        dir.path(),
    );
    let running = start_paid_retrieval(
        settings.unwrap().unwrap(),
        "127.0.0.1:8080",
        None,
        &store,
        FakeWallet::funded(&mint, 0),
        FakeReceiver::new(&mint),
    )
    .await
    .unwrap();

    for refused in [
        ensure_accounts_free(&config, dir.path()).unwrap_err(),
        clear_block(&config, dir.path(), "gateway").unwrap_err(),
        settle(&config, dir.path(), None, FakeWallet::funded(&mint, 0))
            .await
            .err()
            .unwrap(),
    ] {
        let refused = format!("{refused:#}");
        assert!(
            refused.contains("in use by another htree process"),
            "{refused}"
        );
        assert!(refused.contains("stop it first"), "{refused}");
    }

    running.shutdown().await;
    drop(running);
    ensure_accounts_free(&config, dir.path()).unwrap();
}

#[tokio::test]
async fn settle_resends_an_unacknowledged_payment_to_a_gateway() {
    let mint = Arc::new(FakeMint::default());
    // A gateway that sells over HTTP only, without FIPS.
    let gateway_dir = tempfile::tempdir().unwrap();
    let gateway_store = Arc::new(HashtreeStore::new(gateway_dir.path().join("store")).unwrap());
    let selling_config: Config = toml::from_str(&format!(
        "[paid_retrieval.provider]\nmints = [\"{MINT}\"]\nhttp = true"
    ))
    .unwrap();
    let selling_settings =
        PaidRetrievalSettings::from_config(&selling_config.paid_retrieval, &[], gateway_dir.path());
    let receiver = FakeReceiver::new(&mint);
    let selling = start_paid_retrieval(
        selling_settings.unwrap().unwrap(),
        "127.0.0.1:8080",
        None,
        &gateway_store,
        FakeWallet::funded(&mint, 0),
        receiver.clone(),
    )
    .await
    .unwrap();
    let (gateway, server) = serve_http(gateway_store, Some(selling.clone())).await;

    // The client handed over a token, and its daemon stopped before the
    // gateway acknowledged it.
    let dir = tempfile::tempdir().unwrap();
    let config = buying_config(&gateway);
    let store = account_store(&config, dir.path());
    let mut account = AccountState::new();
    account.pending = Some(PendingPayment {
        id: [1; 16],
        payment: WalletPayment {
            mint_url: MINT.into(),
            token: mint.issue(MINT, 5),
            amount_sat: 5,
            fee_sat: 0,
            reclaim_id: None,
        },
        attempts: 0,
    });
    store.save(&account).unwrap();
    let before = offline_status(&config, dir.path()).unwrap();
    assert_eq!(before["client"]["providers"][0]["payment_pending"], true);

    let wallet = FakeWallet::funded(&mint, 100);
    let settled = settle(&config, dir.path(), None, wallet.clone())
        .await
        .unwrap();

    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].0, "gateway");
    assert!(settled[0].1.is_ok(), "{:?}", settled[0].1);
    assert_eq!(receiver.received(), 5);
    assert_eq!(wallet.payments(), 0, "the same token, not a new one");
    let saved = store.load().unwrap().unwrap();
    assert_eq!(saved.pending, None);
    assert_eq!(saved.credited_msat, 5_000);
    assert_eq!(saved.balance_msat, 5_000);

    // While the daemon is not running, the status comes from the files.
    let status = offline_status(&config, dir.path()).unwrap();
    let provider = &status["client"]["providers"][0];
    assert_eq!(provider["balance_msat"], 5_000);
    assert_eq!(provider["payment_pending"], false);
    assert_eq!(status["client"]["spending_limit_sat"], 1_000);
    let lines = format_paid_status(&status).join("\n");
    assert!(lines.contains("gateway: balance 5.000 sat\n"), "{lines}");
    assert!(lines.contains("Unresolved tokens: 0 (0 sat)"), "{lines}");
    assert!(!lines.contains("Declined"), "{lines}");
    assert!(
        !status.to_string().contains("cashuFake"),
        "a token is shown"
    );

    server.abort();
    selling.shutdown().await;
}
