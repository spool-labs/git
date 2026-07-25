//! Making the remote's objects available locally
//!
//! Git asks for specific object ids, but packs are the unit of storage here, so we
//! install whichever packs this repository is missing and let git pick refs out of
//! them. Which packs are already installed is remembered on disk, so `git pull`
//! stays incremental instead of replaying the whole history every time.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::git;
use crate::store::Store;

/// File under the git directory recording which packs are already installed
const INSTALLED_PACKS: &str = "tape/installed-packs";

fn installed_path() -> Result<PathBuf> {
    Ok(git::git_dir()?.join(INSTALLED_PACKS))
}

/// Packs this repository has already indexed
///
/// A missing or unreadable file simply means "none known", which costs one
/// redundant fetch rather than failing an otherwise fine clone.
pub fn load_installed() -> BTreeSet<String> {
    let Ok(path) = installed_path() else {
        return BTreeSet::new();
    };
    let Ok(body) = std::fs::read_to_string(path) else {
        return BTreeSet::new();
    };

    let mut installed = BTreeSet::new();
    for line in body.lines() {
        if !line.is_empty() {
            installed.insert(line.to_string());
        }
    }

    installed
}

pub fn save_installed(installed: &BTreeSet<String>) -> Result<()> {
    let path = installed_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create the tape state directory")?;
    }

    let mut body = String::new();
    for key in installed {
        body.push_str(key);
        body.push('\n');
    }

    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))
}

/// Handle git's `fetch` batch
pub async fn fetch(store: &Store) -> Result<()> {
    let Some((index, _)) = store.read_index().await? else {
        return Ok(());
    };

    let mut installed = load_installed();
    let mut fetched = 0u64;

    for entry in &index.packs {
        let key = store.installed_key(entry.track);
        if installed.contains(&key) {
            continue;
        }

        eprintln!("tape: fetching pack {} ({} bytes)", entry.track, entry.size);
        let pack = store.read_pack(entry).await?;
        if git::pack_object_count(&pack) > 0 {
            git::index_pack(&pack)?;
        }

        installed.insert(key);
        fetched += 1;
    }

    if fetched > 0 {
        save_installed(&installed)?;
    }

    Ok(())
}
