//! `git-remote-tape`, a git remote helper backed by Tapedrive
//!
//! Git speaks a small line protocol to helpers over stdin/stdout. We implement the
//! `fetch`/`push` dialect, which moves opaque packfiles and a ref map and never
//! interprets git data. That is what makes branches, commit messages, authors,
//! merge topology, tags and signatures round-trip byte-identically. We are not
//! reconstructing any of it, only carrying bytes.
//!
//! Layout on the tape:
//!   * one **unnamed** content-addressed track per pushed packfile
//!   * one **named** object, rewritten per push, holding refs and the pack list
//!
//! Every read is proven against its on-chain commitment, so a clone does not have
//! to trust whoever served it.
//!
//! Progress messages go to stderr with `eprintln!` rather than through `tracing`.
//! They are the interface git shows the user, not diagnostics, and stdout is
//! reserved for protocol, where a stray line corrupts the stream.

mod fetch;
mod git;
mod index;
mod push;
mod store;

use std::io::{BufRead, Lines, Write};

use anyhow::{anyhow, bail, Context, Result};

use crate::store::Store;

/// Url scheme this helper is registered for
const URL_SCHEME: &str = "tape://";

// The worker count has to be a literal, because an attribute macro cannot read a
// const. Four is plenty for work that is almost entirely network wait.
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("git-remote-tape: {error:#}");
        std::process::exit(1);
    }
}

/// Extract the bucket address from the url git invoked us with
///
/// Helpers are called as `git-remote-<transport> <remote> <url>`. When the remote
/// is an inline url there is no name, so the url is the only argument.
fn bucket_from_args() -> Result<String> {
    let url = std::env::args()
        .nth(2)
        .or_else(|| std::env::args().nth(1))
        .ok_or_else(|| anyhow!("usage: git-remote-tape <remote> {URL_SCHEME}<bucket>"))?;

    let bucket = url
        .strip_prefix(URL_SCHEME)
        .ok_or_else(|| anyhow!("`{url}` is not a {URL_SCHEME} url"))?
        .trim_end_matches('/');

    Ok(bucket.to_string())
}

async fn run() -> Result<()> {
    let store = Store::open(&bucket_from_args()?)?;

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut out = std::io::stdout();

    while let Some(line) = lines.next() {
        let line = line.context("read command")?;
        let command = line.trim_end();

        // A bare blank line at top level ends the conversation.
        if command.is_empty() {
            break;
        }

        if command == "capabilities" {
            capabilities(&mut out)?;
        } else if command == "list" || command == "list for-push" {
            list(&store, &mut out).await?;
        } else if command.starts_with("option ") {
            // Options are advisory, but every one still needs an answer, because
            // git blocks waiting for it.
            writeln!(out, "unsupported")?;
            out.flush()?;
        } else if command.starts_with("fetch ") {
            // Batched: consume the rest of the block, then act once. Packs are the
            // unit of transfer, so per-object-id work would be wasted.
            drain_batch(&mut lines)?;
            fetch::fetch(&store).await?;
            writeln!(out)?;
            out.flush()?;
        } else if let Some(first) = command.strip_prefix("push ") {
            let specs = collect_batch(&mut lines, first, "push ")?;
            push::push(&store, &specs, &mut out).await?;
        } else {
            bail!("unsupported command from git: `{command}`");
        }
    }

    Ok(())
}

fn capabilities(out: &mut impl Write) -> Result<()> {
    writeln!(out, "fetch")?;
    writeln!(out, "push")?;
    writeln!(out, "option")?;
    writeln!(out)?;
    out.flush()?;

    Ok(())
}

/// Consume the remainder of a batched command block
fn drain_batch(lines: &mut Lines<impl BufRead>) -> Result<()> {
    for line in lines {
        if line.context("read batch")?.trim_end().is_empty() {
            break;
        }
    }

    Ok(())
}

/// Collect a batched command block, stripping the repeated command prefix
fn collect_batch(
    lines: &mut Lines<impl BufRead>,
    first: &str,
    prefix: &str,
) -> Result<Vec<String>> {
    let mut batch = vec![first.to_string()];

    for line in lines {
        let line = line.context("read batch")?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        batch.push(line.strip_prefix(prefix).unwrap_or(line).to_string());
    }

    Ok(batch)
}

/// Advertise the remote's refs, plus HEAD as a symref
///
/// Without the symref `git clone` lands with nothing in the working tree, which
/// looks like total failure rather than one missing line.
async fn list(store: &Store, out: &mut impl Write) -> Result<()> {
    if let Some((index, _)) = store.read_index().await? {
        for (name, object_id) in &index.refs {
            writeln!(out, "{object_id} {name}")?;
        }
        if let Some(head) = index.head.as_deref() {
            if index.refs.contains_key(head) {
                writeln!(out, "@{head} HEAD")?;
            }
        }
    }

    writeln!(out)?;
    out.flush()?;

    Ok(())
}
