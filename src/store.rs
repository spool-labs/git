//! Tapedrive side of the helper: read the index, read packs, write packs

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Error, Result};

use peer_http::HttpApi;
use rpc_solana::{RpcConfig, SolanaRpc};
use tape_api::program::tapedrive::track_pda;
use tape_core::types::{ContentType, TrackNumber};
use tape_crypto::ed25519::Keypair;
use tape_crypto::hash::hash;
use tape_crypto::prelude::Address;
use tape_sdk::error::TapedriveError;
use tape_sdk::keys::helpers::load_ed25519_keypair;
use tape_sdk::keys::tape_key::TapeKey;
use tape_sdk::{Gateway, Tapedrive};

use crate::index::{digest, Index, PackEntry, INDEX_CONTENT_TYPE, INDEX_NAME};

const DEFAULT_RPC: &str = "https://api.devnet.solana.com";

/// Where the `tape` CLI files a tape keypair, relative to the home directory
const CASSETTE_DIR: &str = ".tape/cassettes";

/// Where the solana CLI keeps its default keypair, relative to the home directory
const SOLANA_KEYPAIR: &str = ".config/solana/id.json";

/// Backoff between read attempts
///
/// The last entry is never slept on, so this is nine tries over about forty-two
/// seconds.
const READ_BACKOFF_MS: [u64; 9] = [400, 800, 1_600, 3_200, 6_000, 8_000, 10_000, 12_000, 0];

/// Backoff while a freshly created tape propagates to the RPC reader used by
/// this process.
const ACCOUNT_PROPAGATION_BACKOFF_MS: [u64; 8] =
    [500, 1_000, 2_000, 4_000, 5_000, 5_000, 5_000, 0];

/// How many times to re-try a gateway fetch that came back rate limited
const GATEWAY_RATE_LIMIT_RETRIES: u64 = 3;

/// Wait used when a rate-limited gateway gives us no `Retry-After`
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);

/// Ceiling on an honoured `Retry-After`
///
/// A public gateway asking us to wait several minutes is a signal to go direct,
/// not to stall the user's clone.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);

/// Track-listing page size when enumerating index versions
const TRACK_PAGE_SIZE: u32 = 1_000;

/// Backoff between listings while one catches up with the chain, last entry never slept on
const INDEX_LISTING_BACKOFF_MS: [u64; 8] = [100, 250, 500, 1_000, 2_000, 4_000, 8_000, 0];

pub struct Store {
    /// Direct peer client
    ///
    /// Does every write, and every read whose bytes cannot be proven any other
    /// way.
    sdk: Tapedrive<SolanaRpc, HttpApi>,

    /// Optional gateway for bulk reads
    ///
    /// Reachable over plain 443 where storage nodes may not be, and much faster,
    /// but its bytes arrive unproven, so nothing returns them to git unchecked.
    gateway: Option<Gateway<SolanaRpc>>,

    /// The tape address parsed out of the `tape://` url
    bucket: Address,

    /// Present only when we hold the tape's key, which is what makes a remote
    /// pushable. Reads never need it.
    cassette: Option<TapeKey>,

    /// Whether a fee payer was found. Reads do not need one, writes do.
    has_payer: bool,
}

/// Build an RPC handle for the configured endpoint
fn open_rpc(rpc_url: &str) -> Result<SolanaRpc> {
    SolanaRpc::new(RpcConfig {
        endpoints: vec![rpc_url.to_string()],
        ..Default::default()
    })
    .map_err(|error| anyhow!("solana rpc {rpc_url}: {error}"))
}

/// Locate a fee payer, if there is one to find
///
/// Cloning needs no keypair at all, since reads are public with no stake and no
/// account, so absence downgrades the remote to read-only rather than failing.
/// Requiring one here would mean nobody could clone without first installing a
/// Solana wallet they never spend from. An explicitly configured path is
/// different, because a bad value there is a mistake worth reporting rather than
/// something to silently ignore.
fn load_payer() -> Result<Option<Keypair>> {
    if let Ok(configured) = std::env::var("TAPE_KEYPAIR") {
        let path = PathBuf::from(configured);
        let payer = load_ed25519_keypair(&path)
            .map_err(|error| anyhow!("payer keypair {}: {error}", path.display()))?;
        return Ok(Some(payer));
    }

    let Some(path) = dirs::home_dir().map(|home| home.join(SOLANA_KEYPAIR)) else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }

    Ok(load_ed25519_keypair(&path).ok())
}

/// Locate the tape's own key, which is what authorises a push
fn load_cassette(bucket: Address) -> Result<Option<TapeKey>> {
    let path = match std::env::var("TAPE_CASSETTE") {
        Ok(configured) => Some(PathBuf::from(configured)),
        Err(_) => dirs::home_dir()
            .map(|home| home.join(CASSETTE_DIR).join(format!("{bucket}.json")))
            .filter(|path| path.exists()),
    };

    let Some(path) = path else {
        return Ok(None);
    };

    let cassette = TapeKey::load(&path)
        .map_err(|error| anyhow!("tape keypair {}: {error}", path.display()))?;

    // A cassette for a different tape would write to the wrong bucket and
    // silently produce a remote that never shows the pushed refs.
    if cassette.address() != bucket {
        bail!(
            "tape keypair controls {} but the remote is {bucket}",
            cassette.address()
        );
    }

    Ok(Some(cassette))
}

/// Build a gateway client when one is configured
fn open_gateway(rpc_url: &str) -> Result<Option<Gateway<SolanaRpc>>> {
    let Ok(url) = std::env::var("TAPE_GATEWAY_URL") else {
        return Ok(None);
    };
    let url = url.trim();
    if url.is_empty() {
        return Ok(None);
    }

    // A gateway needs its own RPC handle, because the SDK takes one by value.
    let gateway = Tapedrive::new_gateway_read_only(open_rpc(rpc_url)?, url)
        .map_err(|error| anyhow!("gateway {url}: {error}"))?;

    Ok(Some(gateway))
}

impl Store {
    /// Build a client for `tape://<bucket>`
    ///
    /// Reads work with nothing configured. Push additionally needs a fee payer
    /// and the tape's own key.
    pub fn open(bucket: &str) -> Result<Self> {
        let bucket: Address = bucket
            .parse()
            .map_err(|_| anyhow!("`{bucket}` is not a tape address"))?;

        let rpc_url = std::env::var("TAPE_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC.to_string());
        let payer = load_payer()?;
        let cassette = load_cassette(bucket)?;
        let gateway = open_gateway(&rpc_url)?;

        let has_payer = payer.is_some();
        let rpc = open_rpc(&rpc_url)?;
        let sdk = match payer {
            Some(payer) => Tapedrive::new(rpc, payer),
            None => Tapedrive::new_read_only(rpc),
        };

        Ok(Self {
            sdk,
            gateway,
            bucket,
            cassette,
            has_payer,
        })
    }

    /// Identity for a pack in `.git/tape/installed-packs`
    ///
    /// Scoped by bucket so a repo with two `tape://` remotes cannot confuse one
    /// remote's track numbers for the other's.
    pub fn installed_key(&self, track: u64) -> String {
        format!("{}:{track}", self.bucket)
    }

    /// The tape key needed to push, with a diagnosis when we cannot
    pub fn writable(&self) -> Result<&TapeKey> {
        if !self.has_payer {
            bail!(
                "pushing needs a fee payer: set TAPE_KEYPAIR or create \
                 ~/{SOLANA_KEYPAIR} (cloning this remote needs neither)"
            );
        }

        self.cassette.as_ref().ok_or_else(|| {
            anyhow!(
                "no tape keypair for {}: set TAPE_CASSETTE or place it at \
                 ~/{CASSETTE_DIR}/{}.json to push",
                self.bucket,
                self.bucket
            )
        })
    }

    /// Fetch a track's bytes through the gateway, unproven
    ///
    /// Returns `None` when the gateway cannot serve it, whether unconfigured, rate
    /// limited past our patience, or erroring, so callers fall back to storage nodes
    /// instead of failing. Rate limiting is expected rather than exceptional.
    /// Public gateways are throttled, and the right response is to honour
    /// `Retry-After` briefly and then stop leaning on someone else's capacity.
    async fn gateway_bytes(&self, track: TrackNumber) -> Option<Vec<u8>> {
        let gateway = self.gateway.as_ref()?;
        let address = track_pda(self.bucket, track).0;

        for attempt in 0..=GATEWAY_RATE_LIMIT_RETRIES {
            match gateway.read_track(&address).await {
                Ok(bytes) => return Some(bytes),
                Err(TapedriveError::RateLimited { retry_after }) => {
                    if attempt == GATEWAY_RATE_LIMIT_RETRIES {
                        eprintln!(
                            "tape: gateway still rate limiting; reading from \
                             storage nodes instead"
                        );
                        return None;
                    }
                    let wait = retry_after
                        .unwrap_or(DEFAULT_RETRY_AFTER)
                        .min(MAX_RETRY_AFTER);
                    eprintln!(
                        "tape: gateway rate limited, waiting {:.1}s",
                        wait.as_secs_f32()
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(error) => {
                    eprintln!("tape: gateway could not serve track {} ({error})", track.0);
                    return None;
                }
            }
        }

        None
    }

    /// Read a track and prove it against its on-chain commitment
    ///
    /// Tries the gateway first when one is configured, then checks the bytes with
    /// `verify`, which compares them to the commitment recorded on-chain. Bytes
    /// that fail are discarded and refetched from storage nodes, so a gateway buys
    /// speed and reachability without being trusted: one that is broken, stale, or
    /// actively lying simply loses.
    async fn read_track_proven(&self, track: TrackNumber) -> Result<Vec<u8>> {
        if let Some(bytes) = self.gateway_bytes(track).await {
            let address = track_pda(self.bucket, track).0;
            match self.sdk.verify(&address, &bytes).await {
                Ok(true) => return Ok(bytes),
                Ok(false) => eprintln!(
                    "tape: gateway bytes for track {} do not match the on-chain \
                     commitment; refetching from storage nodes",
                    track.0
                ),
                Err(error) => eprintln!(
                    "tape: could not verify gateway bytes for track {} ({error}); \
                     refetching from storage nodes",
                    track.0
                ),
            }
        }

        self.read_track(track).await
    }

    /// Read one track's verified bytes straight from storage nodes
    ///
    /// The SDK's direct peer read checks the bytes against the track's on-chain
    /// commitment itself, so this path is trustless by construction.
    ///
    /// Retries, because a track can be certified on-chain before enough peers will
    /// serve its slices. Reading back something written seconds ago, which is
    /// exactly what a push does to confirm its own index, otherwise fails with
    /// "insufficient slices" on a track that is perfectly fine.
    async fn read_track(&self, track: TrackNumber) -> Result<Vec<u8>> {
        let address = track_pda(self.bucket, track).0;
        let mut last_error = None;

        for (attempt, backoff) in READ_BACKOFF_MS.iter().enumerate() {
            match self.sdk.read(&address).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) => {
                    if attempt + 1 < READ_BACKOFF_MS.len() {
                        eprintln!(
                            "tape: track {} not readable yet ({error}); retrying in {backoff}ms",
                            track.0
                        );
                        tokio::time::sleep(Duration::from_millis(*backoff)).await;
                    }
                    last_error = Some(error);
                }
            }
        }

        match last_error {
            Some(error) => Err(anyhow!("read track {} ({address}): {error}", track.0)),
            None => Err(anyhow!("read track {} ({address}): no attempt made", track.0)),
        }
    }

    /// The current ref index, or `None` when nothing has been pushed yet
    pub async fn read_index(&self) -> Result<Option<(Index, TrackNumber)>> {
        // Ref metadata is already indexed on chain. Enumerating the tape avoids
        // making repository discovery depend on one assigned storage peer being
        // reachable, while the index bytes themselves still go through the
        // gateway-and-proof path below.
        let Some(track) = self.index_versions().await?.last().copied() else {
            return Ok(None);
        };
        let index = self.read_index_at(track).await?;
        Ok(Some((index, track)))
    }

    /// Read a specific version of the ref index
    pub async fn read_index_at(&self, track: TrackNumber) -> Result<Index> {
        let bytes = self.read_track_proven(track).await?;

        Index::decode(&bytes).context("decode ref index")
    }

    /// Every version of the ref index ever written, ascending
    ///
    /// The store is append-only, so superseded versions are still there. That is
    /// what lets a pusher notice it raced with someone: there is no
    /// compare-and-swap to lean on, but nothing is ever actually lost either.
    pub async fn index_versions(&self) -> Result<Vec<TrackNumber>> {
        let (versions, _) = self.index_versions_seen().await?;

        Ok(versions)
    }

    /// Every version of the ref index, with the highest track number the listing reached
    async fn index_versions_seen(&self) -> Result<(Vec<TrackNumber>, TrackNumber)> {
        let mut last_error = None;
        for (attempt, backoff) in ACCOUNT_PROPAGATION_BACKOFF_MS.iter().enumerate() {
            match self.index_versions_once().await {
                Ok(seen) => return Ok(seen),
                Err(error)
                    if is_account_propagation_error(&error)
                        && attempt + 1 < ACCOUNT_PROPAGATION_BACKOFF_MS.len() =>
                {
                    eprintln!(
                        "tape: tape account is not visible yet; retrying in {backoff}ms"
                    );
                    tokio::time::sleep(Duration::from_millis(*backoff)).await;
                    last_error = Some(error);
                }
                Err(error) => return Err(anyhow!("list index versions: {error}")),
            }
        }

        match last_error {
            Some(error) => Err(anyhow!("list index versions: {error}")),
            None => Err(anyhow!("list index versions: no attempt made")),
        }
    }

    /// Every version of the ref index, from a listing that reaches the chain and `at_least`
    pub async fn index_versions_complete(&self, at_least: TrackNumber) -> Result<Vec<TrackNumber>> {
        let target = self.listing_target(at_least).await;

        for (attempt, backoff) in INDEX_LISTING_BACKOFF_MS.iter().enumerate() {
            let (versions, highest) = self.index_versions_seen().await?;
            if highest.0 >= target.0 {
                return Ok(versions);
            }

            if attempt + 1 < INDEX_LISTING_BACKOFF_MS.len() {
                eprintln!(
                    "tape: listing has reached track {} of {}; retrying in {backoff}ms",
                    highest.0, target.0
                );
                tokio::time::sleep(Duration::from_millis(*backoff)).await;
            }
        }

        Err(not_listed(target))
    }

    /// The track number a listing has to reach before a merge can trust it
    async fn listing_target(&self, at_least: TrackNumber) -> TrackNumber {
        match self.sdk.get_tape(&self.bucket).await {
            Ok(tape) => {
                let newest = tape.tracks.next_number().0.saturating_sub(1);
                TrackNumber(newest.max(at_least.0))
            }
            // A worse floor than the chain, still better than the first answer we get
            Err(error) => {
                eprintln!("tape: could not read the tape account ({error})");
                at_least
            }
        }
    }

    async fn index_versions_once(&self) -> Result<(Vec<TrackNumber>, TrackNumber), TapedriveError> {
        let key = hash(INDEX_NAME.as_bytes());
        let mut versions = Vec::new();
        let mut highest = TrackNumber(0);
        let mut cursor = None;

        loop {
            let (tracks, next) = self
                .sdk
                .list_tracks_by_tape(&self.bucket, cursor, TRACK_PAGE_SIZE)
                .await?;

            for track in &tracks {
                if track.track_number.0 > highest.0 {
                    highest = track.track_number;
                }
                if track.key == key {
                    versions.push(track.track_number);
                }
            }

            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        versions.sort_by_key(|track| track.0);

        Ok((versions, highest))
    }

    /// Fetch a pack recorded in the index, checking it against that record
    ///
    /// The index this entry came from was proven against its on-chain commitment,
    /// so its digest is a trustworthy statement about the pack. That is what makes
    /// it safe to take the bulk bytes from an untrusted gateway: a mismatch means
    /// we throw them away and go direct.
    pub async fn read_pack(&self, entry: &PackEntry) -> Result<Vec<u8>> {
        let track = TrackNumber(entry.track);

        if entry.stream {
            let address = track_pda(self.bucket, track).0;
            let bytes = self
                .sdk
                .read_bytes(&address)
                .await
                .map_err(|error| anyhow!("read pack stream at track {}: {error}", entry.track))?;
            if !entry.matches(&bytes) {
                bail!(
                    "pack stream at track {} does not match the digest the index recorded",
                    entry.track
                );
            }
            return Ok(bytes);
        }

        if let Some(bytes) = self.gateway_bytes(track).await {
            if entry.matches(&bytes) {
                return Ok(bytes);
            }
            eprintln!(
                "tape: gateway pack at track {} failed its digest; refetching \
                 from storage nodes",
                entry.track
            );
        }

        let bytes = self.read_track(track).await?;
        if !entry.matches(&bytes) {
            bail!(
                "pack at track {} does not match the digest the index recorded",
                entry.track
            );
        }

        Ok(bytes)
    }

    /// Store a pack as an unnamed, content-addressed track
    ///
    /// Unnamed writes key on `hash(payload)`, or the erasure-coding commitment once
    /// past the inline size, so identical packs are the same track and never appear
    /// in the bucket's object listing.
    pub async fn write_pack(&self, pack: &[u8]) -> Result<PackEntry> {
        let key = self.writable()?;
        let track = self
            .sdk
            .write_track(key, pack)
            .await
            .map_err(|error| anyhow!("write pack ({} bytes): {error}", pack.len()))?;

        Ok(PackEntry {
            track: track.track_number.0,
            size: pack.len() as u64,
            digest: digest(pack),
            stream: false,
        })
    }

    /// Publish a new version of the ref index
    ///
    /// Returns its track number so the caller can check whether it really ended up
    /// as the visible head.
    pub async fn write_index(&self, index: &Index) -> Result<TrackNumber> {
        let key = self.writable()?;
        let bytes = index.encode()?;
        let track = self
            .sdk
            .write_named_track(
                key,
                INDEX_NAME,
                ContentType::from_str(INDEX_CONTENT_TYPE),
                &bytes,
            )
            .await
            .map_err(|error| anyhow!("write ref index: {error}"))?;

        Ok(track.track_number)
    }
}

/// Error for a listing that has not caught up with the chain
pub fn not_listed(track: TrackNumber) -> Error {
    anyhow!(
        "no storage node has listed this tape up to track {} yet, so the ref index \
         cannot be merged safely. Nothing was lost, every object and every index \
         version is still stored, and the push will go through if you retry it in a \
         moment",
        track.0
    )
}

fn is_account_propagation_error(error: &TapedriveError) -> bool {
    matches!(error, TapedriveError::Rpc(error) if is_account_propagation_category(error.category()))
}

fn is_account_propagation_category(category: &str) -> bool {
    category == "not_found"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propagation_retry() {
        assert!(is_account_propagation_category("not_found"));
        assert!(!is_account_propagation_category("rpc_error"));
        assert!(!is_account_propagation_category("timeout"));
        assert!(!is_account_propagation_category("tx_error"));
    }
}
