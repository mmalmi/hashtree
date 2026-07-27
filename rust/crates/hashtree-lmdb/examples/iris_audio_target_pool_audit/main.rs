mod audit;
mod config;
mod io;
mod model;
mod probe;
mod traversal;
mod witness;

use audit::{run, RunPaths};
use std::io::Write;
use std::path::PathBuf;
use witness::{verify_existing, witness_json_line, WitnessPaths};

#[cfg(test)]
mod witness_tests;

fn usage() -> &'static str {
    "usage: iris_audio_target_pool_audit CONFIG_JSON INVENTORY_TSV LEDGER_JSONL CHECKPOINT_JSON MANIFEST_JSON [--max-batches N]\n       iris_audio_target_pool_audit --verify-existing CONFIG_JSON INVENTORY_TSV LEDGER_JSONL MANIFEST_JSON --challenge 64_LOWERHEX"
}

enum Command {
    Audit {
        paths: RunPaths,
        max_batches: Option<usize>,
    },
    VerifyExisting {
        paths: WitnessPaths,
        challenge: String,
    },
}

fn parse_args_from(
    args: impl IntoIterator<Item = String>,
) -> Result<Command, Box<dyn std::error::Error>> {
    let args = args.into_iter().collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--verify-existing") {
        if args.len() != 7 || args[5] != "--challenge" {
            return Err(usage().into());
        }
        return Ok(Command::VerifyExisting {
            paths: WitnessPaths {
                config: PathBuf::from(&args[1]),
                inventory: PathBuf::from(&args[2]),
                ledger: PathBuf::from(&args[3]),
                manifest: PathBuf::from(&args[4]),
            },
            challenge: args[6].clone(),
        });
    }
    if args.len() != 5 && args.len() != 7 {
        return Err(usage().into());
    }
    let max_batches = if args.len() == 7 {
        if args[5] != "--max-batches" {
            return Err(usage().into());
        }
        let value = args[6].parse::<usize>()?;
        if value == 0 {
            return Err("--max-batches must be greater than zero".into());
        }
        Some(value)
    } else {
        None
    };
    Ok(Command::Audit {
        paths: RunPaths {
            config: PathBuf::from(&args[0]),
            inventory: PathBuf::from(&args[1]),
            ledger: PathBuf::from(&args[2]),
            checkpoint: PathBuf::from(&args[3]),
            manifest: PathBuf::from(&args[4]),
        },
        max_batches,
    })
}

fn parse_args() -> Result<Command, Box<dyn std::error::Error>> {
    parse_args_from(std::env::args().skip(1))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match parse_args()? {
        Command::Audit { paths, max_batches } => {
            let outcome = run(&paths, max_batches)?;
            eprintln!(
                "audit_progress={}/{} ledger_rows={} complete={} release_ready={}",
                outcome.next_work_item,
                outcome.total_work_items,
                outcome.ledger_rows,
                outcome.complete,
                outcome.release_ready
            );
        }
        Command::VerifyExisting { paths, challenge } => {
            let witness = verify_existing(&paths, &challenge)?;
            let line = witness_json_line(&witness)?;
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            output.write_all(&line)?;
            output.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn parses_exact_verify_existing_subcommand_shape() {
        let command = parse_args_from(
            [
                "--verify-existing",
                "config.json",
                "inventory.tsv",
                "ledger.jsonl",
                "manifest.json",
                "--challenge",
                &"ab".repeat(32),
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .expect("verify-existing command");
        let Command::VerifyExisting { paths, challenge } = command else {
            panic!("expected verify-existing command");
        };
        assert_eq!(paths.config, PathBuf::from("config.json"));
        assert_eq!(paths.inventory, PathBuf::from("inventory.tsv"));
        assert_eq!(paths.ledger, PathBuf::from("ledger.jsonl"));
        assert_eq!(paths.manifest, PathBuf::from("manifest.json"));
        assert_eq!(challenge, "ab".repeat(32));
    }

    #[test]
    fn rejects_verify_existing_argument_permutations() {
        for args in [
            vec!["--verify-existing"],
            vec![
                "--verify-existing",
                "config",
                "inventory",
                "ledger",
                "manifest",
                "--not-challenge",
                "00",
            ],
            vec![
                "--verify-existing",
                "config",
                "inventory",
                "ledger",
                "manifest",
                "--challenge",
                "00",
                "extra",
            ],
        ] {
            assert!(parse_args_from(args.into_iter().map(str::to_owned)).is_err());
        }
    }
}
