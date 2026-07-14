use anyhow::Result;
use nostr::nips::nip19::FromBech32;
use nostr::PublicKey;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::Config;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdentityBootstrapOutcome {
    pub contacts_seeded: bool,
    pub aliases_seeded: bool,
}

pub fn seed_identity_defaults(
    data_dir: &Path,
    config: &Config,
) -> Result<IdentityBootstrapOutcome> {
    let contacts_seeded = seed_bootstrap_contacts(data_dir, &config.nostr.bootstrap_follows)?;
    let aliases_seeded = seed_default_alias()?;
    Ok(IdentityBootstrapOutcome {
        contacts_seeded,
        aliases_seeded,
    })
}

fn seed_bootstrap_contacts(data_dir: &Path, bootstrap_follows: &[String]) -> Result<bool> {
    let contacts_path = data_dir.join("contacts.json");
    if contacts_path.exists() {
        return Ok(false);
    }

    let contacts = bootstrap_follow_hexes(bootstrap_follows);
    if contacts.is_empty() {
        return Ok(false);
    }

    if let Some(parent) = contacts_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&contacts_path, serde_json::to_string_pretty(&contacts)?)?;
    Ok(true)
}

fn bootstrap_follow_hexes(bootstrap_follows: &[String]) -> Vec<String> {
    bootstrap_follows
        .iter()
        .filter_map(|npub| PublicKey::from_bech32(npub).ok())
        .map(|pubkey| pubkey.to_hex())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn seed_default_alias() -> Result<bool> {
    let aliases_path = hashtree_config::get_aliases_path();
    let Some(parent) = aliases_path.parent() else {
        return Ok(false);
    };
    fs::create_dir_all(parent)?;

    let existing = if aliases_path.exists() {
        let content = fs::read_to_string(&aliases_path)?;
        hashtree_config::parse_keys_file(&content)
    } else {
        Vec::new()
    };
    if existing.iter().any(|entry| {
        entry.secret == hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB
            || entry.alias.as_deref() == Some(hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_ALIAS)
    }) {
        return Ok(false);
    }

    let line = format!(
        "{} {}\n",
        hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB,
        hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_ALIAS
    );

    if aliases_path.exists() {
        use std::io::Write;

        let mut file = fs::OpenOptions::new().append(true).open(&aliases_path)?;
        let needs_newline = fs::metadata(&aliases_path)?.len() > 0;
        if needs_newline {
            file.write_all(b"\n")?;
        }
        file.write_all(line.as_bytes())?;
    } else {
        let content = format!(
            "# Public read-only aliases for repos you clone or fetch.\n# Format: npub1... alias\n{line}"
        );
        fs::write(&aliases_path, content)?;
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn seed_identity_defaults_creates_contacts_and_aliases() {
        let _lock = crate::test_support::test_env_lock().blocking_lock();
        let temp = TempDir::new().expect("temp dir");
        let _guard = crate::test_support::EnvVarGuard::set("HTREE_CONFIG_DIR", temp.path());

        let mut config = Config::default();
        config.storage.data_dir = temp.path().join("data").to_string_lossy().to_string();

        let outcome = seed_identity_defaults(Path::new(&config.storage.data_dir), &config)
            .expect("seed identity defaults");
        assert!(outcome.contacts_seeded);
        assert!(outcome.aliases_seeded);

        let contacts: Vec<String> = serde_json::from_str(
            &fs::read_to_string(temp.path().join("data/contacts.json")).expect("read contacts"),
        )
        .expect("parse contacts");
        assert_eq!(contacts.len(), 1);
        assert_eq!(
            contacts[0],
            PublicKey::from_bech32(hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB)
                .expect("parse default npub")
                .to_hex()
        );

        let aliases = fs::read_to_string(temp.path().join("aliases")).expect("read aliases");
        assert!(aliases.contains(&format!(
            "{} {}",
            hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB,
            hashtree_config::DEFAULT_SOCIALGRAPH_ENTRYPOINT_ALIAS
        )));
        assert!(!aliases
            .contains("# npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm sirius"));
    }

    #[test]
    fn seed_identity_defaults_respects_existing_contacts_and_opt_out() {
        let _lock = crate::test_support::test_env_lock().blocking_lock();
        let temp = TempDir::new().expect("temp dir");
        let _guard = crate::test_support::EnvVarGuard::set("HTREE_CONFIG_DIR", temp.path());

        let data_dir = temp.path().join("data");
        fs::create_dir_all(&data_dir).expect("create data dir");
        let existing_hex = "11".repeat(32);
        fs::write(
            data_dir.join("contacts.json"),
            serde_json::to_string_pretty(&vec![existing_hex.clone()]).expect("encode contacts"),
        )
        .expect("write contacts");

        let mut config = Config::default();
        config.storage.data_dir = data_dir.to_string_lossy().to_string();
        config.nostr.bootstrap_follows.clear();

        let outcome =
            seed_identity_defaults(Path::new(&config.storage.data_dir), &config).expect("seed");
        assert!(!outcome.contacts_seeded);
        assert!(outcome.aliases_seeded);

        let contacts: Vec<String> = serde_json::from_str(
            &fs::read_to_string(data_dir.join("contacts.json")).expect("read contacts"),
        )
        .expect("parse contacts");
        assert_eq!(contacts, vec![existing_hex]);
    }
}
