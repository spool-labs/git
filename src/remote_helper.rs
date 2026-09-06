use std::io::{BufRead, Lines, Write};

use anyhow::{Context, Result, anyhow, bail};

use crate::{git::Repository, store::Store};

const URL_SCHEME: &str = "tape://";

pub async fn run_from_env() -> Result<()> {
    let store = Store::open(&bucket_from_args()?)?;
    let repository = Repository::current()?;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut out = std::io::stdout();

    while let Some(line) = lines.next() {
        let line = line.context("read command")?;
        let command = line.trim_end();
        if command.is_empty() {
            break;
        }
        if command == "capabilities" {
            capabilities(&mut out)?;
        } else if command == "list" || command == "list for-push" {
            list(&store, &mut out).await?;
        } else if command.starts_with("option ") {
            writeln!(out, "unsupported")?;
            out.flush()?;
        } else if command.starts_with("fetch ") {
            drain_batch(&mut lines)?;
            crate::fetch::fetch(&store, &repository).await?;
            writeln!(out)?;
            out.flush()?;
        } else if let Some(first) = command.strip_prefix("push ") {
            let specs = collect_batch(&mut lines, first, "push ")?;
            crate::push::push(&store, &repository, &specs, &mut out).await?;
        } else {
            bail!("unsupported command from git: `{command}`");
        }
    }
    Ok(())
}

fn bucket_from_args() -> Result<String> {
    let url = std::env::args()
        .nth(2)
        .or_else(|| std::env::args().nth(1))
        .ok_or_else(|| anyhow!("usage: git-remote-tape <remote> {URL_SCHEME}<bucket>"))?;
    let bucket = url
        .strip_prefix(URL_SCHEME)
        .ok_or_else(|| anyhow!("`{url}` is not a {URL_SCHEME} url"))?
        .trim_end_matches('/');
    if bucket.is_empty() {
        bail!("tape URL is missing its address");
    }
    Ok(bucket.to_string())
}

fn capabilities(out: &mut impl Write) -> Result<()> {
    writeln!(out, "fetch")?;
    writeln!(out, "push")?;
    writeln!(out, "option")?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn drain_batch(lines: &mut Lines<impl BufRead>) -> Result<()> {
    for line in lines {
        if line.context("read batch")?.trim_end().is_empty() {
            break;
        }
    }
    Ok(())
}

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
