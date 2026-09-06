//! Direct publication of a complete local repository to a fresh tape.

use std::{path::Path, sync::Arc};

use anyhow::{anyhow, Context, Result};
use peer_http::HttpApi;
use rpc_solana::{RpcConfig, SolanaRpc};
use tape_core::{prelude::StorageUnits, types::ContentType};
use tape_protocol::api::CertifyRes;
use tape_sdk::{
    keys::{helpers::load_ed25519_keypair, tape_key::TapeKey},
    track::write::WrittenTrack,
    Tapedrive,
};

use crate::{
    git::Repository,
    index::{digest, Index, PackEntry, INDEX_NAME},
};

type Client = Tapedrive<SolanaRpc, HttpApi>;
const INDEX_CONTENT_TYPE: &str = "application/json";

#[derive(Clone)]
pub struct Publisher {
    client: Arc<Client>,
}

pub struct PublishedRepository {
    pub head: String,
    pub commit: String,
    pub bytes: u64,
    pub verification: Verification,
}

pub struct Verification {
    client: Arc<Client>,
    key: TapeKey,
    tracks: Vec<PendingTrack>,
}

struct PendingTrack {
    written: WrittenTrack,
    receipts: Vec<CertifyRes>,
}

impl Publisher {
    pub fn connect(rpc_url: String, payer_path: &Path) -> Result<Self> {
        let payer = load_ed25519_keypair(payer_path)
            .with_context(|| format!("load payer {}", payer_path.display()))?;
        let rpc = SolanaRpc::new(RpcConfig {
            endpoints: vec![rpc_url],
            ..Default::default()
        })
        .map_err(|error| anyhow!("configure Solana RPC: {error}"))?;
        Ok(Self {
            client: Arc::new(Tapedrive::new(rpc, payer)),
        })
    }

    pub async fn publish(
        &self,
        repository: Repository,
        pack_directory: &Path,
        tape_key_json: &str,
        capacity_bytes: u64,
    ) -> Result<PublishedRepository> {
        let key = TapeKey::from_json_bytes(tape_key_json.as_bytes())
            .map_err(|error| anyhow!("load tape key: {error}"))?;
        let (head, refs, pack_paths) = tokio::task::spawn_blocking({
            let repository = repository.clone();
            let pack_directory = pack_directory.to_owned();
            move || {
                let head = repository.head_ref()?;
                let refs = repository.branches()?;
                let packs = repository.pack_all(&pack_directory)?;
                Ok::<_, anyhow::Error>((head, refs, packs))
            }
        })
        .await
        .context("Git pack task stopped")??;

        let commit = refs
            .get(&head)
            .cloned()
            .context("HEAD does not name a local branch")?;
        let mut bytes = 0u64;
        for path in &pack_paths {
            bytes = bytes
                .checked_add(tokio::fs::metadata(path).await?.len())
                .context("repository size overflow")?;
        }
        if bytes >= capacity_bytes {
            return Err(anyhow!(
                "Git packs require {bytes} bytes but the tape has {capacity_bytes} bytes"
            ));
        }
        let mut index = Index {
            head: Some(head.clone()),
            refs,
            ..Default::default()
        };
        let mut pending = Vec::new();
        for path in pack_paths {
            let contents = tokio::fs::read(&path)
                .await
                .with_context(|| format!("read pack {}", path.display()))?;
            let size = contents.len() as u64;
            let file = tokio::fs::File::open(&path).await?;
            let (written, receipts) = self
                .client
                .store_named_stream(
                    &key,
                    b"",
                    ContentType::Unknown,
                    StorageUnits::from_bytes(size),
                    file,
                )
                .await
                .map_err(|error| anyhow!("store Git pack: {error}"))?;
            index.packs.push(PackEntry {
                track: written.track.track_number.0,
                size,
                digest: digest(&contents),
                stream: true,
            });
            pending.push(PendingTrack { written, receipts });
        }

        let encoded = index.encode()?;
        // Readers find this object before they know which tracks are streams,
        // so keep the small ref index as a direct blob. Its entries identify
        // the streamed pack tracks that need assembly.
        let (written, plan) = self
            .client
            .write_named_blob(
                &key,
                INDEX_NAME,
                ContentType::from_str(INDEX_CONTENT_TYPE),
                &encoded,
            )
            .await
            .map_err(|error| anyhow!("register Git ref index: {error}"))?;
        let receipts = self
            .client
            .upload(&written, &plan)
            .await
            .map_err(|error| anyhow!("upload Git ref index: {error}"))?;
        pending.push(PendingTrack { written, receipts });

        Ok(PublishedRepository {
            head,
            commit,
            bytes,
            verification: Verification {
                client: self.client.clone(),
                key,
                tracks: pending,
            },
        })
    }
}

impl Verification {
    pub async fn certify(self) -> Result<()> {
        for pending in self.tracks {
            self.client
                .certify_with_receipts(&self.key, &pending.written, &pending.receipts)
                .await
                .map_err(|error| anyhow!("certify Git track: {error}"))?;
        }
        Ok(())
    }
}
