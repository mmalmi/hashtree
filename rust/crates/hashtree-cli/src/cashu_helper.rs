pub use cashu_service::{
    base_helper_args, helper_binary_name, helper_binary_path, run_helper_status, CashuHelperClient,
    CashuMintBalance, CashuPaymentClient, CashuReceivedPayment, CashuSentPayment, CARGO_HELPER_ENV,
    CASHU_HELPER_ENV,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_env_lock;
    use serde_json::json;
    use std::env;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[test]
    fn test_helper_binary_path_prefers_env_override() {
        let _guard = test_env_lock().blocking_lock();
        let temp_dir = tempfile::tempdir().unwrap();
        let override_path = temp_dir.path().join("custom-helper");
        std::fs::write(&override_path, b"").unwrap();

        env::set_var(CASHU_HELPER_ENV, &override_path);
        env::remove_var(CARGO_HELPER_ENV);

        let resolved = helper_binary_path(Path::new("/tmp/htree")).unwrap();
        assert_eq!(resolved, override_path);

        env::remove_var(CASHU_HELPER_ENV);
    }

    #[test]
    fn test_helper_binary_path_falls_back_to_sibling_binary() {
        let _guard = test_env_lock().blocking_lock();
        env::remove_var(CASHU_HELPER_ENV);
        env::remove_var(CARGO_HELPER_ENV);

        let temp_dir = tempfile::tempdir().unwrap();
        let current_exe = temp_dir.path().join("htree");
        std::fs::write(&current_exe, b"").unwrap();
        let sibling = temp_dir.path().join(helper_binary_name());
        std::fs::write(&sibling, b"").unwrap();

        let resolved = helper_binary_path(&current_exe).unwrap();
        assert_eq!(resolved, sibling);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_cashu_helper_client_send_and_receive_json() {
        let _guard = test_env_lock().lock().await;
        env::remove_var(CARGO_HELPER_ENV);

        let temp_dir = tempfile::tempdir().unwrap();
        let helper_path = temp_dir.path().join("htree-cashu-stub");
        let script = format!(
            "#!/bin/sh\nif [ \"$3\" = \"internal\" ] && [ \"$4\" = \"send\" ]; then\n  printf '%s' '{}'\nelif [ \"$3\" = \"internal\" ] && [ \"$4\" = \"receive\" ]; then\n  cat >/dev/null\n  printf '%s' '{}'\nelse\n  printf '%s' '{}'\nfi\n",
            json!({
                "mint_url": "https://mint.example",
                "unit": "sat",
                "amount_sat": 3,
                "send_fee_sat": 1,
                "operation_id": "op-123",
                "token": "cashuBtoken"
            }),
            json!({
                "mint_url": "https://mint.example",
                "unit": "sat",
                "amount_sat": 3
            }),
            json!({"ok": true}),
        );
        std::fs::write(&helper_path, script).unwrap();
        let mut perms = std::fs::metadata(&helper_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&helper_path, perms).unwrap();

        env::set_var(CASHU_HELPER_ENV, &helper_path);
        let client = CashuHelperClient::discover(temp_dir.path()).unwrap();

        let sent = client
            .send_payment("https://mint.example", 3)
            .await
            .unwrap();
        assert_eq!(sent.amount_sat, 3);
        assert_eq!(sent.send_fee_sat, 1);
        assert_eq!(sent.operation_id, "op-123");

        let received = client.receive_payment("cashuBtoken").await.unwrap();
        assert_eq!(received.amount_sat, 3);
        assert_eq!(received.mint_url, "https://mint.example");

        client
            .revoke_payment("https://mint.example", "op-123")
            .await
            .unwrap();

        env::remove_var(CASHU_HELPER_ENV);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_cashu_helper_client_queries_mint_balance_json() {
        let _guard = test_env_lock().lock().await;
        env::remove_var(CARGO_HELPER_ENV);

        let temp_dir = tempfile::tempdir().unwrap();
        let helper_path = temp_dir.path().join("htree-cashu-stub");
        let script = format!(
            "#!/bin/sh\nif [ \"$3\" = \"internal\" ] && [ \"$4\" = \"balance\" ]; then\n  printf '%s' '{}'\nelse\n  printf '%s' '{}'\nfi\n",
            json!({
                "mint_url": "https://mint.example",
                "unit": "sat",
                "balance_sat": 21
            }),
            json!({"ok": true}),
        );
        std::fs::write(&helper_path, script).unwrap();
        let mut perms = std::fs::metadata(&helper_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&helper_path, perms).unwrap();

        env::set_var(CASHU_HELPER_ENV, &helper_path);
        let client = CashuHelperClient::discover(temp_dir.path()).unwrap();
        let balance = client.mint_balance("https://mint.example").await.unwrap();
        assert_eq!(balance.mint_url, "https://mint.example");
        assert_eq!(balance.unit, "sat");
        assert_eq!(balance.balance_sat, 21);

        env::remove_var(CASHU_HELPER_ENV);
    }
}
