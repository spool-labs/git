//! The ref index: the one mutable thing in an otherwise append-only store
//!
//! Packs are content-addressed and immutable, so refs need somewhere to live
//! that can change. That is a *named* object, and a named write appends a new
//! version whose `hash(name)` key resolves to the newest one, which is exactly
//! mutable-pointer semantics for free.
//!
//! The encoding is kept deliberately small. Under 825 bytes a write is a single
//! inline transaction that is readable almost immediately. Past that it becomes
//! an erasure-coded track that has to be uploaded and certified before anyone can
//! read it back. Staying inline for as long as possible is the difference between
//! a push that takes half a second and one that takes several.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use tape_crypto::hash::hash;

/// Object name the index is stored under
///
/// Namespaced so it cannot collide with a bucket that also serves a website out
/// of its named objects.
pub const INDEX_NAME: &str = "git/refs.json";

/// Current index encoding version
pub const INDEX_VERSION: u64 = 1;

/// Hex characters kept from a pack's sha256
///
/// The SDK verifies every track against its on-chain commitment, but this digest
/// is what lets an *untrusted* gateway serve pack bytes. The index is proven
/// against the chain, so its digests are trustworthy statements about the packs.
/// That makes the width security-relevant rather than merely anti-corruption,
/// hence 128 bits. The 32 characters saved per pack still help keep the index
/// inline.
const DIGEST_CHARS: usize = 32;

/// Truncated sha256 of a pack, as recorded in the index
pub fn digest(bytes: &[u8]) -> String {
    let mut hex = hex::encode(hash(bytes).to_bytes());
    hex.truncate(DIGEST_CHARS);
    hex
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PackEntry {
    /// Track number on the bucket's tape
    ///
    /// Recorded so reads go straight to `track_pda(tape, track)`. Resolving by
    /// content hash would make the node scan every track on the tape per lookup.
    pub track: u64,

    pub size: u64,

    /// Truncated sha256 of the pack bytes
    ///
    /// `sha256` is accepted as an alias so indexes written before the field was
    /// renamed still load.
    #[serde(alias = "sha256")]
    pub digest: String,
}

impl PackEntry {
    /// Whether `bytes` is the pack this entry points at
    ///
    /// Compares only as many characters as the entry stored, so a full-length
    /// digest written by an older version still matches.
    pub fn matches(&self, bytes: &[u8]) -> bool {
        let full = hex::encode(hash(bytes).to_bytes());
        let width = self.digest.len().min(full.len());

        self.digest[..width] == full[..width]
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Index {
    pub version: u64,

    /// Ref HEAD points at, so `git clone` knows which branch to check out
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,

    /// Full refname to object id
    #[serde(default)]
    pub refs: BTreeMap<String, String>,

    /// Packs in push order, replayed in this order on fetch
    #[serde(default)]
    pub packs: Vec<PackEntry>,

    /// Track number of the index version this one was derived from
    ///
    /// Recorded so a reader can tell whether two versions were written from the
    /// same base, which is the only signal available that a push raced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            head: None,
            refs: BTreeMap::new(),
            packs: Vec::new(),
            parent: None,
        }
    }
}

impl Index {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Every object id the remote already has
    ///
    /// Used as the `--not` basis so a push only carries objects the remote is
    /// missing.
    pub fn tips(&self) -> Vec<String> {
        let mut tips = Vec::with_capacity(self.refs.len());
        for object_id in self.refs.values() {
            tips.push(object_id.clone());
        }
        tips
    }

    /// Whether this index already lists the given pack
    pub fn has_pack(&self, track: u64) -> bool {
        for entry in &self.packs {
            if entry.track == track {
                return true;
            }
        }
        false
    }

    /// Take on every pack from another version, keeping push order
    ///
    /// Packs are immutable and purely additive, so their union is always safe, and
    /// dropping one would orphan objects somebody else's refs depend on.
    pub fn absorb_packs(&mut self, other: &Index) {
        for entry in &other.packs {
            if !self.has_pack(entry.track) {
                self.packs.push(entry.clone());
            }
        }
        self.packs.sort_by_key(|entry| entry.track);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(track: u64, bytes: &[u8]) -> PackEntry {
        PackEntry {
            track,
            size: bytes.len() as u64,
            digest: digest(bytes),
        }
    }

    // a digest matches the bytes it was made from and nothing else
    #[test]
    fn digest_matching() {
        let pack = entry(1, b"pack contents");

        assert!(pack.matches(b"pack contents"));
        assert!(!pack.matches(b"pack contentt"));
    }

    // a full-length digest from an older writer still verifies
    #[test]
    fn legacy_digest() {
        let mut pack = entry(1, b"pack contents");
        pack.digest = hex::encode(hash(b"pack contents").to_bytes());

        assert!(pack.matches(b"pack contents"));
        assert!(!pack.matches(b"something else"));
    }

    // the older field name still deserializes
    #[test]
    fn sha256_alias() {
        let json = br#"{"version":1,"packs":[{"track":3,"size":9,"sha256":"abcdef"}]}"#;

        let index = Index::decode(json).expect("index should decode");

        assert_eq!(index.packs[0].digest, "abcdef");
    }

    // a round trip preserves refs, head and packs
    #[test]
    fn round_trip() {
        let mut index = Index {
            head: Some("refs/heads/main".to_string()),
            ..Default::default()
        };
        index
            .refs
            .insert("refs/heads/main".to_string(), "a".repeat(40));
        index.packs.push(entry(7, b"pack"));

        let decoded = Index::decode(&index.encode().expect("encode")).expect("decode");

        assert_eq!(decoded.head.as_deref(), Some("refs/heads/main"));
        assert_eq!(decoded.refs.len(), 1);
        assert_eq!(decoded.packs[0].track, 7);
    }

    // absorbing another version keeps both pack sets, ordered, without duplicates
    #[test]
    fn absorb_packs() {
        let mut ours = Index::default();
        ours.packs.push(entry(4, b"ours"));
        let mut theirs = Index::default();
        theirs.packs.push(entry(2, b"theirs"));
        theirs.packs.push(entry(4, b"ours"));

        ours.absorb_packs(&theirs);

        let mut tracks = Vec::new();
        for pack in &ours.packs {
            tracks.push(pack.track);
        }
        assert_eq!(tracks, vec![2, 4]);
    }

    // a handful of refs and packs still fits inside one inline write
    #[test]
    fn stays_inline() {
        const INLINE_LIMIT: usize = 825;
        let mut index = Index {
            head: Some("refs/heads/main".to_string()),
            ..Default::default()
        };
        for name in ["main", "develop", "release", "feature-one", "feature-two"] {
            index
                .refs
                .insert(format!("refs/heads/{name}"), "a".repeat(40));
        }
        for track in 0..5 {
            index.packs.push(entry(track, b"pack"));
        }

        let encoded = index.encode().expect("encode");

        assert!(
            encoded.len() < INLINE_LIMIT,
            "index grew to {} bytes",
            encoded.len()
        );
    }
}
