use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::{index::Index, store::Store};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Cloneability {
    pub head: String,
    pub refs: BTreeMap<String, String>,
    pub pack_count: usize,
}

/// Resolve and verify the current ref index without requiring a cassette.
///
/// `None` means no repository has been published yet. `Some` means the same
/// ref advertisement used by `git ls-remote` is readable and internally
/// coherent, which is the gate a UI should use before revealing a `tape://`
/// clone URL.
pub async fn probe_cloneable(bucket: &str) -> Result<Option<Cloneability>> {
    let store = Store::open(bucket)?;
    let Some((index, _)) = store.read_index().await? else {
        return Ok(None);
    };
    validate(index)
}

fn validate(index: Index) -> Result<Option<Cloneability>> {
    let Some(head) = index.head.clone() else {
        return Ok(None);
    };
    if !index.refs.contains_key(&head) {
        bail!("published Git HEAD points at a missing ref: {head}");
    }
    for (name, object_id) in &index.refs {
        if !name.starts_with("refs/") || name.contains(['\0', '\n', '\r']) {
            bail!("published Git index contains an invalid ref name");
        }
        if object_id.len() != 40 || !object_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("published Git index contains an invalid object id for {name}");
        }
    }
    Ok(Some(Cloneability {
        head,
        refs: index.refs,
        pack_count: index.packs.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let mut index = Index {
            head: Some("refs/heads/main".into()),
            ..Default::default()
        };
        index
            .refs
            .insert("refs/heads/main".into(), "a".repeat(40));
        index
    }

    #[test]
    fn coherent_index() {
        let result = validate(index()).expect("valid").expect("cloneable");
        assert_eq!(result.head, "refs/heads/main");
        assert_eq!(result.refs.len(), 1);
    }

    #[test]
    fn empty_index() {
        assert!(validate(Index::default()).expect("valid").is_none());
    }

    #[test]
    fn dangling_head() {
        let mut index = index();
        index.head = Some("refs/heads/missing".into());
        assert!(validate(index).is_err());
    }

    #[test]
    fn invalid_object_id() {
        let mut index = index();
        index.refs.insert("refs/heads/main".into(), "nope".into());
        assert!(validate(index).is_err());
    }
}
