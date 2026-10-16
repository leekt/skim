//! Merkle-Patricia proof checks for SNAP point lookups (PROJECT.md §3.4, §8 "Verified State").
//!
//! [`lookup`] walks from `root` along a hashed key through the proof nodes a peer returned:
//! - `Ok(Some(value))`: the key is present, with this leaf value
//! - `Ok(None)`: the key is **proven absent** (the path diverges, or a branch has no child there)
//! - `Err(_)`: the proof doesn't settle it (missing node, malformed node). The caller must
//!   treat this as "unavailable", never as zero.
//!
//! SNAP range proofs carry the nodes for both range boundaries in one list. Nodes are looked up
//! by hash, so extra nodes are harmless.

use std::collections::HashMap;

use alloy_primitives::{B256, Bytes, keccak256};
use alloy_rlp::Decodable;
use alloy_trie::{
    EMPTY_ROOT_HASH, HashBuilder, Nibbles,
    nodes::{RlpNode, TrieNode},
};

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ProofError {
    #[error("proof is missing node {0}")]
    MissingNode(B256),
    #[error("malformed trie node: {0}")]
    Malformed(String),
}

pub fn lookup(root: B256, key: B256, proof: &[Bytes]) -> Result<Option<Vec<u8>>, ProofError> {
    lookup_path(root, key, proof).map(|(v, _)| v)
}

/// [`lookup`], also returning the path it walked: every hash-referenced node from the root
/// toward `key`, in order (the EIP-1186 `eth_getProof` proof for this key).
pub fn lookup_path(root: B256, key: B256, proof: &[Bytes]) -> Result<(Option<Vec<u8>>, Vec<Bytes>), ProofError> {
    let mut walked: Vec<Bytes> = Vec::new();
    let r = lookup_inner(root, key, proof, &mut walked)?;
    Ok((r, walked))
}

/// The proof for `key` in a trie we hold completely (`leaves`, hashed keys + RLP values), e.g.
/// a small storage trie a peer returned whole. Callers check the root first ([`root_of`]).
pub fn proof_from_leaves(mut leaves: Vec<(B256, Bytes)>, key: B256) -> Vec<Bytes> {
    use alloy_trie::proof::ProofRetainer;
    leaves.sort_by(|a, b| a.0.cmp(&b.0));
    leaves.dedup_by(|a, b| a.0 == b.0);
    let target = Nibbles::unpack(key);
    let mut hb = HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![target.clone()]));
    for (k, v) in &leaves {
        hb.add_leaf(Nibbles::unpack(k), v);
    }
    let _ = hb.root();
    hb.take_proof_nodes()
        .into_nodes_sorted()
        .into_iter()
        .filter(|(path, _)| target.starts_with(path))
        .map(|(_, node)| node)
        .collect()
}

fn lookup_inner(root: B256, key: B256, proof: &[Bytes], walked: &mut Vec<Bytes>) -> Result<Option<Vec<u8>>, ProofError> {
    if root == EMPTY_ROOT_HASH {
        return Ok(None);
    }
    let nodes: HashMap<B256, &[u8]> = proof.iter().map(|n| (keccak256(n), n.as_ref())).collect();
    let path = Nibbles::unpack(key);
    let mut pos = 0usize;
    let mut cur: Vec<u8> = nodes.get(&root).ok_or(ProofError::MissingNode(root))?.to_vec();
    walked.push(Bytes::copy_from_slice(&cur));
    let mut resolve = |child: &RlpNode| -> Result<Vec<u8>, ProofError> {
        match child.as_hash() {
            Some(h) => {
                let n = nodes.get(&h).map(|n| n.to_vec()).ok_or(ProofError::MissingNode(h))?;
                walked.push(Bytes::copy_from_slice(&n));
                Ok(n)
            }
            None => Ok(child.to_vec()), // inlined node (< 32 bytes): part of its parent
        }
    };
    loop {
        let node = TrieNode::decode(&mut cur.as_slice()).map_err(|e| ProofError::Malformed(e.to_string()))?;
        match node {
            TrieNode::EmptyRoot => return Ok(None),
            TrieNode::Branch(b) => {
                if pos >= path.len() {
                    return Err(ProofError::Malformed("branch at end of a hashed key".into()));
                }
                let nib = path.get_unchecked(pos);
                match b.as_ref().children().find(|(i, _)| *i == nib).and_then(|(_, c)| c) {
                    None => return Ok(None),
                    Some(child) => {
                        pos += 1;
                        cur = resolve(child)?;
                    }
                }
            }
            TrieNode::Extension(e) => {
                let rest = path.slice(pos..);
                if !rest.starts_with(&e.key) {
                    return Ok(None);
                }
                pos += e.key.len();
                cur = resolve(&e.child)?;
            }
            TrieNode::Leaf(l) => {
                return Ok((path.slice(pos..) == l.key).then_some(l.value));
            }
        }
    }
}

/// Root of a trie holding exactly `leaves` (hashed key, RLP value). Used when a peer returns a
/// whole trie without proof: the response is accepted only if this equals the expected root.
/// `None` if a key repeats: peers control these keys, and a repeated key is never a valid trie
/// (the hash builder would panic on it).
pub fn root_of(mut leaves: Vec<(B256, Bytes)>) -> Option<B256> {
    leaves.sort_by(|a, b| a.0.cmp(&b.0));
    if leaves.windows(2).any(|w| w[0].0 == w[1].0) {
        return None;
    }
    let mut hb = HashBuilder::default();
    for (k, v) in &leaves {
        hb.add_leaf(Nibbles::unpack(k), v);
    }
    Some(hb.root())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use alloy_trie::proof::ProofRetainer;

    /// A storage-like trie of `n` slots (keccak keys, RLP U256 values) and proofs for `targets`.
    fn trie(n: u64, targets: &[B256]) -> (B256, Vec<(B256, Bytes)>, Vec<Bytes>) {
        let mut leaves: Vec<(B256, Bytes)> = (1..=n)
            .map(|i| (keccak256(U256::from(i).to_be_bytes::<32>()), alloy_rlp::encode(U256::from(i * 7)).into()))
            .collect();
        leaves.sort_by(|a, b| a.0.cmp(&b.0));
        let retainer = ProofRetainer::new(targets.iter().map(|t| Nibbles::unpack(t)).collect());
        let mut hb = HashBuilder::default().with_proof_retainer(retainer);
        for (k, v) in &leaves {
            hb.add_leaf(Nibbles::unpack(k), v);
        }
        let root = hb.root();
        let proof = hb.take_proof_nodes().into_nodes_sorted().into_iter().map(|(_, b)| b).collect();
        (root, leaves, proof)
    }

    fn key(i: u64) -> B256 {
        keccak256(U256::from(i).to_be_bytes::<32>())
    }

    #[test]
    fn inclusion_and_exclusion() {
        let (present, absent) = (key(42), key(10_000));
        let (root, _, proof) = trie(300, &[present, absent]);
        let v = lookup(root, present, &proof).unwrap().unwrap();
        assert_eq!(U256::decode(&mut v.as_slice()).unwrap(), U256::from(42 * 7));
        assert_eq!(lookup(root, absent, &proof).unwrap(), None);
    }

    #[test]
    fn missing_or_wrong_proof_is_an_error_not_absence() {
        let k = key(42);
        let (root, _, proof) = trie(300, &[k]);
        // No proof at all.
        assert!(matches!(lookup(root, k, &[]), Err(ProofError::MissingNode(_))));
        // A proof for a different trie.
        let (other_root, _, _) = trie(301, &[k]);
        assert!(matches!(lookup(other_root, k, &proof), Err(ProofError::MissingNode(_))));
        // A truncated proof (drop the deepest node).
        let mut short = proof.clone();
        short.pop();
        // Whatever a truncated proof yields, it must never be read as "proven absent".
        assert!(!matches!(lookup(root, k, &short), Ok(None)));
    }

    #[test]
    fn tampered_node_breaks_the_hash_chain() {
        let k = key(42);
        let (root, _, mut proof) = trie(300, &[k]);
        let last = proof.len() - 1;
        let mut bad = proof[last].to_vec();
        let n = bad.len();
        bad[n - 1] ^= 1; // flip the value's last byte
        proof[last] = bad.into();
        assert!(matches!(lookup(root, k, &proof), Err(ProofError::MissingNode(_))));
    }

    #[test]
    fn empty_root_proves_absence_and_full_trie_root_check() {
        assert_eq!(lookup(EMPTY_ROOT_HASH, key(1), &[]).unwrap(), None);
        let (root, leaves, _) = trie(5, &[]);
        assert_eq!(root_of(leaves.clone()), Some(root));
        let mut tampered = leaves;
        tampered[0].1 = alloy_rlp::encode(U256::from(999)).into();
        assert_ne!(root_of(tampered), Some(root));
    }

    #[test]
    fn repeated_keys_from_a_peer_are_rejected_not_a_panic() {
        // Found by fuzzing (fuzz/fuzz_targets/snap_proof.rs): a response repeating a slot hash
        // made HashBuilder::add_leaf assert and panic.
        let k = keccak256([1u8]);
        let leaves = vec![(k, Bytes::from_static(&[0x01])), (k, Bytes::from_static(&[0x02]))];
        assert_eq!(root_of(leaves), None);
    }

    #[test]
    fn proof_paths_are_ordered_and_self_verifying() {
        let keys: Vec<B256> = (1..=40u64).map(|i| keccak256(U256::from(i).to_be_bytes::<32>())).collect();
        let (root, leaves, proof) = trie(40, &keys[..1]);
        let (v, path) = lookup_path(root, keys[0], &proof).unwrap();
        assert!(v.is_some());
        assert_eq!(keccak256(&path[0]), root); // starts at the root
        // The ordered path alone (no extra nodes) proves the key again.
        assert_eq!(lookup(root, keys[0], &path).unwrap(), v);
        // Built from the whole trie: same answer.
        let built = proof_from_leaves(leaves, keys[0]);
        assert_eq!(keccak256(&built[0]), root);
        assert_eq!(lookup(root, keys[0], &built).unwrap(), v);
    }
}
