mod audit;
mod config;
mod io;
mod model;
mod probe;
mod traversal;

use audit::{run, RunPaths};
use std::path::PathBuf;

fn usage() -> &'static str {
    "usage: iris_audio_target_pool_audit CONFIG_JSON INVENTORY_TSV LEDGER_JSONL CHECKPOINT_JSON MANIFEST_JSON [--max-batches N]"
}

fn parse_args() -> Result<(RunPaths, Option<usize>), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
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
    Ok((
        RunPaths {
            config: PathBuf::from(&args[0]),
            inventory: PathBuf::from(&args[1]),
            ledger: PathBuf::from(&args[2]),
            checkpoint: PathBuf::from(&args[3]),
            manifest: PathBuf::from(&args[4]),
        },
        max_batches,
    ))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (paths, max_batches) = parse_args()?;
    let outcome = run(&paths, max_batches)?;
    eprintln!(
        "audit_progress={}/{} ledger_rows={} complete={} release_ready={}",
        outcome.next_work_item,
        outcome.total_work_items,
        outcome.ledger_rows,
        outcome.complete,
        outcome.release_ready
    );
    Ok(())
}
