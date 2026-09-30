//! Making the remote's objects available locally
//!
//! Git asks for specific object ids, but packs are the unit of storage here, so we
//! install whichever packs this repository is missing and let git pick refs out of
//! them. Which packs are already installed is remembered on disk, so `git pull`
//! stays incremental instead of replaying the whole history every time.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::git::{self, Repository};
use crate::progress::Progress;
use crate::store::Store;

/// File under the git directory recording which packs are already installed
const INSTALLED_PACKS: &str = "tape/installed-packs";

/// Directory under the git directory holding downloaded packs until they are indexed
const INCOMING_PACKS: &str = "tape/incoming";

/// Packs smaller than this index in a blink, so git's own progress would only add lines
const PACK_PROGRESS_MIN_BYTES: u64 = 1024 * 1024;

fn installed_path(repository: &Repository) -> Result<PathBuf> {
    Ok(repository.git_dir()?.join(INSTALLED_PACKS))
}

/// Packs this repository has already indexed
///
/// A missing or unreadable file simply means "none known", which costs one
/// redundant fetch rather than failing an otherwise fine clone.
pub fn load_installed(repository: &Repository) -> BTreeSet<String> {
    let Ok(path) = installed_path(repository) else {
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

pub fn save_installed(repository: &Repository, installed: &BTreeSet<String>) -> Result<()> {
    let path = installed_path(repository)?;
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
///
/// Every missing pack downloads under one progress line and waits on disk, then
/// each is indexed. With `is_progress` set, git prints its own receiving and
/// resolving lines for any pack big enough to take a moment.
pub async fn fetch(store: &Store, repository: &Repository, is_progress: bool) -> Result<()> {
    let Some((index, _)) = store.read_index().await? else {
        return Ok(());
    };

    let mut installed = load_installed(repository);
    let mut missing = Vec::new();
    let mut total_bytes = 0;
    for entry in &index.packs {
        if !installed.contains(&store.installed_key(entry.track)) {
            total_bytes += entry.size;
            missing.push(entry);
        }
    }
    let count = missing.len();
    if count == 0 {
        return Ok(());
    }

    let incoming = repository.git_dir()?.join(INCOMING_PACKS);
    std::fs::create_dir_all(&incoming).context("create the incoming pack directory")?;

    let mut progress = Progress::start("Downloading packs", count, total_bytes, is_progress);
    let mut downloaded = Vec::with_capacity(count);
    for entry in &missing {
        let pack = store.read_pack(entry, &mut progress).await?;
        let path = incoming.join(format!("{}.pack", entry.track));
        std::fs::write(&path, &pack).with_context(|| format!("write {}", path.display()))?;
        downloaded.push((entry, path));
        progress.complete_item();
    }
    progress.finish();

    for (entry, path) in downloaded {
        let pack = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        if git::pack_object_count(&pack) > 0 {
            let is_pack_progress = is_progress && entry.size >= PACK_PROGRESS_MIN_BYTES;
            repository.index_pack(&pack, is_pack_progress)?;
        }
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        installed.insert(store.installed_key(entry.track));
    }
    std::fs::remove_dir(&incoming).with_context(|| format!("remove {}", incoming.display()))?;

    save_installed(repository, &installed)?;

    Ok(())
}
