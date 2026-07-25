//! Subprocess wrappers around the local git
//!
//! The helper never parses git's object format itself. It asks git to produce a
//! packfile and asks git to consume one. That is what keeps SHA-1s byte-identical
//! across a push/fetch round trip, and with them commit messages, authors,
//! signatures, and merge topology.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

use anyhow::{bail, Context, Result};

/// A packfile opens with `PACK`, a 4-byte version, then a 4-byte big-endian
/// object count.
const PACK_SIGNATURE: [u8; 4] = *b"PACK";
const PACK_HEADER_BYTES: usize = 12;
const PACK_COUNT_OFFSET: usize = 8;

fn capture(args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn git {}", args.join(" ")))
}

/// Run git and require success, returning trimmed stdout
pub fn git(args: &[&str]) -> Result<String> {
    let output = capture(args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The repository git invoked us for
pub fn git_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(git(&["rev-parse", "--absolute-git-dir"])?))
}

/// Resolve a revision to a full object id, or `None` when it does not exist
pub fn rev_parse(revision: &str) -> Option<String> {
    let output = capture(&["rev-parse", "--verify", "--quiet", revision]).ok()?;
    if !output.status.success() {
        return None;
    }
    let object_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!object_id.is_empty()).then_some(object_id)
}

/// Whether the object is present locally
///
/// Fast-forward checks are meaningless against an object we do not have, so
/// callers use this to decide when to demand a force push instead of guessing.
pub fn has_object(object_id: &str) -> bool {
    capture(&["cat-file", "-e", &format!("{object_id}^{{object}}")])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Whether `old` is reachable from `new`, meaning the update fast-forwards
pub fn is_ancestor(old: &str, new: &str) -> bool {
    capture(&["merge-base", "--is-ancestor", old, new])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Number of objects a packfile carries, read from its header
///
/// An "empty" pack is still 32 bytes of header and trailer, so size alone cannot
/// tell us whether a push has anything new to store.
pub fn pack_object_count(pack: &[u8]) -> u64 {
    if pack.len() < PACK_HEADER_BYTES || pack[..PACK_SIGNATURE.len()] != PACK_SIGNATURE {
        return 0;
    }

    let mut count = [0u8; 4];
    count.copy_from_slice(&pack[PACK_COUNT_OFFSET..PACK_HEADER_BYTES]);
    u64::from(u32::from_be_bytes(count))
}

/// Feed `input` to a child's stdin from another thread
///
/// The child's output can outgrow the pipe buffer, so writing inline would
/// deadlock us against our own unread stdout.
fn write_stdin_detached(child: &mut Child, input: Vec<u8>, what: &str) -> Result<()> {
    let mut stdin = child
        .stdin
        .take()
        .with_context(|| format!("{what} stdin was not piped"))?;

    std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });

    Ok(())
}

/// Build a packfile holding everything reachable from `include` that is not
/// already reachable from `exclude`
///
/// Deliberately not `--thin`. Every pack must resolve its own deltas so a clone
/// can replay packs in push order without later ones depending on objects an
/// earlier read might have missed.
pub fn pack_objects(include: &[String], exclude: &[String]) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .args([
            "pack-objects",
            "--stdout",
            "--revs",
            "--delta-base-offset",
            "--quiet",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn git pack-objects")?;

    let mut revisions = String::new();
    for object_id in include {
        revisions.push_str(object_id);
        revisions.push('\n');
    }
    for object_id in exclude {
        revisions.push('^');
        revisions.push_str(object_id);
        revisions.push('\n');
    }

    write_stdin_detached(&mut child, revisions.into_bytes(), "git pack-objects")?;

    let output = child.wait_with_output().context("git pack-objects")?;
    if !output.status.success() {
        bail!(
            "git pack-objects failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(output.stdout)
}

/// Install a packfile into the repository, making its objects available to git
pub fn index_pack(pack: &[u8]) -> Result<()> {
    let mut child = Command::new("git")
        .args(["index-pack", "--stdin", "--fix-thin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn git index-pack")?;

    write_stdin_detached(&mut child, pack.to_vec(), "git index-pack")?;

    let output = child.wait_with_output().context("git index-pack")?;
    if !output.status.success() {
        bail!(
            "git index-pack failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(count: u32) -> Vec<u8> {
        let mut pack = Vec::new();
        pack.extend_from_slice(&PACK_SIGNATURE);
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&count.to_be_bytes());
        pack
    }

    // a well-formed header reports its object count
    #[test]
    fn counts_objects() {
        let pack = header(278);

        assert_eq!(pack_object_count(&pack), 278);
    }

    // an empty pack is still a valid header, and must report zero
    #[test]
    fn empty_pack() {
        let pack = header(0);

        assert_eq!(pack_object_count(&pack), 0);
    }

    // truncated or foreign bytes count as nothing rather than panicking
    #[test]
    fn not_a_pack() {
        assert_eq!(pack_object_count(b"PAC"), 0);
        assert_eq!(pack_object_count(&[]), 0);
        assert_eq!(pack_object_count(b"NOPEnope\0\0\0\x05"), 0);
    }
}
