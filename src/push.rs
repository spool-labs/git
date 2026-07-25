//! Applying a batch of refspecs: one pack for what is new, one rewritten index
//!
//! Tapedrive resolves a name to its newest version but cannot reject a write
//! based on the previous one, so a plain read-modify-write silently loses a
//! concurrent pusher's refs. Instead we write, then check whether the visible
//! index actually reflects our changes, and re-merge against the newer head if it
//! does not. Every superseded version stays readable, so nothing is destroyed
//! while this converges.

use std::io::Write;

use anyhow::{bail, Result};

use tape_core::types::TrackNumber;

use crate::fetch::{load_installed, save_installed};
use crate::git;
use crate::index::{Index, PackEntry};
use crate::store::Store;

/// How many times to re-read, re-merge and re-write the index before giving up
const PUSH_ATTEMPTS: u64 = 5;

/// Branches to fall back on when choosing a remote HEAD
const DEFAULT_HEADS: [&str; 2] = ["refs/heads/main", "refs/heads/master"];

/// One refspec as git sends it: `[+]<src>:<dst>`, or `:<dst>` to delete
pub struct Spec {
    pub is_forced: bool,
    pub source: String,
    pub destination: String,

    /// Set for a refspec we could not parse, reported verbatim and never acted on
    pub malformed: Option<String>,
}

/// What a push should do, evaluated against one particular base index
#[derive(Default)]
pub struct Decision {
    /// One line per refspec, in order, for git
    pub results: Vec<String>,

    /// Refs to write
    pub updates: Vec<(String, String)>,

    /// Refs to drop
    pub deletions: Vec<String>,

    /// New tips whose objects have to be packed
    pub include: Vec<String>,
}

/// Split the refspecs git sent us
pub fn parse_specs(specs: &[String]) -> Vec<Spec> {
    let mut parsed = Vec::with_capacity(specs.len());

    for spec in specs {
        let is_forced = spec.starts_with('+');
        let body = spec.strip_prefix('+').unwrap_or(spec);

        match body.split_once(':') {
            Some((source, destination)) => parsed.push(Spec {
                is_forced,
                source: source.to_string(),
                destination: destination.to_string(),
                malformed: None,
            }),
            None => parsed.push(Spec {
                is_forced: false,
                source: String::new(),
                destination: String::new(),
                malformed: Some(body.to_string()),
            }),
        }
    }

    parsed
}

/// Decide one refspec against `index`
fn decide_spec(spec: &Spec, index: &Index, decision: &mut Decision) {
    if let Some(bad) = &spec.malformed {
        decision.results.push(format!("error {bad} malformed refspec"));
        return;
    }
    let destination = &spec.destination;

    if spec.source.is_empty() {
        decision.deletions.push(destination.clone());
        decision.results.push(format!("ok {destination}"));
        return;
    }

    let Some(new_id) = git::rev_parse(&spec.source) else {
        decision.results.push(format!(
            "error {destination} no such ref locally: {}",
            spec.source
        ));
        return;
    };

    if let Some(old_id) = index.refs.get(destination) {
        if old_id == &new_id {
            decision.results.push(format!("ok {destination}"));
            return;
        }
        // Someone advanced this ref past us while we were working. Our objects are
        // already stored and the remote tip contains our commit, so the push
        // succeeded, and moving the ref backwards would be wrong.
        if git::has_object(old_id) && git::is_ancestor(&new_id, old_id) {
            decision.results.push(format!("ok {destination}"));
            return;
        }
        if !spec.is_forced {
            if !git::has_object(old_id) {
                decision.results.push(format!(
                    "error {destination} remote is at {old_id}, which is not in \
                     this repository; fetch first"
                ));
                return;
            }
            if !git::is_ancestor(old_id, &new_id) {
                decision
                    .results
                    .push(format!("error {destination} non-fast-forward"));
                return;
            }
        }
    }

    decision.updates.push((destination.clone(), new_id.clone()));
    decision.include.push(new_id);
    decision.results.push(format!("ok {destination}"));
}

/// Decide every refspec against `index`
///
/// Deliberately a pure function of (refspecs, base index): re-running it against a
/// newer base *is* the conflict merge, because every check gets re-evaluated
/// against whatever the other pusher left behind: fast-forward, already-there,
/// superseded.
pub fn decide(specs: &[Spec], index: &Index) -> Decision {
    let mut decision = Decision::default();

    for spec in specs {
        decide_spec(spec, index, &mut decision);
    }

    decision
}

/// Whether the visible index already reflects everything this decision wanted
///
/// Checking the observable outcome rather than "was my write the newest" keeps the
/// loop correct no matter who else wrote in the meantime: if a later pusher merged
/// our changes in, we are done and do not need to write again.
pub fn is_satisfied(index: &Index, decision: &Decision, pack: Option<&PackEntry>) -> bool {
    if let Some(entry) = pack {
        if !index.has_pack(entry.track) {
            return false;
        }
    }

    for (name, object_id) in &decision.updates {
        if index.refs.get(name) != Some(object_id) {
            return false;
        }
    }
    for name in &decision.deletions {
        if index.refs.contains_key(name) {
            return false;
        }
    }

    true
}

/// Pick a remote HEAD when it is unset or dangling
///
/// A shared remote's default branch must not flip to whatever branch the most
/// recent pusher happened to be standing on, because that would change what
/// everyone else's next clone checks out. The first push seeds it from the local
/// HEAD. After that it is sticky, and moving it is a deliberate act.
fn choose_head(index: &Index) -> Option<String> {
    if let Ok(local) = git::git(&["symbolic-ref", "HEAD"]) {
        if index.refs.contains_key(&local) {
            return Some(local);
        }
    }

    for name in DEFAULT_HEADS {
        if index.refs.contains_key(name) {
            return Some(name.to_string());
        }
    }

    for name in index.refs.keys() {
        if name.starts_with("refs/heads/") {
            return Some(name.clone());
        }
    }

    None
}

/// Fold a decision into an index
pub fn apply(index: &mut Index, decision: &Decision, pack: Option<&PackEntry>) {
    for name in &decision.deletions {
        index.refs.remove(name);
    }
    for (name, object_id) in &decision.updates {
        index.refs.insert(name.clone(), object_id.clone());
    }
    if let Some(entry) = pack {
        if !index.has_pack(entry.track) {
            index.packs.push(entry.clone());
        }
    }

    let is_head_live = match index.head.as_deref() {
        Some(head) => index.refs.contains_key(head),
        None => false,
    };
    if !is_head_live {
        index.head = choose_head(index);
    }
}

/// Store the objects a push needs, if there are any
///
/// Written once, outside the retry loop: pack contents are content-addressed and a
/// retry only changes ref bookkeeping, never which objects have to exist.
async fn store_pack(
    store: &Store,
    base: &Index,
    decision: &Decision,
) -> Result<Option<PackEntry>> {
    if decision.include.is_empty() {
        return Ok(None);
    }

    // Only object ids we actually hold can be excluded. A tip pushed by someone
    // else that we never fetched would otherwise abort `pack-objects` with "bad
    // object". Dropping it just re-sends objects the remote already has.
    let mut exclude = Vec::new();
    for object_id in base.tips() {
        if git::has_object(&object_id) {
            exclude.push(object_id);
        }
    }

    let pack = git::pack_objects(&decision.include, &exclude)?;
    let objects = git::pack_object_count(&pack);
    if objects == 0 {
        return Ok(None);
    }

    let plural = if objects == 1 { "" } else { "s" };
    eprintln!(
        "tape: writing pack of {objects} object{plural} ({} bytes)",
        pack.len()
    );
    let entry = store.write_pack(&pack).await?;
    eprintln!("tape: pack stored at track {}", entry.track);

    // We built this pack from local objects, so we already have every object in
    // it. Recording it as installed stops the next `git pull` in this repo from
    // downloading our own push back again.
    let mut installed = load_installed();
    installed.insert(store.installed_key(entry.track));
    save_installed(&installed)?;

    Ok(Some(entry))
}

/// Publish the index, re-merging until the visible head reflects our changes
async fn publish(
    store: &Store,
    specs: &[Spec],
    pack: Option<&PackEntry>,
) -> Result<Vec<String>> {
    // Index versions this process wrote. Deciding against our own version would
    // re-derive our own state and miss a pusher we shadowed, so a retry always
    // bases on the newest version somebody *else* wrote.
    let mut ours: Vec<TrackNumber> = Vec::new();
    let mut results = Vec::new();

    for attempt in 1..=PUSH_ATTEMPTS {
        let versions = store.index_versions().await?;
        let head_version = versions.last().copied();
        let mut base_version = None;
        for version in versions.iter().rev() {
            if !ours.contains(version) {
                base_version = Some(*version);
                break;
            }
        }

        let mut index = match base_version {
            Some(track) => store.read_index_at(track).await?,
            None => Index::default(),
        };

        // Deciding against their version is the merge: their refs and packs are
        // already in `index`, so our fast-forward checks run against reality and
        // `apply` layers our updates on top rather than replacing them.
        let decision = decide(specs, &index);
        results = decision.results.clone();

        let visible = match head_version {
            Some(track) if Some(track) == base_version => index.clone(),
            Some(track) => store.read_index_at(track).await?,
            None => Index::default(),
        };
        if is_satisfied(&visible, &decision, pack) {
            break;
        }

        // Never drop packs belonging to a version we are about to supersede. They
        // are immutable and someone's objects depend on them.
        index.absorb_packs(&visible);

        apply(&mut index, &decision, pack);
        index.parent = base_version.map(|track| track.0);
        let written = store.write_index(&index).await?;
        ours.push(written);

        // Confirm using track metadata only. Reading our own index back would mean
        // fetching slices of a track written seconds ago. The version list tells us
        // what we need without touching content. We won without a race when nothing
        // landed after us and the version directly below ours is exactly the base we
        // merged against.
        let after = store.index_versions().await?;
        let mut below = None;
        for version in after.iter().rev() {
            if version.0 < written.0 {
                below = Some(*version);
                break;
            }
        }
        if after.last() == Some(&written) && below == base_version {
            break;
        }

        if attempt == PUSH_ATTEMPTS {
            bail!(
                "ref index kept being overwritten by a concurrent push after \
                 {PUSH_ATTEMPTS} attempts. Nothing was lost, every object and every \
                 index version is still stored, but the push needs retrying"
            );
        }
        eprintln!("tape: concurrent push detected, merging and retrying");
    }

    Ok(results)
}

/// Handle git's `push` batch
pub async fn push(store: &Store, specs: &[String], out: &mut impl Write) -> Result<()> {
    // Fail before doing any work. Packing a repository and then discovering we
    // have no key to write with wastes the user's time and reads like a crash.
    store.writable()?;

    let parsed = parse_specs(specs);
    let base = match store.read_index().await? {
        Some((index, _)) => index,
        None => Index::default(),
    };

    let pack = store_pack(store, &base, &decide(&parsed, &base)).await?;
    let results = publish(store, &parsed, pack.as_ref()).await?;

    for line in results {
        writeln!(out, "{line}")?;
    }
    writeln!(out)?;
    out.flush()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with(name: &str, object_id: &str) -> Index {
        let mut index = Index::default();
        index.refs.insert(name.to_string(), object_id.to_string());
        index
    }

    // a plain refspec splits into source and destination
    #[test]
    fn plain_refspec() {
        let parsed = parse_specs(&["refs/heads/main:refs/heads/main".to_string()]);

        assert!(!parsed[0].is_forced);
        assert_eq!(parsed[0].source, "refs/heads/main");
        assert_eq!(parsed[0].destination, "refs/heads/main");
    }

    // a leading plus marks a force push
    #[test]
    fn forced_refspec() {
        let parsed = parse_specs(&["+refs/heads/main:refs/heads/main".to_string()]);

        assert!(parsed[0].is_forced);
        assert_eq!(parsed[0].source, "refs/heads/main");
    }

    // an empty source is a deletion
    #[test]
    fn delete_refspec() {
        let parsed = parse_specs(&[":refs/heads/gone".to_string()]);
        let decision = decide(&parsed, &index_with("refs/heads/gone", &"a".repeat(40)));

        assert_eq!(decision.deletions, vec!["refs/heads/gone".to_string()]);
        assert_eq!(decision.results, vec!["ok refs/heads/gone".to_string()]);
    }

    // a refspec with no colon is reported, not acted on
    #[test]
    fn malformed_refspec() {
        let parsed = parse_specs(&["nonsense".to_string()]);
        let decision = decide(&parsed, &Index::default());

        assert!(decision.updates.is_empty());
        assert!(decision.results[0].starts_with("error nonsense"));
    }

    // a ref that does not exist locally is rejected
    #[test]
    fn missing_source() {
        let parsed = parse_specs(&["refs/heads/nope-not-here:refs/heads/x".to_string()]);
        let decision = decide(&parsed, &Index::default());

        assert!(decision.updates.is_empty());
        assert!(decision.results[0].contains("no such ref locally"));
    }

    // a decision is satisfied only once the index carries its updates
    #[test]
    fn satisfied_updates() {
        let object_id = "b".repeat(40);
        let mut decision = Decision::default();
        decision
            .updates
            .push(("refs/heads/main".to_string(), object_id.clone()));

        assert!(!is_satisfied(&Index::default(), &decision, None));
        assert!(is_satisfied(
            &index_with("refs/heads/main", &object_id),
            &decision,
            None
        ));
    }

    // a decision is unsatisfied while a deleted ref is still present
    #[test]
    fn satisfied_deletions() {
        let mut decision = Decision::default();
        decision.deletions.push("refs/heads/gone".to_string());

        assert!(!is_satisfied(
            &index_with("refs/heads/gone", &"c".repeat(40)),
            &decision,
            None
        ));
        assert!(is_satisfied(&Index::default(), &decision, None));
    }

    // an established head is never moved by a later push
    #[test]
    fn head_is_sticky() {
        let mut index = index_with("refs/heads/release", &"d".repeat(40));
        index.refs.insert("refs/heads/main".to_string(), "e".repeat(40));
        index.head = Some("refs/heads/release".to_string());


        apply(&mut index, &Decision::default(), None);

        assert_eq!(index.head.as_deref(), Some("refs/heads/release"));
    }

    // a dangling head falls back to a branch that exists
    #[test]
    fn head_recovers() {
        let mut index = index_with("refs/heads/main", &"f".repeat(40));
        index.head = Some("refs/heads/deleted".to_string());

        apply(&mut index, &Decision::default(), None);

        assert_eq!(index.head.as_deref(), Some("refs/heads/main"));
    }
}
