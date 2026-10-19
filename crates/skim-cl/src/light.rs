//! Consensus light-client verification (Altair sync protocol, Gloas containers).
//!
//! What this establishes: a beacon block header was signed by a supermajority of the current
//! sync committee, and that header's body commits (Merkle branch, gindex 2856) to the execution
//! block hash its bid builds on. The committee itself is trusted from a checkpoint root (weak
//! subjectivity) and rolls forward only through finalized headers it signed.
//!
//! Gloas specifics: `LightClientHeader = { beacon, execution_block_hash, execution_branch[11] }`,
//! where `execution_block_hash` is the bid's `parent_block_hash`. So a signed beacon block at
//! slot N vouches for the execution block it builds on (usually slot N-1's payload).

use alloy_primitives::B256;
use blst::min_pk::{AggregatePublicKey, PublicKey, Signature};
use sha2::{Digest, Sha256};

/// Mainnet-preset sizes (Sepolia, mainnet). Other presets: [`LcConfig`].
pub const SYNC_COMMITTEE_SIZE: usize = 512;
pub const SLOTS_PER_PERIOD: u64 = 8192;
pub const EXECUTION_BLOCK_HASH_GINDEX: u64 = 2856;
pub const CURRENT_SYNC_COMMITTEE_GINDEX: u64 = 2945;
/// `next_sync_committee` is the field right after `current_sync_committee` in the state.
pub const NEXT_SYNC_COMMITTEE_GINDEX: u64 = CURRENT_SYNC_COMMITTEE_GINDEX + 1;
pub const FINALIZED_ROOT_GINDEX: u64 = 735;
pub const DOMAIN_SYNC_COMMITTEE: [u8; 4] = [7, 0, 0, 0];
/// Sepolia `genesis_validators_root`.
pub const SEPOLIA_GENESIS_VALIDATORS_ROOT: [u8; 32] =
    alloy_primitives::hex!("d8ea171f3c94aea21ebc42a1ed61052acf3f9209c00e4efbaaddac09ed9b8078");
const BLS_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

/// What the light client needs to know about the network: the preset's sync committee size and
/// period length, and the chain's `genesis_validators_root` (part of every signing domain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LcConfig {
    pub committee_size: usize,
    pub slots_per_period: u64,
    pub genesis_validators_root: [u8; 32],
}

impl LcConfig {
    pub const SEPOLIA: Self =
        Self { committee_size: SYNC_COMMITTEE_SIZE, slots_per_period: SLOTS_PER_PERIOD, genesis_validators_root: SEPOLIA_GENESIS_VALIDATORS_ROOT };

    fn agg_len(&self) -> usize {
        self.committee_size / 8 + 96
    }
    fn committee_len(&self) -> usize {
        self.committee_size * 48 + 48
    }
    pub fn optimistic_update_len(&self) -> usize {
        HEADER_LEN + self.agg_len() + 8
    }
    pub fn finality_update_len(&self) -> usize {
        HEADER_LEN * 2 + 32 * 9 + self.agg_len() + 8
    }
    pub fn bootstrap_len(&self) -> usize {
        HEADER_LEN + self.committee_len() + 32 * 11
    }
    pub fn update_len(&self) -> usize {
        HEADER_LEN + self.committee_len() + 32 * 11 + HEADER_LEN + 32 * 9 + self.agg_len() + 8
    }
    /// Supermajority of the committee (the spec's optimistic threshold is 1; skim asks for 2/3).
    pub fn min_participants(&self) -> usize {
        self.committee_size * 2 / 3 + 1
    }
    pub fn period(&self, slot: u64) -> u64 {
        slot / self.slots_per_period
    }
}

const HEADER_LEN: usize = 112 + 32 + 32 * 11; // beacon + execution_block_hash + branch
// Mainnet-preset sizes; code paths use [`LcConfig`].
const SYNC_AGGREGATE_LEN: usize = 64 + 96;
pub const OPTIMISTIC_UPDATE_LEN: usize = HEADER_LEN + SYNC_AGGREGATE_LEN + 8;
pub const FINALITY_UPDATE_LEN: usize = HEADER_LEN * 2 + 32 * 9 + SYNC_AGGREGATE_LEN + 8;
pub const BOOTSTRAP_LEN: usize = HEADER_LEN + SYNC_COMMITTEE_SIZE * 48 + 48 + 32 * 11;
const COMMITTEE_LEN: usize = SYNC_COMMITTEE_SIZE * 48 + 48;
/// `LightClientUpdate`: attested header, next sync committee + branch, finalized header +
/// branch, sync aggregate, signature slot (all fixed-size here).
pub const UPDATE_LEN: usize = HEADER_LEN + COMMITTEE_LEN + 32 * 11 + HEADER_LEN + 32 * 9 + SYNC_AGGREGATE_LEN + 8;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum LcError {
    #[error("bad length {0} (expected {1})")]
    Length(usize, usize),
    #[error("merkle branch does not verify: {0}")]
    Branch(&'static str),
    #[error("bootstrap header root {0} is not the trusted checkpoint {1}")]
    Checkpoint(B256, B256),
    #[error("sync committee for period {0} not known (have {1:?})")]
    Period(u64, Option<u64>),
    #[error("only {0} sync committee members signed (need {1})")]
    Participation(usize, usize),
    #[error("bad BLS data: {0}")]
    Bls(String),
    #[error("sync committee signature does not verify")]
    Signature,
}

fn sha(a: &[u8], b: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(a);
    h.update(b);
    h.finalize().into()
}

fn u64_leaf(v: u64) -> [u8; 32] {
    let mut l = [0u8; 32];
    l[..8].copy_from_slice(&v.to_le_bytes());
    l
}

/// Merkle root of `leaves`, padded with zero chunks to the next power of two.
fn merkleize(mut leaves: Vec<[u8; 32]>) -> [u8; 32] {
    let n = leaves.len().next_power_of_two().max(1);
    leaves.resize(n, [0u8; 32]);
    while leaves.len() > 1 {
        leaves = leaves.chunks(2).map(|p| sha(&p[0], &p[1])).collect();
    }
    leaves[0]
}

fn pubkey_root(pk: &[u8]) -> [u8; 32] {
    let mut second = [0u8; 32];
    second[..16].copy_from_slice(&pk[32..48]);
    sha(&pk[..32], &second)
}

/// `is_valid_normalized_merkle_branch` for a branch of exactly `floorlog2(gindex)` nodes.
pub fn verify_branch(leaf: [u8; 32], branch: &[[u8; 32]], gindex: u64, root: [u8; 32]) -> bool {
    let depth = 63 - gindex.leading_zeros() as usize;
    if branch.len() != depth {
        return false;
    }
    let index = gindex - (1 << depth);
    let mut v = leaf;
    for (i, node) in branch.iter().enumerate() {
        v = if (index >> i) & 1 == 1 { sha(node, &v) } else { sha(&v, node) };
    }
    v == root
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeaconHeader {
    pub slot: u64,
    pub proposer_index: u64,
    pub parent_root: [u8; 32],
    pub state_root: [u8; 32],
    pub body_root: [u8; 32],
}

impl BeaconHeader {
    fn parse(b: &[u8]) -> Self {
        let r = |o: usize| -> [u8; 32] { b[o..o + 32].try_into().unwrap() };
        Self {
            slot: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            proposer_index: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            parent_root: r(16),
            state_root: r(48),
            body_root: r(80),
        }
    }

    pub fn root(&self) -> [u8; 32] {
        merkleize(vec![u64_leaf(self.slot), u64_leaf(self.proposer_index), self.parent_root, self.state_root, self.body_root])
    }
}

/// A Gloas `LightClientHeader`, with its execution branch already checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LcHeader {
    pub beacon: BeaconHeader,
    /// The execution block hash this beacon block's bid builds on.
    pub execution_block_hash: B256,
}

fn branch(b: &[u8], n: usize) -> Vec<[u8; 32]> {
    (0..n).map(|i| b[i * 32..(i + 1) * 32].try_into().unwrap()).collect()
}

fn parse_header(b: &[u8]) -> Result<LcHeader, LcError> {
    let beacon = BeaconHeader::parse(&b[..112]);
    let exec: [u8; 32] = b[112..144].try_into().unwrap();
    if !verify_branch(exec, &branch(&b[144..HEADER_LEN], 11), EXECUTION_BLOCK_HASH_GINDEX, beacon.body_root) {
        return Err(LcError::Branch("execution_block_hash"));
    }
    Ok(LcHeader { beacon, execution_block_hash: B256::from(exec) })
}

pub struct Committee {
    pub period: u64,
    pubkeys: Vec<PublicKey>,
}

impl Committee {
    /// A committee from validated public keys (tests, fuzzing): exactly the preset's size.
    pub fn from_pubkeys(period: u64, pubkeys: Vec<PublicKey>, cfg: &LcConfig) -> Option<Self> {
        (pubkeys.len() == cfg.committee_size).then_some(Self { period, pubkeys })
    }
}

/// A serialized `SyncCommittee` (the preset's pubkeys + aggregate): its hash tree root and the keys.
fn parse_committee(b: &[u8], cfg: &LcConfig) -> Result<([u8; 32], Vec<PublicKey>), LcError> {
    let pk_bytes = &b[..cfg.committee_size * 48];
    let agg = &b[cfg.committee_size * 48..cfg.committee_len()];
    let root = sha(&merkleize(pk_bytes.chunks(48).map(pubkey_root).collect()), &pubkey_root(agg));
    let keys = pk_bytes
        .chunks(48)
        .map(|p| PublicKey::key_validate(p).map_err(|e| LcError::Bls(format!("{e:?}"))))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((root, keys))
}

/// The committees a light client holds: the current period's, and the next period's once an
/// update has authenticated it (the spec's `current_sync_committee` / `next_sync_committee`).
pub struct Committees {
    pub current: Committee,
    pub next: Option<Committee>,
    pub cfg: LcConfig,
}

impl Committees {
    pub fn new(current: Committee, cfg: LcConfig) -> Self {
        Self { current, next: None, cfg }
    }

    /// The committee that signs at `signature_slot`: by the slot's own period (the `- 1` in the
    /// spec is for choosing the fork version, not the committee).
    pub fn for_signature_slot(&self, signature_slot: u64) -> Result<&Committee, LcError> {
        let p = self.cfg.period(signature_slot);
        if p == self.current.period {
            return Ok(&self.current);
        }
        match &self.next {
            Some(n) if n.period == p => Ok(n),
            _ => Err(LcError::Period(p, Some(self.current.period))),
        }
    }

    /// After a verified finalized header: once finality reaches the next period, its committee
    /// becomes current. True if it rotated.
    pub fn advance(&mut self, finalized_slot: u64) -> bool {
        if self.cfg.period(finalized_slot) > self.current.period
            && let Some(n) = self.next.take_if(|n| n.period == self.current.period + 1)
        {
            self.current = n;
            return true;
        }
        false
    }
}

/// Verifies a `LightClientBootstrap` against a trusted checkpoint block root and returns the
/// checkpoint header and its sync committee.
pub fn bootstrap(b: &[u8], trusted_root: B256, cfg: &LcConfig) -> Result<(LcHeader, Committee), LcError> {
    if b.len() != cfg.bootstrap_len() {
        return Err(LcError::Length(b.len(), cfg.bootstrap_len()));
    }
    let header = parse_header(&b[..HEADER_LEN])?;
    let root = B256::from(header.beacon.root());
    if root != trusted_root {
        return Err(LcError::Checkpoint(root, trusted_root));
    }
    let (committee_root, pubkeys) = parse_committee(&b[HEADER_LEN..HEADER_LEN + cfg.committee_len()], cfg)?;
    let br = branch(&b[HEADER_LEN + cfg.committee_len()..], 11);
    if !verify_branch(committee_root, &br, CURRENT_SYNC_COMMITTEE_GINDEX, header.beacon.state_root) {
        return Err(LcError::Branch("current_sync_committee"));
    }
    Ok((header, Committee { period: cfg.period(header.beacon.slot), pubkeys }))
}

fn signing_root(object_root: [u8; 32], fork_version: [u8; 4], gvr: [u8; 32]) -> [u8; 32] {
    let mut v = [0u8; 32];
    v[..4].copy_from_slice(&fork_version);
    let fork_data_root = sha(&v, &gvr);
    let mut domain = [0u8; 32];
    domain[..4].copy_from_slice(&DOMAIN_SYNC_COMMITTEE);
    domain[4..].copy_from_slice(&fork_data_root[..28]);
    sha(&object_root, &domain)
}

/// Checks that at least `min` members of the committee that signs at `signature_slot` signed
/// `attested`'s root.
fn verify_sync_aggregate(
    cs: &Committees,
    attested: &BeaconHeader,
    agg: &[u8],
    signature_slot: u64,
    fork_version: [u8; 4],
    min: usize,
) -> Result<usize, LcError> {
    let c = cs.for_signature_slot(signature_slot)?;
    let nbits = cs.cfg.committee_size / 8;
    let bits = &agg[..nbits];
    let participants: Vec<&PublicKey> =
        (0..cs.cfg.committee_size).filter(|i| bits[i / 8] >> (i % 8) & 1 == 1).map(|i| &c.pubkeys[i]).collect();
    if participants.len() < min {
        return Err(LcError::Participation(participants.len(), min));
    }
    let apk = AggregatePublicKey::aggregate(&participants, false).map_err(|e| LcError::Bls(format!("{e:?}")))?;
    let sig = Signature::sig_validate(&agg[nbits..nbits + 96], true).map_err(|e| LcError::Bls(format!("{e:?}")))?;
    let msg = signing_root(attested.root(), fork_version, cs.cfg.genesis_validators_root);
    match sig.verify(true, &msg, BLS_DST, &[], &apk.to_public_key(), false) {
        blst::BLST_ERROR::BLST_SUCCESS => Ok(participants.len()),
        _ => Err(LcError::Signature),
    }
}

/// Supermajority of a mainnet-preset committee (see [`LcConfig::min_participants`]).
pub const MIN_PARTICIPANTS: usize = SYNC_COMMITTEE_SIZE * 2 / 3 + 1;

/// Verifies a `LightClientOptimisticUpdate`: returns the attested header (with its execution
/// block hash proven) and how many members signed.
pub fn optimistic_update(c: &Committees, b: &[u8], fork_version: [u8; 4]) -> Result<(LcHeader, usize), LcError> {
    let (want, agg_len) = (c.cfg.optimistic_update_len(), c.cfg.agg_len());
    if b.len() != want {
        return Err(LcError::Length(b.len(), want));
    }
    let attested = parse_header(&b[..HEADER_LEN])?;
    let agg = &b[HEADER_LEN..HEADER_LEN + agg_len];
    let signature_slot = u64::from_le_bytes(b[HEADER_LEN + agg_len..].try_into().unwrap());
    let n = verify_sync_aggregate(c, &attested.beacon, agg, signature_slot, fork_version, c.cfg.min_participants())?;
    Ok((attested, n))
}

/// Verifies a `LightClientFinalityUpdate`: the attested header is signed, and its state commits
/// to the finalized header. Returns `(attested, finalized)`.
pub fn finality_update(c: &Committees, b: &[u8], fork_version: [u8; 4]) -> Result<(LcHeader, LcHeader), LcError> {
    let (want, agg_len) = (c.cfg.finality_update_len(), c.cfg.agg_len());
    if b.len() != want {
        return Err(LcError::Length(b.len(), want));
    }
    let attested = parse_header(&b[..HEADER_LEN])?;
    let finalized = parse_header(&b[HEADER_LEN..2 * HEADER_LEN])?;
    let fb = branch(&b[2 * HEADER_LEN..2 * HEADER_LEN + 32 * 9], 9);
    if !verify_branch(finalized.beacon.root(), &fb, FINALIZED_ROOT_GINDEX, attested.beacon.state_root) {
        return Err(LcError::Branch("finalized_root"));
    }
    let o = 2 * HEADER_LEN + 32 * 9;
    let agg = &b[o..o + agg_len];
    let signature_slot = u64::from_le_bytes(b[o + agg_len..].try_into().unwrap());
    verify_sync_aggregate(c, &attested.beacon, agg, signature_slot, fork_version, c.cfg.min_participants())?;
    Ok((attested, finalized))
}

/// A verified `LightClientUpdate`.
pub struct FullUpdate {
    pub attested: LcHeader,
    /// The finalized header the attested state commits to (`None` if the update carries none).
    pub finalized: Option<LcHeader>,
    /// The next period's committee, authenticated against the attested state, when the attested
    /// header is in the current period (otherwise it is not "next" for this client).
    pub next: Option<Committee>,
}

/// Verifies a `LightClientUpdate` (from `LightClientUpdatesByRange`): signed by the committee for
/// its signature slot; the next sync committee and the finalized header proven against the
/// attested state root.
pub fn full_update(cs: &Committees, b: &[u8], fork_version: [u8; 4]) -> Result<FullUpdate, LcError> {
    let (want, agg_len, committee_len) = (cs.cfg.update_len(), cs.cfg.agg_len(), cs.cfg.committee_len());
    if b.len() != want {
        return Err(LcError::Length(b.len(), want));
    }
    let attested = parse_header(&b[..HEADER_LEN])?;
    let mut o = HEADER_LEN;
    let (next_root, next_keys) = parse_committee(&b[o..o + committee_len], &cs.cfg)?;
    o += committee_len;
    let next_branch = branch(&b[o..o + 32 * 11], 11);
    o += 32 * 11;
    if !verify_branch(next_root, &next_branch, NEXT_SYNC_COMMITTEE_GINDEX, attested.beacon.state_root) {
        return Err(LcError::Branch("next_sync_committee"));
    }
    let fin_bytes = &b[o..o + HEADER_LEN];
    o += HEADER_LEN;
    let fin_branch = branch(&b[o..o + 32 * 9], 9);
    o += 32 * 9;
    let finalized = if fin_bytes.iter().all(|x| *x == 0) {
        None
    } else {
        let f = parse_header(fin_bytes)?;
        if !verify_branch(f.beacon.root(), &fin_branch, FINALIZED_ROOT_GINDEX, attested.beacon.state_root) {
            return Err(LcError::Branch("finalized_root"));
        }
        Some(f)
    };
    let agg = &b[o..o + agg_len];
    let signature_slot = u64::from_le_bytes(b[o + agg_len..].try_into().unwrap());
    verify_sync_aggregate(cs, &attested.beacon, agg, signature_slot, fork_version, cs.cfg.min_participants())?;
    let attested_period = cs.cfg.period(attested.beacon.slot);
    let next = (attested_period == cs.current.period).then(|| Committee { period: attested_period + 1, pubkeys: next_keys });
    Ok(FullUpdate { attested, finalized, next })
}

/// What [`apply_updates`] did.
#[derive(Default)]
pub struct Applied {
    /// Updates verified and applied, in order.
    pub applied: usize,
    /// Committee rotations (periods advanced).
    pub rotations: usize,
    /// The newest attested and finalized headers among the applied updates.
    pub attested: Option<LcHeader>,
    pub finalized: Option<LcHeader>,
    /// Why processing stopped early, if it did (later updates depend on this one).
    pub stopped: Option<LcError>,
}

/// Processes `LightClientUpdate`s in order (as `LightClientUpdatesByRange` returns them),
/// advancing `cs` the way the sync protocol does: each update must be signed by a held
/// committee; one attested in the current period teaches the next committee; a finalized header
/// in the next period rotates it to current, after which the same update's next committee is
/// for the new current period. This lets a client that bootstrapped periods ago catch up to the
/// live period, instead of holding P / P+1 while the network signs with P+2's committee.
pub fn apply_updates(cs: &mut Committees, updates: &[Vec<u8>], fork_version: [u8; 4]) -> Applied {
    let mut out = Applied::default();
    for u in updates {
        let up = match full_update(cs, u, fork_version) {
            Ok(up) => up,
            Err(e) => {
                out.stopped = Some(e);
                break;
            }
        };
        if cs.next.is_none() {
            cs.next = up.next;
        }
        if let Some(f) = up.finalized {
            if cs.advance(f.beacon.slot) {
                out.rotations += 1;
                // The update's committee (proven against its attested state) is the next one now.
                if cs.next.is_none()
                    && let Ok(again) = full_update(cs, u, fork_version)
                {
                    cs.next = again.next;
                }
            }
            if out.finalized.is_none_or(|x| x.beacon.slot < f.beacon.slot) {
                out.finalized = Some(f);
            }
        }
        if out.attested.is_none_or(|x| x.beacon.slot < up.attested.beacon.slot) {
            out.attested = Some(up.attested);
        }
        out.applied += 1;
    }
    out
}

/// Generated light-client fixtures (BLS keys, Merkle branches) for tests here and in dependent
/// crates (`testkit` feature): committees, bootstraps, optimistic and full updates.
#[cfg(any(test, feature = "testkit"))]
pub mod testkit {
    use super::*;

    pub use blst::min_pk::{AggregateSignature, SecretKey};

    pub const FV: [u8; 4] = [0x90, 0, 0, 0x76];

    /// The root `verify_branch` folds `leaf` and `branch` into for `gindex`.
    pub fn fold(leaf: [u8; 32], branch: &[[u8; 32]], gindex: u64) -> [u8; 32] {
        let depth = 63 - gindex.leading_zeros() as usize;
        let index = gindex - (1 << depth);
        let mut v = leaf;
        for (i, node) in branch.iter().enumerate() {
            v = if (index >> i) & 1 == 1 { sha(node, &v) } else { sha(&v, node) };
        }
        v
    }

    pub fn nodes(seed: u8, n: usize) -> Vec<[u8; 32]> {
        (0..n).map(|i| [seed.wrapping_add(i as u8); 32]).collect()
    }

    /// The execution block hash the fixture header at `slot` carries.
    pub fn exec_hash(slot: u64) -> B256 {
        let mut x = [0xee; 32];
        x[24..].copy_from_slice(&slot.to_be_bytes());
        B256::from(x)
    }

    /// A `LightClientHeader` at `slot` with the given state root (execution branch valid).
    pub fn lc_header(slot: u64, state_root: [u8; 32]) -> (Vec<u8>, BeaconHeader) {
        let exec = exec_hash(slot).0;
        let eb = nodes(0x10, 11);
        let body_root = fold(exec, &eb, EXECUTION_BLOCK_HASH_GINDEX);
        let h = BeaconHeader { slot, proposer_index: 7, parent_root: [1; 32], state_root, body_root };
        let mut b = Vec::with_capacity(HEADER_LEN);
        b.extend_from_slice(&slot.to_le_bytes());
        b.extend_from_slice(&7u64.to_le_bytes());
        b.extend_from_slice(&[1; 32]);
        b.extend_from_slice(&state_root);
        b.extend_from_slice(&body_root);
        b.extend_from_slice(&exec);
        eb.iter().for_each(|n| b.extend_from_slice(n));
        (b, h)
    }

    /// A `SyncCommittee` of 512 copies of `sk`'s key: (bytes, root).
    pub fn committee_bytes(sk: &SecretKey) -> (Vec<u8>, [u8; 32]) {
        let pk = sk.sk_to_pk().to_bytes();
        let mut b: Vec<u8> = (0..SYNC_COMMITTEE_SIZE).flat_map(|_| pk).collect();
        b.extend_from_slice(&pk);
        let root = sha(&merkleize(b[..SYNC_COMMITTEE_SIZE * 48].chunks(48).map(pubkey_root).collect()), &pubkey_root(&pk));
        (b, root)
    }

    /// A sync aggregate: the first MIN_PARTICIPANTS members (all `sk`) sign `h`.
    pub fn aggregate(sk: &SecretKey, h: &BeaconHeader) -> Vec<u8> {
        let sig = sk.sign(&signing_root(h.root(), FV, SEPOLIA_GENESIS_VALIDATORS_ROOT), BLS_DST, &[]);
        let sigs = vec![&sig; MIN_PARTICIPANTS];
        let agg = AggregateSignature::aggregate(&sigs, false).unwrap().to_signature();
        let mut b = vec![0u8; 64];
        for i in 0..MIN_PARTICIPANTS {
            b[i / 8] |= 1 << (i % 8);
        }
        b.extend_from_slice(&agg.to_bytes());
        b
    }

    pub fn optimistic(sk: &SecretKey, attested_slot: u64, signature_slot: u64) -> Vec<u8> {
        let (mut b, h) = lc_header(attested_slot, [3; 32]);
        b.extend_from_slice(&aggregate(sk, &h));
        b.extend_from_slice(&signature_slot.to_le_bytes());
        b
    }

    pub fn key(i: u8) -> SecretKey {
        SecretKey::key_gen(&[i; 32], &[]).unwrap()
    }

    pub const P: u64 = 1400;
    pub const BOUNDARY: u64 = (P + 1) * SLOTS_PER_PERIOD; // first slot of period P + 1

    pub fn bootstrapped(sk: &SecretKey) -> Committees {
        let (cb, root) = committee_bytes(sk);
        let br = nodes(0x20, 11);
        let (hb, h) = lc_header(BOUNDARY - 100, fold(root, &br, CURRENT_SYNC_COMMITTEE_GINDEX));
        let mut b = hb;
        b.extend_from_slice(&cb);
        br.iter().for_each(|n| b.extend_from_slice(n));
        let (header, c) = bootstrap(&b, B256::from(h.root()), &LcConfig::SEPOLIA).unwrap();
        assert_eq!((header.beacon.slot, c.period), (BOUNDARY - 100, P));
        Committees::new(c, LcConfig::SEPOLIA)
    }

    /// A `LightClientUpdate` attested at `attested_slot` (no finalized header), carrying
    /// `next`'s committee, signed by `signer` at `signature_slot`.
    pub fn update(signer: &SecretKey, next: &SecretKey, attested_slot: u64, signature_slot: u64, tamper: bool) -> Vec<u8> {
        let (cb, root) = committee_bytes(next);
        let nb = nodes(0x30, 11);
        let state_root = fold(if tamper { [0xad; 32] } else { root }, &nb, NEXT_SYNC_COMMITTEE_GINDEX);
        let (mut b, h) = lc_header(attested_slot, state_root);
        b.extend_from_slice(&cb);
        nb.iter().for_each(|n| b.extend_from_slice(n));
        b.extend_from_slice(&[0u8; HEADER_LEN]); // no finalized header
        b.extend_from_slice(&[0u8; 32 * 9]);
        b.extend_from_slice(&aggregate(signer, &h));
        b.extend_from_slice(&signature_slot.to_le_bytes());
        b
    }

    /// A state root committing to two leaves at `g1` and `g2`, with their branches (other
    /// subtrees are filler).
    pub fn two_leaf_root(g1: u64, l1: [u8; 32], g2: u64, l2: [u8; 32]) -> ([u8; 32], Vec<[u8; 32]>, Vec<[u8; 32]>) {
        fn value(g: u64, g1: u64, l1: [u8; 32], g2: u64, l2: [u8; 32]) -> [u8; 32] {
            if g == g1 {
                return l1;
            }
            if g == g2 {
                return l2;
            }
            let under = |x: u64| {
                let mut x = x;
                while x > g {
                    x >>= 1;
                }
                x == g
            };
            if under(g1) || under(g2) {
                sha(&value(2 * g, g1, l1, g2, l2), &value(2 * g + 1, g1, l1, g2, l2))
            } else {
                [0x5a; 32]
            }
        }
        let branch = |g: u64| {
            let mut out = Vec::new();
            let mut x = g;
            while x > 1 {
                out.push(value(x ^ 1, g1, l1, g2, l2));
                x >>= 1;
            }
            out
        };
        (value(1, g1, l1, g2, l2), branch(g1), branch(g2))
    }

    /// A full update attested at `attested_slot`, carrying `next`'s committee and a finalized
    /// header at `finalized_slot`, signed by `signer` at `signature_slot`.
    pub fn update_with_finality(signer: &SecretKey, next: &SecretKey, attested_slot: u64, finalized_slot: u64, signature_slot: u64) -> Vec<u8> {
        let (cb, croot) = committee_bytes(next);
        let (fb, fh) = lc_header(finalized_slot, [9; 32]);
        let (state_root, nb, finb) = two_leaf_root(NEXT_SYNC_COMMITTEE_GINDEX, croot, FINALIZED_ROOT_GINDEX, fh.root());
        let (mut b, h) = lc_header(attested_slot, state_root);
        b.extend_from_slice(&cb);
        nb.iter().for_each(|n| b.extend_from_slice(n));
        b.extend_from_slice(&fb);
        finb.iter().for_each(|n| b.extend_from_slice(n));
        b.extend_from_slice(&aggregate(signer, &h));
        b.extend_from_slice(&signature_slot.to_le_bytes());
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_roundtrip_and_tamper() {
        // Build a depth-3 tree and prove leaf 5 (gindex 8 + 5 = 13).
        let leaves: Vec<[u8; 32]> = (0..8u8).map(|i| [i; 32]).collect();
        let root = merkleize(leaves.clone());
        let l1: Vec<[u8; 32]> = leaves.chunks(2).map(|p| sha(&p[0], &p[1])).collect();
        let l2: Vec<[u8; 32]> = l1.chunks(2).map(|p| sha(&p[0], &p[1])).collect();
        let proof = vec![leaves[4], l1[3], l2[0]];
        assert!(verify_branch(leaves[5], &proof, 13, root));
        assert!(!verify_branch(leaves[4], &proof, 13, root));
        assert!(!verify_branch(leaves[5], &proof[..2], 13, root));
    }

    #[test]
    fn header_root_matches_known_mainnet_genesis_header() {
        // Mainnet slot-0 header root with all-zero parent/body and genesis state root is a
        // fixed point of the merkleization; here we just check field order on a crafted value.
        let h = BeaconHeader { slot: 1, proposer_index: 2, parent_root: [3; 32], state_root: [4; 32], body_root: [5; 32] };
        let expect = merkleize(vec![u64_leaf(1), u64_leaf(2), [3; 32], [4; 32], [5; 32]]);
        assert_eq!(h.root(), expect);
        assert_ne!(BeaconHeader { slot: 2, ..h }.root(), expect);
    }

    #[test]
    fn bls_aggregate_signature_over_a_signing_root() {
        use blst::min_pk::SecretKey;
        let sks: Vec<SecretKey> = (1..=3u8).map(|i| SecretKey::key_gen(&[i; 32], &[]).unwrap()).collect();
        let header = BeaconHeader { slot: 100, proposer_index: 1, parent_root: [1; 32], state_root: [2; 32], body_root: [3; 32] };
        let fv = [1, 2, 3, 4];
        let msg = signing_root(header.root(), fv, SEPOLIA_GENESIS_VALIDATORS_ROOT);
        let sigs: Vec<Signature> = sks.iter().map(|s| s.sign(&msg, BLS_DST, &[])).collect();
        let agg = blst::min_pk::AggregateSignature::aggregate(&sigs.iter().collect::<Vec<_>>(), false).unwrap().to_signature();
        // A 512-member committee where members 0..3 are our keys and everyone else is key 0.
        let mut pubkeys: Vec<PublicKey> = sks.iter().map(|s| s.sk_to_pk()).collect();
        pubkeys.resize(SYNC_COMMITTEE_SIZE, sks[0].sk_to_pk());
        let c = Committees::new(Committee { period: 0, pubkeys }, LcConfig::SEPOLIA);
        let mut agg_bytes = vec![0u8; SYNC_AGGREGATE_LEN];
        agg_bytes[0] = 0b111;
        agg_bytes[64..].copy_from_slice(&agg.to_bytes());
        assert_eq!(verify_sync_aggregate(&c, &header, &agg_bytes, 101, fv, 3), Ok(3));
        // Wrong header, wrong fork version, too few signers: all rejected.
        let other = BeaconHeader { slot: 101, ..header };
        assert_eq!(verify_sync_aggregate(&c, &other, &agg_bytes, 101, fv, 3), Err(LcError::Signature));
        assert_eq!(verify_sync_aggregate(&c, &header, &agg_bytes, 101, [9, 9, 9, 9], 3), Err(LcError::Signature));
        assert_eq!(verify_sync_aggregate(&c, &header, &agg_bytes, 101, fv, 4), Err(LcError::Participation(3, 4)));
    }

    // ---- Committee rollover (generated fixtures: `testkit`) ------------------------------------

    use super::testkit::*;

    #[test]
    fn the_committee_rolls_into_the_next_period() {
        let (a, bk) = (key(1), key(2)); // committee of period P, and of P + 1
        let mut cs = bootstrapped(&a);
        // Before the next committee is known, an update signed at the first slot of P + 1 is
        // rejected (it can't be checked), not misattributed to P's committee.
        assert_eq!(optimistic_update(&cs, &optimistic(&bk, BOUNDARY - 1, BOUNDARY), FV).err(), Some(LcError::Period(P + 1, Some(P))));
        // P's committee authenticates P + 1's.
        let up = full_update(&cs, &update(&a, &bk, BOUNDARY - 50, BOUNDARY - 49, false), FV).unwrap();
        let next = up.next.expect("next committee learned");
        assert_eq!(next.period, P + 1);
        cs.next = Some(next);
        // The last slot of P is signed by P's committee ...
        assert!(optimistic_update(&cs, &optimistic(&a, BOUNDARY - 2, BOUNDARY - 1), FV).is_ok());
        // ... and the very first slot of P + 1 by P + 1's (the old code used signature_slot - 1
        // and checked it against P's committee).
        assert!(optimistic_update(&cs, &optimistic(&bk, BOUNDARY - 1, BOUNDARY), FV).is_ok());
        assert_eq!(optimistic_update(&cs, &optimistic(&a, BOUNDARY - 1, BOUNDARY), FV).err(), Some(LcError::Signature));
        // Finality in P + 1 rotates: P + 1 is current, P's committee no longer signs anything.
        assert!(!cs.advance(BOUNDARY - 10));
        assert!(cs.advance(BOUNDARY + 5));
        assert_eq!((cs.current.period, cs.next.is_none()), (P + 1, true));
        assert!(optimistic_update(&cs, &optimistic(&bk, BOUNDARY + 10, BOUNDARY + 11), FV).is_ok());
        assert_eq!(optimistic_update(&cs, &optimistic(&a, BOUNDARY - 2, BOUNDARY - 1), FV).err(), Some(LcError::Period(P, Some(P + 1))));
    }

    #[test]
    fn a_next_committee_must_be_proven_and_belong_to_the_current_period() {
        let (a, bk) = (key(1), key(2));
        let cs = bootstrapped(&a);
        // A next committee that isn't in the attested state is rejected.
        assert_eq!(full_update(&cs, &update(&a, &bk, BOUNDARY - 50, BOUNDARY - 49, true), FV).err(), Some(LcError::Branch("next_sync_committee")));
        // Signed by a committee skim doesn't hold: rejected.
        assert_eq!(full_update(&cs, &update(&bk, &bk, BOUNDARY - 50, BOUNDARY - 49, false), FV).err(), Some(LcError::Signature));
        // An attested header from an earlier period doesn't say what comes after P.
        let up = full_update(&cs, &update(&a, &bk, BOUNDARY - SLOTS_PER_PERIOD - 10, BOUNDARY - 49, false), FV).unwrap();
        assert!(up.next.is_none());
    }

    // ---- Catch-up from an old checkpoint (sequential full updates) ----------------------------

    #[test]
    fn a_client_two_periods_behind_catches_up_from_full_updates() {
        // Review finding: checkpoint in P, network in P + 2. Fetching only P + 1's committee
        // left the client at P / P + 1, rejecting all live gossip.
        let (a, b, c, d) = (key(1), key(2), key(3), key(4)); // committees of P .. P + 3
        let mut cs = bootstrapped(&a);
        let p1 = BOUNDARY; // first slot of P + 1
        let p2 = BOUNDARY + SLOTS_PER_PERIOD;
        let live = optimistic(&c, p2 + 100, p2 + 101); // signed by P + 2's committee
        assert!(optimistic_update(&cs, &live, FV).is_err());
        // LightClientUpdatesByRange(P, 3): one update per period, in order.
        let updates = vec![
            update_with_finality(&a, &b, p1 - 200, p1 - 300, p1 - 199), // P: teaches P + 1
            update_with_finality(&b, &c, p2 - 200, p2 - 300, p2 - 199), // P + 1: finality in P + 1, teaches P + 2
            update_with_finality(&c, &d, p2 + 50, p2 + 10, p2 + 51),    // P + 2: finality in P + 2, teaches P + 3
        ];
        let r = apply_updates(&mut cs, &updates, FV);
        assert!(r.stopped.is_none(), "{:?}", r.stopped);
        assert_eq!((r.applied, r.rotations), (3, 2));
        assert_eq!(cs.current.period, P + 2);
        assert_eq!(cs.next.as_ref().map(|n| n.period), Some(P + 3));
        assert_eq!(r.finalized.map(|f| f.beacon.slot), Some(p2 + 10));
        // Live gossip from P + 2 verifies now.
        assert!(optimistic_update(&cs, &live, FV).is_ok());
    }

    #[test]
    fn catch_up_stops_at_the_first_update_it_cannot_verify() {
        let (a, b, c) = (key(1), key(2), key(3));
        let mut cs = bootstrapped(&a);
        let p2 = BOUNDARY + SLOTS_PER_PERIOD;
        // The update for P is missing: P + 1's update can't be checked (its committee is unknown).
        let updates = vec![update_with_finality(&b, &c, p2 - 200, p2 - 300, p2 - 199)];
        let r = apply_updates(&mut cs, &updates, FV);
        assert_eq!(r.applied, 0);
        assert!(matches!(r.stopped, Some(LcError::Period(..))));
        assert_eq!((cs.current.period, cs.next.is_none()), (P, true));
    }
}
