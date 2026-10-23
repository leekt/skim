//! Hand-written SSZ offset parsing for the few Gloas gossip containers skim needs.
//!
//! Gloas (EIP-7688) turns `ExecutionPayload`, `ExecutionPayloadEnvelope`, `ExecutionRequests`
//! and `BeaconBlockBody` into `ProgressiveContainer`s and most lists into `ProgressiveList`s.
//! EIP-7495 / EIP-7916 change only *merkleization* (`hash_tree_root`); the byte serialization of
//! a `ProgressiveContainer` is that of a `Container` with the same active fields in order, and a
//! `ProgressiveList[T]` serializes like `List[T, N]` (no length prefix, offsets for variable-size
//! items). So a plain offset parser is enough here; only roots would need the new rules.
//!
//! Layout references: consensus-specs `specs/gloas/beacon-chain.md` (ExecutionPayload ~L940,
//! ExecutionPayloadEnvelope ~L777, BeaconBlockBody ~L829, ExecutionPayloadBid ~L749).

use alloy_primitives::{Address, B256, Bloom, Bytes, U256};

use crate::network::BodyLayout;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SszError {
    #[error("ssz: buffer too short ({what}: need {need}, have {have})")]
    Short { what: &'static str, need: usize, have: usize },
    #[error("ssz: bad offset in {0}")]
    BadOffset(&'static str),
    #[error("ssz: {0}")]
    Invalid(&'static str),
}

type Result<T> = std::result::Result<T, SszError>;

fn need(b: &[u8], n: usize, what: &'static str) -> Result<()> {
    if b.len() < n { Err(SszError::Short { what, need: n, have: b.len() }) } else { Ok(()) }
}

fn u32_at(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn b256_at(b: &[u8], at: usize) -> B256 {
    B256::from_slice(&b[at..at + 32])
}

/// Splits the variable-size fields of a container. `offset_pos` lists where each offset lives in
/// the fixed part (in field order); returns one slice per variable field.
fn var_fields<'a>(
    b: &'a [u8],
    fixed_len: usize,
    offset_pos: &[usize],
    what: &'static str,
) -> Result<Vec<&'a [u8]>> {
    need(b, fixed_len, what)?;
    let offs: Vec<usize> = offset_pos.iter().map(|&p| u32_at(b, p)).collect();
    if offs.first().is_some_and(|&o| o != fixed_len) {
        return Err(SszError::BadOffset(what));
    }
    let mut out = Vec::with_capacity(offs.len());
    for (i, &start) in offs.iter().enumerate() {
        let end = offs.get(i + 1).copied().unwrap_or(b.len());
        if start > end || end > b.len() {
            return Err(SszError::BadOffset(what));
        }
        out.push(&b[start..end]);
    }
    Ok(out)
}

/// Items of a list of variable-size elements (offset table, then data).
pub fn var_list<'a>(b: &'a [u8], what: &'static str) -> Result<Vec<&'a [u8]>> {
    if b.is_empty() {
        return Ok(Vec::new());
    }
    need(b, 4, what)?;
    let first = u32_at(b, 0);
    if !first.is_multiple_of(4) || first == 0 || first > b.len() {
        return Err(SszError::BadOffset(what));
    }
    let n = first / 4;
    let pos: Vec<usize> = (0..n).map(|i| i * 4).collect();
    var_fields(b, first, &pos, what)
}

/// Signed wrapper `{ message: <variable>, signature: BLSSignature }` -> (message, signature).
fn signed<'a>(b: &'a [u8], what: &'static str) -> Result<(&'a [u8], &'a [u8])> {
    let f = var_fields(b, 100, &[0], what)?;
    Ok((f[0], &b[4..100]))
}

/// One EL withdrawal (SSZ `Withdrawal`, 44 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Withdrawal {
    pub index: u64,
    pub validator_index: u64,
    pub address: Address,
    pub amount_gwei: u64,
}

/// The fields of a Gloas `ExecutionPayload`, borrowing variable data from the message.
#[derive(Debug, Clone)]
pub struct ExecutionPayload<'a> {
    pub parent_hash: B256,
    pub fee_recipient: Address,
    pub state_root: B256,
    pub receipts_root: B256,
    pub logs_bloom: Bloom,
    pub prev_randao: B256,
    pub block_number: u64,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub timestamp: u64,
    pub extra_data: &'a [u8],
    pub base_fee_per_gas: U256,
    pub block_hash: B256,
    /// EIP-2718 encoded transactions, as on the wire.
    pub transactions: Vec<&'a [u8]>,
    pub withdrawals: Vec<Withdrawal>,
    pub blob_gas_used: u64,
    pub excess_blob_gas: u64,
    /// RLP-encoded EIP-7928 block access list, exactly as committed to by the EL header.
    pub block_access_list: &'a [u8],
    pub slot_number: u64,
}

/// Fixed part of the Gloas `ExecutionPayload` (19 fields, 5 of them variable).
pub const PAYLOAD_FIXED_LEN: usize = 540;

impl<'a> ExecutionPayload<'a> {
    pub fn parse(b: &'a [u8]) -> Result<Self> {
        // Offsets of extra_data, transactions, withdrawals, block_access_list.
        let v = var_fields(b, PAYLOAD_FIXED_LEN, &[436, 504, 508, 528], "ExecutionPayload")?;
        if v[0].len() > 32 {
            return Err(SszError::Invalid("extra_data longer than 32 bytes"));
        }
        let wd = v[2];
        if wd.len() % 44 != 0 {
            return Err(SszError::Invalid("withdrawals length not a multiple of 44"));
        }
        let withdrawals = wd
            .chunks_exact(44)
            .map(|w| Withdrawal {
                index: u64_at(w, 0),
                validator_index: u64_at(w, 8),
                address: Address::from_slice(&w[16..36]),
                amount_gwei: u64_at(w, 36),
            })
            .collect();
        Ok(Self {
            parent_hash: b256_at(b, 0),
            fee_recipient: Address::from_slice(&b[32..52]),
            state_root: b256_at(b, 52),
            receipts_root: b256_at(b, 84),
            logs_bloom: Bloom::from_slice(&b[116..372]),
            prev_randao: b256_at(b, 372),
            block_number: u64_at(b, 404),
            gas_limit: u64_at(b, 412),
            gas_used: u64_at(b, 420),
            timestamp: u64_at(b, 428),
            extra_data: v[0],
            base_fee_per_gas: U256::from_le_slice(&b[440..472]),
            block_hash: b256_at(b, 472),
            transactions: var_list(v[1], "Transactions")?,
            withdrawals,
            blob_gas_used: u64_at(b, 512),
            excess_blob_gas: u64_at(b, 520),
            block_access_list: v[3],
            slot_number: u64_at(b, 532),
        })
    }
}

/// `ExecutionRequests`: one SSZ list per request type, in type order (0x00 deposits,
/// 0x01 withdrawals, 0x02 consolidations, and since EIP-8282 0x03 builder deposits, 0x04 builder
/// exits). All fields are variable-size, so the field count is read from the first offset; this
/// accepts both the 3-field and the 5-field layouts.
#[derive(Debug, Clone, Default)]
pub struct ExecutionRequests<'a> {
    /// Serialized list per request type (index == type byte).
    pub lists: Vec<&'a [u8]>,
}

impl<'a> ExecutionRequests<'a> {
    pub fn parse(b: &'a [u8]) -> Result<Self> {
        need(b, 4, "ExecutionRequests")?;
        let first = u32_at(b, 0);
        if !first.is_multiple_of(4) || first == 0 || first > 64 {
            return Err(SszError::BadOffset("ExecutionRequests"));
        }
        let pos: Vec<usize> = (0..first / 4).map(|i| i * 4).collect();
        Ok(Self { lists: var_fields(b, first, &pos, "ExecutionRequests")? })
    }

    /// EIP-7685 `requests_hash`: sha256 over sha256(type ++ data) of each non-empty list
    /// (consensus-specs `get_execution_requests_list`).
    pub fn requests_hash(&self) -> B256 {
        use sha2::{Digest, Sha256};
        let mut outer = Sha256::new();
        for (ty, data) in self.lists.iter().enumerate() {
            if data.is_empty() {
                continue;
            }
            let mut h = Sha256::new();
            h.update([ty as u8]);
            h.update(data);
            outer.update(h.finalize());
        }
        B256::from_slice(&outer.finalize())
    }
}

/// A decoded `SignedExecutionPayloadEnvelope` (the `execution_payload` gossip message).
#[derive(Debug, Clone)]
pub struct Envelope<'a> {
    pub payload: ExecutionPayload<'a>,
    pub execution_requests: ExecutionRequests<'a>,
    pub builder_index: u64,
    pub beacon_block_root: B256,
    pub parent_beacon_block_root: B256,
    pub signature: &'a [u8],
}

impl<'a> Envelope<'a> {
    /// Parses the SSZ bytes of a `SignedExecutionPayloadEnvelope` (already snappy-decompressed).
    pub fn parse(b: &'a [u8]) -> Result<Self> {
        let (msg, signature) = signed(b, "SignedExecutionPayloadEnvelope")?;
        // payload (offset), execution_requests (offset), builder_index, two roots.
        let v = var_fields(msg, 80, &[0, 4], "ExecutionPayloadEnvelope")?;
        Ok(Self {
            payload: ExecutionPayload::parse(v[0])?,
            execution_requests: ExecutionRequests::parse(v[1])?,
            builder_index: u64_at(msg, 8),
            beacon_block_root: b256_at(msg, 16),
            parent_beacon_block_root: b256_at(msg, 48),
            signature,
        })
    }
}

/// What skim needs from a Gloas `SignedBeaconBlock`: its slot, parent and the payload bid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockInfo {
    pub slot: u64,
    pub proposer_index: u64,
    pub parent_root: B256,
    pub state_root: B256,
    /// `body.signed_execution_payload_bid.message`, if the body parsed as Gloas.
    pub bid: Option<BidInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BidInfo {
    pub parent_block_hash: B256,
    pub parent_block_root: B256,
    pub block_hash: B256,
    pub builder_index: u64,
    pub slot: u64,
    pub value_gwei: u64,
}

/// Where the `BeaconBlockBody` fixed part keeps the first list offset (`proposer_slashings`)
/// and the `signed_execution_payload_bid` offset.
///
/// Gloas: randao_reveal 96 + eth1_data 72 + graffiti 32, then 5 list offsets + sync_aggregate
/// 160 + bls_to_execution_changes offset.
/// Heze (experimental, EIP-8015): no `eth1_data`, no `deposits` offset. The bid gains
/// `inclusion_list_bits` (EIP-7805) after the fields read here.
const fn body_offsets(layout: BodyLayout) -> (usize, usize) {
    match layout {
        BodyLayout::Gloas => (96 + 72 + 32, 96 + 72 + 32 + 5 * 4 + 160 + 4),
        BodyLayout::Heze => (96 + 32, 96 + 32 + 4 * 4 + 160 + 4),
    }
}

impl BlockInfo {
    /// A block of the network this process follows (its layout from the block's slot).
    pub fn parse(b: &[u8]) -> Result<Self> {
        Self::parse_with(b, |slot| crate::network::current().body_layout_at(slot))
    }

    pub fn parse_with(b: &[u8], layout: impl Fn(u64) -> BodyLayout) -> Result<Self> {
        let (msg, _) = signed(b, "SignedBeaconBlock")?;
        let v = var_fields(msg, 84, &[80], "BeaconBlock")?;
        let body = v[0];
        let slot = u64_at(msg, 0);
        Ok(Self {
            slot,
            proposer_index: u64_at(msg, 8),
            parent_root: b256_at(msg, 16),
            state_root: b256_at(msg, 48),
            bid: Self::bid(body, layout(slot)).ok(),
        })
    }

    fn bid(body: &[u8], layout: BodyLayout) -> Result<BidInfo> {
        let (first, bid_pos) = body_offsets(layout);
        need(body, bid_pos + 8, "BeaconBlockBody")?;
        let fixed = u32_at(body, first); // first offset (proposer_slashings) == fixed length
        if fixed < bid_pos + 8 || fixed > body.len() {
            return Err(SszError::BadOffset("BeaconBlockBody"));
        }
        let start = u32_at(body, bid_pos);
        let end = u32_at(body, bid_pos + 4);
        let end = if end >= start && end <= body.len() { end } else { body.len() };
        // A variable field starts after the fixed part. (Review: an offset into the fixed part
        // was read as the bid.)
        if start < fixed || start > end {
            return Err(SszError::BadOffset("signed_execution_payload_bid"));
        }
        let (m, _) = signed(&body[start..end], "SignedExecutionPayloadBid")?;
        // parent_block_hash, parent_block_root, block_hash, prev_randao, fee_recipient,
        // gas_limit, builder_index, slot, value, ...
        need(m, 180, "ExecutionPayloadBid")?;
        Ok(BidInfo {
            parent_block_hash: b256_at(m, 0),
            parent_block_root: b256_at(m, 32),
            block_hash: b256_at(m, 64),
            builder_index: u64_at(m, 96 + 32 + 20 + 8),
            slot: u64_at(m, 96 + 32 + 20 + 16),
            value_gwei: u64_at(m, 96 + 32 + 20 + 24),
        })
    }
}

/// Hex helper for logs and JSON.
pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

pub fn to_bytes(b: &[u8]) -> Bytes {
    Bytes::copy_from_slice(b)
}
