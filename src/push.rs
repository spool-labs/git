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
use crate::git::{self, Repository};
use crate::index::{Index, PackEntry};
use crate::store::{not_listed, Store};

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
fn decide_spec(repository: &Repository, spec: &Spec, index: &Index, decision: &mut Decision) {
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

    let Some(new_id) = repository.rev_parse(&spec.source) else {
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
        if repository.has_object(old_id) && repository.is_ancestor(&new_id, old_id) {
            decision.results.push(format!("ok {destination}"));
            return;
        }
        if !spec.is_forced {
            if !repository.has_object(old_id) {
                decision.results.push(format!(
                    "error {destination} remote is at {old_id}, which is not in \
                     this repository; fetch first"
                ));
                return;
            }
            if !repository.is_ancestor(old_id, &new_id) {
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
pub fn decide(repository: &Repository, specs: &[Spec], index: &Index) -> Decision {
    let mut decision = Decision::default();

    for spec in specs {
        decide_spec(repository, spec, index, &mut decision);
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

/// What a version listing says about the index version this process just wrote
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Landing {
    /// Ours is the newest version and it sits directly on the base it merged
    Clean,

    /// Somebody else wrote between our base and now
    Conflict,

    /// The listing does not reach our version at all
    NotVisible,
}

/// The newest index version this process did not write
pub fn choose_base(versions: &[TrackNumber], ours: &[TrackNumber]) -> Option<TrackNumber> {
    for version in versions.iter().rev() {
        if !ours.contains(version) {
            return Some(*version);
        }
    }

    None
}

/// Read a version listing as a verdict on the index version we wrote
///
/// A listing that stops below our own write proves nothing about who else wrote,
/// so it is its own answer rather than a race: treating it as one is what made a
/// lagging storage node look like a competing pusher.
pub fn landing(base: Option<TrackNumber>, ours: TrackNumber, versions: &[TrackNumber]) -> Landing {
    if !versions.contains(&ours) {
        return Landing::NotVisible;
    }

    let mut below = None;
    for version in versions.iter().rev() {
        if version.0 < ours.0 {
            below = Some(*version);
            break;
        }
    }

    if versions.last() == Some(&ours) && below == base {
        return Landing::Clean;
    }

    Landing::Conflict
}

/// Pick a remote HEAD when it is unset or dangling
///
/// A shared remote's default branch must not flip to whatever branch the most
/// recent pusher happened to be standing on, because that would change what
/// everyone else's next clone checks out. The first push seeds it from the local
/// HEAD. After that it is sticky, and moving it is a deliberate act.
fn choose_head(repository: &Repository, index: &Index) -> Option<String> {
    if let Ok(local) = repository.git(&["symbolic-ref", "HEAD"]) {
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
pub fn apply(
    repository: &Repository,
    index: &mut Index,
    decision: &Decision,
    pack: Option<&PackEntry>,
) {
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
        index.head = choose_head(repository, index);
    }
}

/// Store the objects a push needs, if there are any
///
/// Written once, outside the retry loop: pack contents are content-addressed and a
/// retry only changes ref bookkeeping, never which objects have to exist.
async fn store_pack(
    store: &Store,
    repository: &Repository,
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
        if repository.has_object(&object_id) {
            exclude.push(object_id);
        }
    }

    let pack = repository.pack_objects(&decision.include, &exclude)?;
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
    let mut installed = load_installed(repository);
    installed.insert(store.installed_key(entry.track));
    save_installed(repository, &installed)?;

    Ok(Some(entry))
}

/// Publish the index, re-merging until the visible head reflects our changes
async fn publish(
    store: &Store,
    repository: &Repository,
    specs: &[Spec],
    pack: Option<&PackEntry>,
) -> Result<Vec<String>> {
    // Index versions this process wrote. Deciding against our own version would
    // re-derive our own state and miss a pusher we shadowed, so a retry always
    // bases on the newest version somebody *else* wrote.
    let mut ours: Vec<TrackNumber> = Vec::new();
    let mut results = Vec::new();

    for attempt in 1..=PUSH_ATTEMPTS {
        // A node that has not ingested our newest write yet answers with a listing
        // that stops below it, and re-basing on that would write the same merge again.
        let versions = match ours.last() {
            Some(track) => store.index_versions_including(*track).await?,
            None => store.index_versions().await?,
        };
        let head_version = versions.last().copied();
        let base_version = choose_base(&versions, &ours);

        let mut index = match base_version {
            Some(track) => store.read_index_at(track).await?,
            None => Index::default(),
        };

        // Deciding against their version is the merge: their refs and packs are
        // already in `index`, so our fast-forward checks run against reality and
        // `apply` layers our updates on top rather than replacing them.
        let decision = decide(repository, specs, &index);
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

        apply(repository, &mut index, &decision, pack);
        index.parent = base_version.map(|track| track.0);
        let written = store.write_index(&index).await?;
        ours.push(written);

        // Confirm using track metadata only. Reading our own index back would mean
        // fetching slices of a track written seconds ago. The version list tells us
        // what we need without touching content. We won without a race when nothing
        // landed after us and the version directly below ours is exactly the base we
        // merged against.
        let after = store.index_versions_including(written).await?;
        match landing(base_version, written, &after) {
            Landing::Clean => break,
            Landing::NotVisible => return Err(not_listed(written)),
            Landing::Conflict => {}
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
pub async fn push(
    store: &Store,
    repository: &Repository,
    specs: &[String],
    out: &mut impl Write,
) -> Result<()> {
    // Fail before doing any work. Packing a repository and then discovering we
    // have no key to write with wastes the user's time and reads like a crash.
    store.writable()?;

    let parsed = parse_specs(specs);
    let base = match store.read_index().await? {
        Some((index, _)) => index,
        None => Index::default(),
    };

    let pack = store_pack(store, repository, &base, &decide(repository, &parsed, &base)).await?;
    let results = publish(store, repository, &parsed, pack.as_ref()).await?;

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

    fn repository() -> Repository {
        Repository::at(env!("CARGO_MANIFEST_DIR"))
    }

    #[derive(Default)]
    struct Run {
        bases: Vec<Option<TrackNumber>>,
        writes: usize,
    }

    fn serve(script: &[&[u64]], served: &mut usize) -> Vec<TrackNumber> {
        let listing = script[(*served).min(script.len() - 1)];
        *served += 1;

        let mut versions = Vec::new();
        for track in listing {
            versions.push(TrackNumber(*track));
        }

        versions
    }

    // The publish loop over scripted listings, one served per call, with no store
    fn drive(
        script: &[&[u64]],
        writes: &[u64],
        satisfying: &[u64],
        run: &mut Run,
    ) -> Result<()> {
        let mut ours: Vec<TrackNumber> = Vec::new();
        let mut served = 0;

        for attempt in 1..=PUSH_ATTEMPTS {
            let versions = serve(script, &mut served);
            if let Some(track) = ours.last() {
                if !versions.contains(track) {
                    return Err(not_listed(*track));
                }
            }

            let base = choose_base(&versions, &ours);
            run.bases.push(base);

            let head = versions.last().copied();
            if head.is_some_and(|track| satisfying.contains(&track.0)) {
                return Ok(());
            }

            let written = TrackNumber(writes[run.writes]);
            run.writes += 1;
            ours.push(written);

            let after = serve(script, &mut served);
            match landing(base, written, &after) {
                Landing::Clean => return Ok(()),
                Landing::NotVisible => return Err(not_listed(written)),
                Landing::Conflict => {}
            }

            if attempt == PUSH_ATTEMPTS {
                bail!("concurrent push detected");
            }
        }

        Ok(())
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
        let decision = decide(
            &repository(),
            &parsed,
            &index_with("refs/heads/gone", &"a".repeat(40)),
        );

        assert_eq!(decision.deletions, vec!["refs/heads/gone".to_string()]);
        assert_eq!(decision.results, vec!["ok refs/heads/gone".to_string()]);
    }

    // a refspec with no colon is reported, not acted on
    #[test]
    fn malformed_refspec() {
        let parsed = parse_specs(&["nonsense".to_string()]);
        let decision = decide(&repository(), &parsed, &Index::default());

        assert!(decision.updates.is_empty());
        assert!(decision.results[0].starts_with("error nonsense"));
    }

    // a ref that does not exist locally is rejected
    #[test]
    fn missing_source() {
        let parsed = parse_specs(&["refs/heads/nope-not-here:refs/heads/x".to_string()]);
        let decision = decide(&repository(), &parsed, &Index::default());

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

    // a listing that reaches our own version over our base settles the push
    #[test]
    fn clean_write() {
        let mut run = Run::default();

        drive(&[&[4], &[4, 7]], &[7], &[], &mut run).expect("push should settle");

        assert_eq!(run.writes, 1);
        assert_eq!(run.bases, vec![Some(TrackNumber(4))]);
    }

    // a version between our base and our write re-bases the next attempt on it
    #[test]
    fn merge_between() {
        let mut run = Run::default();
        let script: [&[u64]; 4] = [&[4], &[4, 5, 7], &[4, 5, 7], &[4, 5, 7, 9]];

        drive(&script, &[7, 9], &[9], &mut run).expect("push should settle");

        assert_eq!(run.writes, 2);
        assert_eq!(run.bases[1], Some(TrackNumber(5)));
    }

    // a version written after ours ends the push once it satisfies the update
    #[test]
    fn accepts_newer() {
        let mut run = Run::default();

        drive(&[&[4], &[4, 7, 9]], &[7], &[9], &mut run).expect("push should settle");

        assert_eq!(run.writes, 1);
        assert_eq!(run.bases[1], Some(TrackNumber(9)));
    }

    // a listing that never reaches our version is reported as such, without writing again
    #[test]
    fn never_listed() {
        let mut run = Run::default();

        let error = drive(&[&[4], &[4]], &[7], &[], &mut run).expect_err("push should stop");

        assert_eq!(run.writes, 1);
        assert!(error.to_string().contains("no storage node lists it yet"));
        assert!(!error.to_string().contains("concurrent"));
    }

    // an established head is never moved by a later push
    #[test]
    fn head_is_sticky() {
        let mut index = index_with("refs/heads/release", &"d".repeat(40));
        index.refs.insert("refs/heads/main".to_string(), "e".repeat(40));
        index.head = Some("refs/heads/release".to_string());


        apply(&repository(), &mut index, &Decision::default(), None);

        assert_eq!(index.head.as_deref(), Some("refs/heads/release"));
    }

    // a dangling head falls back to a branch that exists
    #[test]
    fn head_recovers() {
        let mut index = index_with("refs/heads/main", &"f".repeat(40));
        index.head = Some("refs/heads/deleted".to_string());

        apply(&repository(), &mut index, &Decision::default(), None);

        assert_eq!(index.head.as_deref(), Some("refs/heads/main"));
    }
}
