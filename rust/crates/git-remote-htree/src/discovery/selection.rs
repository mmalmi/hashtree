use super::SearchResults;
use anyhow::{bail, ensure, Context, Result};
use std::io::{BufRead, IsTerminal, Write};

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn suggestions(results: &SearchResults, command: &str) -> String {
    let mut text = String::new();
    if results.candidates.is_empty() {
        text.push_str("No matches observed in your social graph during this search. Discovery may be incomplete; try htree search with a longer --wait or use an explicit npub/tree URL.\n");
    } else {
        text.push_str("Matches observed in your social graph (profile names are not unique):\n");
        for (i, candidate) in results.candidates.iter().enumerate() {
            text.push_str(&format!(
                "  {}. {} / {} ({} follow hops)\n     {} {}\n",
                i + 1,
                candidate.publisher,
                candidate.name,
                candidate.distance,
                command,
                quoted(&candidate.url)
            ));
        }
        text.push_str("Discovery may be incomplete. Use an explicit URL to select a publisher.\n");
    }
    if results.truncated {
        text.push_str(
            "Showing a bounded selection; narrow your search for more specific results.\n",
        );
    }
    text
}

/// Prompts belong to CLI commands, never the Git helper's protocol streams.
pub fn choose(results: &SearchResults, command: &str, non_interactive: bool) -> Result<String> {
    eprint!("{}", suggestions(results, command));
    ensure!(!results.candidates.is_empty(), "No publisher selected");
    ensure!(
        !non_interactive
            && std::io::stdin().is_terminal()
            && std::io::stderr().is_terminal()
            && std::env::var("GIT_TERMINAL_PROMPT").as_deref() != Ok("0")
            && std::env::var("TERM").as_deref() != Ok("dumb"),
        "A name search needs a publisher choice; retry with one of the explicit URLs above"
    );
    eprint!("Choose 1-{} (Enter to cancel): ", results.candidates.len());
    std::io::stderr().flush()?;
    read_choice(&mut std::io::stdin().lock(), results)
}

pub(super) fn read_choice(reader: &mut impl BufRead, results: &SearchResults) -> Result<String> {
    let mut line = String::new();
    std::io::Read::take(reader, 64).read_line(&mut line)?;
    if line.trim().is_empty() {
        bail!("Cancelled; no publisher selected");
    }
    let index: usize = line.trim().parse().context("Enter a listed number")?;
    results
        .candidates
        .get(index.wrapping_sub(1))
        .map(|c| c.url.clone())
        .context("Enter a listed number")
}
