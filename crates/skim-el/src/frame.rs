//! Transactions and receipts as skim receives them from peers: Ethereum's, plus EIP-8141 frame
//! transactions (Hegota, experimental: docs/guide/hegota.md).
//!
//! reth's Ethereum types can't decode a frame transaction (type `0x06`) or its receipt, and a
//! message that doesn't decode ends the peer's session. On a network with frame transactions
//! that would cost skim a peer for every block body, receipt list or broadcast that carries one.
//! So skim gives reth its own types ([`SkimPrimitives`]): [`SignedTx`] and [`Receipt`], each the
//! Ethereum one or the frame one. Off Hegota networks only the Ethereum side is ever used, and
//! encodes exactly as before.
//!
//! A frame transaction keeps its original bytes: its hash and re-encoding are exact whatever
//! the draft's field layout, and skim reads only what it uses (sender, nonce, fees, frame gas)
//! from `0x06 || rlp([chain_id, nonce_keys?, nonce, sender, frames, signatures, fees,
//! blob_versioned_hashes])`. EIP-8141 is a moving draft: bogota-devnet-1 already adds EIP-8250's
//! `nonce_keys` and EIP-7906's frame mode 3, which alloy's `TxEip8141` (2.5) doesn't decode.
//! The receipt is `0x06 || rlp([cumulative_gas_used, payer, [[status, [execution_gas,
//! state_gas], logs], ...]])` in the receipts trie (no bloom), `[0x06, cumulative_gas_used,
//! payer, frames]` on eth/69+. Both checked against bogota-devnet-1 (tests below).

use alloy_consensus::{
    Eip658Value, EthereumReceipt, EthereumTxEnvelope, InMemorySize, RlpDecodableReceipt, RlpEncodableReceipt, Transaction, TxEip4844,
    TxReceipt, TxType,
    crypto::RecoveryError,
    private::alloy_eips::{
        Typed2718,
        eip2718::{Decodable2718, Eip2718Result, Encodable2718, IsTyped2718},
        eip2930::AccessList,
        eip7702::SignedAuthorization,
    },
    transaction::{SignerRecoverable, TxHashRef},
};
use alloy_primitives::{Address, B256, Bloom, Bytes, ChainId, Log, TxKind, U256, keccak256, logs_bloom};
use alloy_rlp::{BufMut, Decodable, Encodable, Header};

/// EIP-8141 `FRAME_TX_TYPE`.
pub const FRAME_TX_TYPE: u8 = 0x06;

pub type EthTx = EthereumTxEnvelope<TxEip4844>;

/// One frame, as far as skim reads it: `[mode, flags, target, [execution, state], value, data]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FrameInfo {
    /// 0 default, 1 verify, 2 sender, 3 post-transaction (EIP-7906); others kept as given.
    pub mode: u8,
    /// `None`: the sender.
    pub target: Option<Address>,
    pub execution_gas: u64,
    pub state_gas: u64,
    pub value: U256,
    pub data: Bytes,
}

/// An EIP-8141 frame transaction: its exact bytes, and the fields skim uses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FrameTx {
    /// `0x06 || rlp(payload)`, as received.
    pub raw: Bytes,
    pub hash: B256,
    pub chain_id: u64,
    /// EIP-8250 nonce keys (absent before it).
    pub nonce_keys: Vec<U256>,
    pub nonce: u64,
    /// The intended sender. Not a signature recovery: the transaction's VERIFY frames establish
    /// it when executed, which skim doesn't do.
    pub sender: Address,
    /// Empty if the frames didn't parse as skim expects (the transaction is still kept).
    pub frames: Vec<FrameInfo>,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    pub max_fee_per_blob_gas: u128,
    pub blob_versioned_hashes: Vec<B256>,
}

static EMPTY_INPUT: Bytes = Bytes::new();

impl FrameTx {
    /// Decodes the payload after the type byte. Only `chain_id`, `nonce` and `sender` are
    /// required; frames, fees and blob hashes are read if they have the expected shape.
    pub fn decode_payload(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let start = *buf;
        let h = Header::decode(buf)?;
        if !h.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        if buf.len() < h.payload_length {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let (mut p, rest) = buf.split_at(h.payload_length);
        *buf = rest;
        let encoded = &start[..start.len() - rest.len()];
        let mut raw = Vec::with_capacity(encoded.len() + 1);
        raw.push(FRAME_TX_TYPE);
        raw.extend_from_slice(encoded);
        let chain_id = u64::decode(&mut p)?;
        let nonce_keys = if p.first().is_some_and(|b| *b >= 0xc0) { Vec::<U256>::decode(&mut p)? } else { Vec::new() };
        let nonce = u64::decode(&mut p)?;
        let sender = Address::decode(&mut p)?;
        let mut tx = Self {
            hash: keccak256(&raw),
            raw: raw.into(),
            chain_id,
            nonce_keys,
            nonce,
            sender,
            frames: Vec::new(),
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 0,
            max_fee_per_blob_gas: 0,
            blob_versioned_hashes: Vec::new(),
        };
        // The rest best-effort: a draft change here shouldn't cost the peer its session.
        let _ = tx.decode_rest(&mut p);
        Ok(tx)
    }

    fn decode_rest(&mut self, p: &mut &[u8]) -> alloy_rlp::Result<()> {
        let mut frames = Header::decode_bytes(p, true)?;
        let mut out = Vec::new();
        while !frames.is_empty() {
            let mut f = Header::decode_bytes(&mut frames, true)?;
            let mode = u8::decode(&mut f)?;
            let _flags = u64::decode(&mut f)?;
            let t = Bytes::decode(&mut f)?;
            let target = (t.len() == 20).then(|| Address::from_slice(&t));
            let mut limits = Header::decode_bytes(&mut f, true)?;
            let execution_gas = u64::decode(&mut limits)?;
            let state_gas = u64::decode(&mut limits)?;
            let value = U256::decode(&mut f)?;
            let data = Bytes::decode(&mut f)?;
            out.push(FrameInfo { mode, target, execution_gas, state_gas, value, data });
        }
        self.frames = out;
        let _signatures = Header::decode_bytes(p, true)?;
        let mut fees = Header::decode_bytes(p, true)?;
        self.max_priority_fee_per_gas = u128::decode(&mut fees)?;
        self.max_fee_per_gas = u128::decode(&mut fees)?;
        self.max_fee_per_blob_gas = u128::decode(&mut fees)?;
        self.blob_versioned_hashes = Vec::<B256>::decode(p)?;
        Ok(())
    }

    /// The frames' gas budgets (intrinsic costs not included).
    pub fn frame_gas(&self) -> u64 {
        self.frames.iter().fold(0u64, |a, f| a.saturating_add(f.execution_gas).saturating_add(f.state_gas))
    }
}

impl Typed2718 for FrameTx {
    fn ty(&self) -> u8 {
        FRAME_TX_TYPE
    }
}

impl Transaction for FrameTx {
    fn chain_id(&self) -> Option<ChainId> {
        Some(self.chain_id)
    }
    fn nonce(&self) -> u64 {
        self.nonce
    }
    fn gas_limit(&self) -> u64 {
        self.frame_gas()
    }
    fn gas_price(&self) -> Option<u128> {
        None
    }
    fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        Some(self.max_priority_fee_per_gas)
    }
    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        (!self.blob_versioned_hashes.is_empty()).then_some(self.max_fee_per_blob_gas)
    }
    fn priority_fee_or_price(&self) -> u128 {
        self.max_priority_fee_per_gas
    }
    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        base_fee.map_or(self.max_fee_per_gas, |b| (b as u128).saturating_add(self.max_priority_fee_per_gas).min(self.max_fee_per_gas))
    }
    fn is_dynamic_fee(&self) -> bool {
        true
    }
    /// A frame transaction has no single call; like alloy's `TxEip8141`, it reads as a call to
    /// its sender with no value or input.
    fn kind(&self) -> TxKind {
        TxKind::Call(self.sender)
    }
    fn is_create(&self) -> bool {
        false
    }
    fn value(&self) -> U256 {
        U256::ZERO
    }
    fn input(&self) -> &Bytes {
        &EMPTY_INPUT
    }
    fn access_list(&self) -> Option<&AccessList> {
        None
    }
    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        (!self.blob_versioned_hashes.is_empty()).then_some(&self.blob_versioned_hashes[..])
    }
    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        None
    }
}

impl Encodable2718 for FrameTx {
    fn encode_2718_len(&self) -> usize {
        self.raw.len()
    }
    fn encode_2718(&self, out: &mut dyn BufMut) {
        out.put_slice(&self.raw)
    }
}

/// A signed transaction as it appears in blocks and gossip (no blob sidecars).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SignedTx {
    Eth(EthTx),
    Frame(FrameTx),
}

impl From<EthTx> for SignedTx {
    fn from(tx: EthTx) -> Self {
        Self::Eth(tx)
    }
}

impl SignedTx {
    pub fn tx_hash(&self) -> &B256 {
        match self {
            Self::Eth(t) => t.tx_hash(),
            Self::Frame(t) => &t.hash,
        }
    }

    pub fn is_eip4844(&self) -> bool {
        matches!(self, Self::Eth(t) if t.is_eip4844())
    }

    pub fn as_frame(&self) -> Option<&FrameTx> {
        match self {
            Self::Frame(t) => Some(t),
            Self::Eth(_) => None,
        }
    }
}

macro_rules! both {
    ($self:ident, $t:ident => $e:expr) => {
        match $self {
            SignedTx::Eth($t) => $e,
            SignedTx::Frame($t) => $e,
        }
    };
}

impl Typed2718 for SignedTx {
    fn ty(&self) -> u8 {
        both!(self, t => t.ty())
    }
}

impl IsTyped2718 for SignedTx {
    fn is_type(ty: u8) -> bool {
        ty == FRAME_TX_TYPE || <EthTx as IsTyped2718>::is_type(ty)
    }
}

impl Transaction for SignedTx {
    fn chain_id(&self) -> Option<ChainId> {
        both!(self, t => t.chain_id())
    }
    fn nonce(&self) -> u64 {
        both!(self, t => t.nonce())
    }
    fn gas_limit(&self) -> u64 {
        both!(self, t => t.gas_limit())
    }
    fn gas_price(&self) -> Option<u128> {
        both!(self, t => t.gas_price())
    }
    fn max_fee_per_gas(&self) -> u128 {
        both!(self, t => t.max_fee_per_gas())
    }
    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        both!(self, t => t.max_priority_fee_per_gas())
    }
    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        both!(self, t => t.max_fee_per_blob_gas())
    }
    fn priority_fee_or_price(&self) -> u128 {
        both!(self, t => t.priority_fee_or_price())
    }
    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        both!(self, t => t.effective_gas_price(base_fee))
    }
    fn is_dynamic_fee(&self) -> bool {
        both!(self, t => t.is_dynamic_fee())
    }
    fn kind(&self) -> TxKind {
        both!(self, t => t.kind())
    }
    fn is_create(&self) -> bool {
        both!(self, t => t.is_create())
    }
    fn value(&self) -> U256 {
        both!(self, t => t.value())
    }
    fn input(&self) -> &Bytes {
        both!(self, t => t.input())
    }
    fn access_list(&self) -> Option<&AccessList> {
        both!(self, t => t.access_list())
    }
    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        both!(self, t => t.blob_versioned_hashes())
    }
    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        both!(self, t => t.authorization_list())
    }
}

impl Encodable2718 for SignedTx {
    fn encode_2718_len(&self) -> usize {
        both!(self, t => t.encode_2718_len())
    }
    fn encode_2718(&self, out: &mut dyn BufMut) {
        both!(self, t => t.encode_2718(out))
    }
    fn trie_hash(&self) -> B256 {
        *self.tx_hash()
    }
}

impl Decodable2718 for SignedTx {
    fn typed_decode(ty: u8, buf: &mut &[u8]) -> Eip2718Result<Self> {
        if ty == FRAME_TX_TYPE {
            Ok(Self::Frame(FrameTx::decode_payload(buf)?))
        } else {
            Ok(Self::Eth(EthTx::typed_decode(ty, buf)?))
        }
    }
    fn fallback_decode(buf: &mut &[u8]) -> Eip2718Result<Self> {
        Ok(Self::Eth(EthTx::fallback_decode(buf)?))
    }
}

/// The network encoding (blocks, gossip): a typed transaction is an RLP string around its
/// EIP-2718 bytes, a legacy one is its RLP list.
impl Encodable for SignedTx {
    fn encode(&self, out: &mut dyn BufMut) {
        self.network_encode(out)
    }
    fn length(&self) -> usize {
        self.network_len()
    }
}

impl Decodable for SignedTx {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Ok(Self::network_decode(buf)?)
    }
}

impl SignerRecoverable for SignedTx {
    fn recover_signer(&self) -> Result<Address, RecoveryError> {
        match self {
            Self::Eth(t) => t.recover_signer(),
            Self::Frame(t) => Ok(t.sender),
        }
    }
    fn recover_signer_unchecked(&self) -> Result<Address, RecoveryError> {
        match self {
            Self::Eth(t) => t.recover_signer_unchecked(),
            Self::Frame(t) => Ok(t.sender),
        }
    }
}

impl TxHashRef for SignedTx {
    fn tx_hash(&self) -> &B256 {
        SignedTx::tx_hash(self)
    }
}

impl InMemorySize for SignedTx {
    fn size(&self) -> usize {
        match self {
            Self::Eth(t) => t.size(),
            Self::Frame(t) => size_of::<FrameTx>() + t.raw.len() + t.frames.len() * size_of::<FrameInfo>() + t.frames.iter().map(|f| f.data.len()).sum::<usize>(),
        }
    }
}

/// Like the Ethereum transaction's JSON; a frame transaction with the fields skim reads.
impl serde::Serialize for SignedTx {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Eth(t) => t.serialize(s),
            Self::Frame(t) => {
                let q = |v: u128| format!("{v:#x}");
                serde_json::json!({
                    "type": format!("{FRAME_TX_TYPE:#x}"), "hash": t.hash,
                    "chainId": format!("{:#x}", t.chain_id), "nonce": format!("{:#x}", t.nonce),
                    "nonceKeys": t.nonce_keys, "sender": t.sender,
                    "frames": t.frames.iter().map(|f| serde_json::json!({
                        "mode": format!("{:#x}", f.mode), "target": f.target,
                        "executionGas": format!("{:#x}", f.execution_gas), "stateGas": format!("{:#x}", f.state_gas),
                        "value": f.value, "data": f.data,
                    })).collect::<Vec<_>>(),
                    "maxPriorityFeePerGas": q(t.max_priority_fee_per_gas), "maxFeePerGas": q(t.max_fee_per_gas),
                    "maxFeePerBlobGas": q(t.max_fee_per_blob_gas), "blobVersionedHashes": t.blob_versioned_hashes,
                    "raw": t.raw,
                })
                .serialize(s)
            }
        }
    }
}

/// alloy's RPC transaction type needs it; a frame transaction comes back from its `raw` bytes.
impl<'de> serde::Deserialize<'de> for SignedTx {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        if v.get("type").and_then(|t| t.as_str()) == Some("0x6") {
            let raw: Bytes = serde_json::from_value(v.get("raw").cloned().unwrap_or_default()).map_err(D::Error::custom)?;
            Self::decode_2718(&mut raw.as_ref()).map_err(D::Error::custom)
        } else {
            serde_json::from_value::<EthTx>(v).map(Self::Eth).map_err(D::Error::custom)
        }
    }
}

// ---- receipts ---------------------------------------------------------------------------------

/// One frame's outcome in an EIP-8141 receipt. Its logs are in [`Receipt::logs`], in frame
/// order: `logs` says how many are this frame's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameResult {
    /// 1 success, 0 failure, 2 skipped (an atomic batch failed).
    pub status: u8,
    pub execution_gas: u64,
    pub state_gas: u64,
    pub logs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameReceipt {
    /// Who paid the transaction's fees.
    pub payer: Address,
    pub frames: Vec<FrameResult>,
}

/// A receipt: Ethereum's fields, plus the frame part for an EIP-8141 transaction.
///
/// For a frame transaction, `logs` is the concatenation of its frames' logs (EIP-8141: what
/// `logsBloom` and log indexes use), and `success` means no frame failed: EIP-8141 receipts
/// carry no transaction-level status (an assumption, docs/guide/hegota.md).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Receipt {
    pub tx_type: u8,
    pub success: bool,
    pub cumulative_gas_used: u64,
    pub logs: Vec<Log>,
    pub frame: Option<FrameReceipt>,
}

impl From<EthereumReceipt> for Receipt {
    fn from(r: EthereumReceipt) -> Self {
        Self { tx_type: r.tx_type as u8, success: r.success, cumulative_gas_used: r.cumulative_gas_used, logs: r.logs, frame: None }
    }
}

impl Receipt {
    /// The Ethereum receipt behind a non-frame receipt.
    fn eth(&self) -> EthereumReceipt {
        EthereumReceipt {
            tx_type: TxType::try_from(self.tx_type).unwrap_or(TxType::Legacy),
            success: self.success,
            cumulative_gas_used: self.cumulative_gas_used,
            logs: self.logs.clone(),
        }
    }

    /// `[[status, [execution, state], logs], ...]`.
    fn frames_rlp(&self, f: &FrameReceipt, out: &mut dyn BufMut) {
        let mut items = Vec::new();
        let mut at = 0;
        for fr in &f.frames {
            let logs = &self.logs[at.min(self.logs.len())..(at + fr.logs).min(self.logs.len())];
            at += fr.logs;
            let mut body = Vec::new();
            fr.status.encode(&mut body);
            let gas_len = fr.execution_gas.length() + fr.state_gas.length();
            Header { list: true, payload_length: gas_len }.encode(&mut body);
            fr.execution_gas.encode(&mut body);
            fr.state_gas.encode(&mut body);
            logs.to_vec().encode(&mut body);
            Header { list: true, payload_length: body.len() }.encode(&mut items);
            items.extend_from_slice(&body);
        }
        Header { list: true, payload_length: items.len() }.encode(out);
        out.put_slice(&items);
    }

    /// The frame receipt's fields after any type: `cumulative_gas_used, payer, frames`.
    fn frame_fields(&self, f: &FrameReceipt) -> Vec<u8> {
        let mut v = Vec::new();
        self.cumulative_gas_used.encode(&mut v);
        f.payer.encode(&mut v);
        self.frames_rlp(f, &mut v);
        v
    }

    /// Decodes `cumulative_gas_used, payer, frames` from a list's payload.
    fn decode_frame_fields(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let cumulative_gas_used = u64::decode(buf)?;
        let payer = Address::decode(buf)?;
        let h = Header::decode(buf)?;
        if !h.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let (mut frames_buf, rest) = buf.split_at(h.payload_length);
        *buf = rest;
        let (mut frames, mut logs) = (Vec::new(), Vec::new());
        while !frames_buf.is_empty() {
            let fh = Header::decode(&mut frames_buf)?;
            if !fh.list {
                return Err(alloy_rlp::Error::UnexpectedString);
            }
            let (mut f, rest) = frames_buf.split_at(fh.payload_length);
            frames_buf = rest;
            let status = u8::decode(&mut f)?;
            let gh = Header::decode(&mut f)?;
            if !gh.list {
                return Err(alloy_rlp::Error::UnexpectedString);
            }
            let execution_gas = u64::decode(&mut f)?;
            let state_gas = u64::decode(&mut f)?;
            let frame_logs = Vec::<Log>::decode(&mut f)?;
            frames.push(FrameResult { status, execution_gas, state_gas, logs: frame_logs.len() });
            logs.extend(frame_logs);
        }
        let success = frames.iter().all(|f| f.status != 0);
        Ok(Self { tx_type: FRAME_TX_TYPE, success, cumulative_gas_used, logs, frame: Some(FrameReceipt { payer, frames }) })
    }
}

impl Typed2718 for Receipt {
    fn ty(&self) -> u8 {
        self.tx_type
    }
}

impl TxReceipt for Receipt {
    type Log = Log;

    fn status_or_post_state(&self) -> Eip658Value {
        Eip658Value::Eip658(self.success)
    }
    fn status(&self) -> bool {
        self.success
    }
    fn bloom(&self) -> Bloom {
        logs_bloom(self.logs.iter())
    }
    fn cumulative_gas_used(&self) -> u64 {
        self.cumulative_gas_used
    }
    fn logs(&self) -> &[Log] {
        &self.logs
    }
}

/// eth/69+ (`Receipts69`/`Receipts70`): `[tx-type, status, cumulative-gas, logs]`, or for a frame
/// receipt `[tx-type, cumulative-gas, payer, frames]`.
impl Encodable for Receipt {
    fn encode(&self, out: &mut dyn BufMut) {
        match &self.frame {
            None => self.eth().encode(out),
            Some(f) => {
                let fields = self.frame_fields(f);
                Header { list: true, payload_length: fields.len() + 1 }.encode(out);
                FRAME_TX_TYPE.encode(out);
                out.put_slice(&fields);
            }
        }
    }
    fn length(&self) -> usize {
        let mut v = Vec::new();
        self.encode(&mut v);
        v.len()
    }
}

impl Decodable for Receipt {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let mut peek = *buf;
        let h = Header::decode(&mut peek)?;
        if h.list && peek.first() == Some(&FRAME_TX_TYPE) {
            let (mut payload, rest) = peek.split_at(h.payload_length);
            let _ty = u8::decode(&mut payload)?;
            let r = Self::decode_frame_fields(&mut payload)?;
            *buf = rest;
            return Ok(r);
        }
        Ok(EthereumReceipt::decode(buf)?.into())
    }
}

/// eth/66-68, with the bloom. Frame receipts only travel on eth/69+ (every Hegota client).
impl RlpEncodableReceipt for Receipt {
    fn rlp_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        self.eth().rlp_encoded_length_with_bloom(bloom)
    }
    fn rlp_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        self.eth().rlp_encode_with_bloom(bloom, out)
    }
}

impl RlpDecodableReceipt for Receipt {
    fn rlp_decode_with_bloom(buf: &mut &[u8]) -> alloy_rlp::Result<alloy_consensus::ReceiptWithBloom<Self>> {
        let r = EthereumReceipt::rlp_decode_with_bloom(buf)?;
        Ok(alloy_consensus::ReceiptWithBloom { logs_bloom: r.logs_bloom, receipt: r.receipt.into() })
    }
}

/// The receipts-trie encoding. A frame receipt has no bloom in it.
impl alloy_consensus::Eip2718EncodableReceipt for Receipt {
    fn eip2718_encoded_length_with_bloom(&self, bloom: &Bloom) -> usize {
        let mut v = Vec::new();
        self.eip2718_encode_with_bloom(bloom, &mut v);
        v.len()
    }
    fn eip2718_encode_with_bloom(&self, bloom: &Bloom, out: &mut dyn BufMut) {
        match &self.frame {
            None => self.eth().eip2718_encode_with_bloom(bloom, out),
            Some(f) => {
                let fields = self.frame_fields(f);
                out.put_u8(FRAME_TX_TYPE);
                Header { list: true, payload_length: fields.len() }.encode(out);
                out.put_slice(&fields);
            }
        }
    }
}

impl InMemorySize for Receipt {
    fn size(&self) -> usize {
        size_of::<Self>() + self.logs.iter().map(|l| size_of::<Log>() + l.topics().len() * 32 + l.data.data.len()).sum::<usize>()
            + self.frame.as_ref().map_or(0, |f| f.frames.len() * size_of::<FrameResult>())
    }
}

impl Receipt {
    /// The receipts root of a block's receipts (`receiptsRoot`).
    pub fn calculate_receipt_root_no_memo(receipts: &[Self]) -> B256 {
        use alloy_consensus::Eip2718EncodableReceipt as _;
        alloy_consensus::proofs::ordered_trie_root_with_encoder(receipts, |r, buf| r.eip2718_encode_with_bloom(&r.bloom(), buf))
    }
}

// ---- reth wiring ------------------------------------------------------------------------------

/// skim's node primitives: Ethereum headers, [`SignedTx`], [`Receipt`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkimPrimitives;

impl reth_primitives_traits::NodePrimitives for SkimPrimitives {
    type Block = alloy_consensus::Block<SignedTx>;
    type BlockHeader = alloy_consensus::Header;
    type BlockBody = alloy_consensus::BlockBody<SignedTx>;
    type SignedTx = SignedTx;
    type Receipt = Receipt;
}

/// What skim's EL network speaks. Pooled transactions are [`SignedTx`] too: skim requests them
/// only for non-blob transactions (`--mempool-probe`), which travel unwrapped.
pub type SkimNetworkPrimitives = reth_eth_wire_types::BasicNetworkPrimitives<SkimPrimitives, SignedTx>;

#[cfg(test)]
mod tests {
    use super::*;

    /// bogota-devnet-1 block 43217 (Nethermind-built): its only transaction, a frame
    /// transaction, and its receipt as `debug_getRawReceipts` returns it.
    const RAW_RECEIPT: &str = include_str!("../tests/fixtures/bogota-43217.receipt.hex");
    const RECEIPTS_ROOT: &str = include_str!("../tests/fixtures/bogota-43217.receipts-root.hex");

    fn hex(s: &str) -> Vec<u8> {
        alloy_primitives::hex::decode(s.trim()).unwrap()
    }

    #[test]
    fn a_frame_receipt_roundtrips_and_matches_the_blocks_receipts_root() {
        let raw = hex(RAW_RECEIPT);
        assert_eq!(raw[0], FRAME_TX_TYPE);
        // The trie encoding: type || rlp([cumulative_gas_used, payer, frames]).
        let mut body = &raw[1..];
        let h = Header::decode(&mut body).unwrap();
        assert!(h.list);
        let r = Receipt::decode_frame_fields(&mut &body[..h.payload_length]).unwrap();
        let f = r.frame.as_ref().unwrap();
        assert_eq!(f.payer, "0xa399c1083d03924ecda5c693282b861fe7c33ba7".parse::<Address>().unwrap());
        assert_eq!((r.cumulative_gas_used, r.success, r.logs.len()), (0x2cb39, true, 2));
        assert_eq!(f.frames[0], FrameResult { status: 1, execution_gas: 0xbeb, state_gas: 0, logs: 0 });
        use alloy_consensus::Eip2718EncodableReceipt as _;
        let mut again = Vec::new();
        r.eip2718_encode_with_bloom(&r.bloom(), &mut again);
        assert_eq!(again, raw, "re-encodes byte for byte");
        assert_eq!(Receipt::calculate_receipt_root_no_memo(std::slice::from_ref(&r)), B256::from_slice(&hex(RECEIPTS_ROOT)));
        // eth/69: [type, cumulative gas, payer, frames], and back.
        let mut wire = Vec::new();
        r.encode(&mut wire);
        assert_eq!(wire.len(), r.length());
        assert_eq!(Receipt::decode(&mut wire.as_slice()).unwrap(), r);
    }

    #[test]
    fn ethereum_receipts_encode_as_before() {
        let e = EthereumReceipt { tx_type: TxType::Eip1559, success: true, cumulative_gas_used: 21_000, logs: vec![] };
        let r: Receipt = e.clone().into();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        e.encode(&mut a);
        r.encode(&mut b);
        assert_eq!(a, b);
        assert_eq!(Receipt::decode(&mut a.as_slice()).unwrap(), r);
        assert_eq!(Receipt::calculate_receipt_root_no_memo(std::slice::from_ref(&r)), EthereumReceipt::calculate_receipt_root_no_memo(&[e]));
    }

    /// A frame transaction in bogota-devnet-1's layout: EIP-8250 nonce keys, an EIP-7906
    /// post-transaction frame (mode 3).
    fn devnet_frame_tx() -> Vec<u8> {
        let mut p = Vec::new();
        7038851975u64.encode(&mut p);
        vec![U256::ZERO].encode(&mut p);
        0x11cu64.encode(&mut p);
        Address::repeat_byte(7).encode(&mut p);
        let frame = |mode: u8, exec: u64| {
            let mut f = Vec::new();
            mode.encode(&mut f);
            0u8.encode(&mut f);
            Bytes::new().encode(&mut f);
            let mut l = Vec::new();
            exec.encode(&mut l);
            0u64.encode(&mut l);
            Header { list: true, payload_length: l.len() }.encode(&mut f);
            f.extend(l);
            U256::ZERO.encode(&mut f);
            Bytes::from_static(&[0xab]).encode(&mut f);
            let mut out = Vec::new();
            Header { list: true, payload_length: f.len() }.encode(&mut out);
            out.extend(f);
            out
        };
        let frames = [frame(1, 5000), frame(3, 100)].concat();
        Header { list: true, payload_length: frames.len() }.encode(&mut p);
        p.extend(frames);
        Vec::<Bytes>::new().encode(&mut p); // signatures
        let mut fees = Vec::new();
        0x77359400u64.encode(&mut fees);
        0x7735940eu64.encode(&mut fees);
        0u64.encode(&mut fees);
        Header { list: true, payload_length: fees.len() }.encode(&mut p);
        p.extend(fees);
        Vec::<B256>::new().encode(&mut p);
        let mut raw = vec![FRAME_TX_TYPE];
        Header { list: true, payload_length: p.len() }.encode(&mut raw);
        raw.extend(p);
        raw
    }

    #[test]
    fn a_frame_transaction_in_the_devnets_layout_keeps_its_bytes() {
        let raw = devnet_frame_tx();
        let tx = SignedTx::decode_2718(&mut raw.as_slice()).unwrap();
        let f = tx.as_frame().unwrap();
        assert_eq!((f.chain_id, f.nonce_keys.clone(), f.nonce, f.sender), (7038851975, vec![U256::ZERO], 0x11c, Address::repeat_byte(7)));
        assert_eq!(f.frames.iter().map(|x| (x.mode, x.execution_gas)).collect::<Vec<_>>(), vec![(1, 5000), (3, 100)]);
        assert_eq!((tx.max_fee_per_gas(), tx.max_priority_fee_per_gas()), (0x7735940e, Some(0x77359400)));
        assert_eq!(*tx.tx_hash(), keccak256(&raw), "the hash is over the bytes as received");
        assert_eq!(tx.encoded_2718(), raw);
        // Through the network encoding (an RLP string around them), and JSON.
        let mut wire = Vec::new();
        tx.encode(&mut wire);
        assert_eq!(SignedTx::decode(&mut wire.as_slice()).unwrap(), tx);
        let back: SignedTx = serde_json::from_value(serde_json::to_value(&tx).unwrap()).unwrap();
        assert_eq!(back, tx);
        assert_eq!(tx.recover_signer().unwrap(), Address::repeat_byte(7));
    }

    #[test]
    fn a_frame_transaction_whose_frames_dont_parse_is_still_kept() {
        // A future draft's frame shape: only the required prefix is read.
        let mut p = Vec::new();
        1u64.encode(&mut p);
        5u64.encode(&mut p);
        Address::repeat_byte(9).encode(&mut p);
        Bytes::from_static(b"not frames").encode(&mut p);
        let mut raw = vec![FRAME_TX_TYPE];
        Header { list: true, payload_length: p.len() }.encode(&mut raw);
        raw.extend(p);
        let tx = SignedTx::decode_2718(&mut raw.as_slice()).unwrap();
        assert_eq!((tx.nonce(), tx.as_frame().unwrap().frames.len(), tx.encoded_2718()), (5, 0, raw));
    }

    #[test]
    fn ethereum_transactions_decode_as_before() {
        let legacy = EthTx::Legacy(alloy_consensus::Signed::new_unhashed(Default::default(), alloy_primitives::Signature::test_signature()));
        let mut a = Vec::new();
        legacy.encode(&mut a);
        assert_eq!(SignedTx::decode(&mut a.as_slice()).unwrap(), SignedTx::Eth(legacy));
    }
}
