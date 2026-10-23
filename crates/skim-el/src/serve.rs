//! Answers other peers' eth and snap requests from what skim holds: recent canonical headers,
//! bodies, receipts and access lists that skim already verified against their headers, and
//! bytecode it keeps. Anything else gets the protocol's "don't have it" answer (an empty or
//! shorter response) right away, instead of letting the peer time out.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use alloy_consensus::{Header, ReceiptWithBloom};
use alloy_primitives::{B256, Bytes, keccak256};
use alloy_rlp::Encodable;
use parking_lot::RwLock;
use reth_eth_wire_types::{
    BlockAccessLists, BlockBodies, BlockHeaders, Cells, GetBlockHeaders, GetReceipts70, HeadersDirection,
    NodeData, Receipts, Receipts69, Receipts70,
    snap::{AccountRangeMessage, BlockAccessListsMessage, ByteCodesMessage, SnapProtocolMessage, StorageRangesMessage},
};
use reth_network::{eth_requests::IncomingEthRequest, p2p::snap::client::SnapResponse};
use tokio::sync::mpsc;

use crate::{BlockBody, Receipt};

pub use reth_eth_wire_types::BlockHashOrNumber;

/// Same bounds as reth's own request handler.
pub const MAX_HEADERS_SERVE: usize = 1024;
pub const MAX_BODIES_SERVE: usize = 1024;
pub const MAX_RECEIPTS_SERVE: usize = 1024;
pub const MAX_BALS_SERVE: usize = 1024;
pub const MAX_CODES_SERVE: usize = 1024;
pub const SOFT_RESPONSE_LIMIT: usize = 2 * 1024 * 1024;

/// What skim can serve. Every item must already be verified against its block header.
pub trait ServeSource: Send + Sync + 'static {
    /// A canonical header by number, or a known canonical header by hash.
    fn header(&self, id: BlockHashOrNumber) -> Option<Header>;
    fn body(&self, hash: B256) -> Option<BlockBody>;
    fn receipts(&self, hash: B256) -> Option<Vec<Receipt>>;
    /// Raw RLP of the block access list (the exact bytes the header commits to).
    fn bal(&self, hash: B256) -> Option<Bytes>;
    fn code(&self, hash: B256) -> Option<Bytes>;
    /// Oldest block from which headers, bodies and access lists are held without gaps up to the
    /// newest one, advertised to eth/69+ peers as the start of the served range.
    fn earliest(&self) -> Option<u64>;
}

/// Requests answered and items returned, per message type.
#[derive(Debug, Default)]
pub struct ServeStats {
    pub header_reqs: AtomicU64,
    pub headers: AtomicU64,
    pub body_reqs: AtomicU64,
    pub bodies: AtomicU64,
    pub receipt_reqs: AtomicU64,
    pub receipts: AtomicU64,
    pub bal_reqs: AtomicU64,
    pub bals: AtomicU64,
    pub snap_reqs: AtomicU64,
    pub codes: AtomicU64,
    pub other_reqs: AtomicU64,
}

/// A copy of [`ServeStats`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ServeCounts {
    pub header_reqs: u64,
    pub headers: u64,
    pub body_reqs: u64,
    pub bodies: u64,
    pub receipt_reqs: u64,
    pub receipts: u64,
    pub bal_reqs: u64,
    pub bals: u64,
    pub snap_reqs: u64,
    pub codes: u64,
    pub other_reqs: u64,
}

impl ServeStats {
    pub fn counts(&self) -> ServeCounts {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ServeCounts {
            header_reqs: g(&self.header_reqs),
            headers: g(&self.headers),
            body_reqs: g(&self.body_reqs),
            bodies: g(&self.bodies),
            receipt_reqs: g(&self.receipt_reqs),
            receipts: g(&self.receipts),
            bal_reqs: g(&self.bal_reqs),
            bals: g(&self.bals),
            snap_reqs: g(&self.snap_reqs),
            codes: g(&self.codes),
            other_reqs: g(&self.other_reqs),
        }
    }
}

fn add(a: &AtomicU64, n: usize) {
    a.fetch_add(n as u64, Ordering::Relaxed);
}

pub(crate) type Source = Arc<RwLock<Option<Arc<dyn ServeSource>>>>;

/// Runs until the network drops its sender. With no source set, every request gets the empty answer.
pub(crate) async fn run(mut rx: mpsc::Receiver<IncomingEthRequest<crate::SkimNetworkPrimitives>>, src: Source, stats: Arc<ServeStats>) {
    while let Some(req) = rx.recv().await {
        let s = src.read().clone();
        handle(s.as_deref(), &stats, req);
    }
}

pub(crate) fn handle(src: Option<&dyn ServeSource>, stats: &ServeStats, req: IncomingEthRequest<crate::SkimNetworkPrimitives>) {
    match req {
        IncomingEthRequest::GetBlockHeaders { request, response, .. } => {
            add(&stats.header_reqs, 1);
            let hs = src.map(|s| headers(s, request)).unwrap_or_default();
            add(&stats.headers, hs.len());
            let _ = response.send(Ok(BlockHeaders(hs)));
        }
        IncomingEthRequest::GetBlockBodies { request, response, .. } => {
            add(&stats.body_reqs, 1);
            let bs = src.map(|s| prefix(request.0.iter().take(MAX_BODIES_SERVE), |h| s.body(*h))).unwrap_or_default();
            add(&stats.bodies, bs.len());
            let _ = response.send(Ok(BlockBodies(bs)));
        }
        IncomingEthRequest::GetReceipts { request, response, .. } => {
            add(&stats.receipt_reqs, 1);
            let rs: Vec<Vec<ReceiptWithBloom<Receipt>>> = src
                .map(|s| prefix(request.0.iter().take(MAX_RECEIPTS_SERVE), |h| s.receipts(*h)))
                .unwrap_or_default()
                .into_iter()
                .map(|b| b.into_iter().map(ReceiptWithBloom::from).collect())
                .collect();
            add(&stats.receipts, rs.len());
            let _ = response.send(Ok(Receipts(rs)));
        }
        IncomingEthRequest::GetReceipts69 { request, response, .. } => {
            add(&stats.receipt_reqs, 1);
            let rs = src.map(|s| prefix(request.0.iter().take(MAX_RECEIPTS_SERVE), |h| s.receipts(*h))).unwrap_or_default();
            add(&stats.receipts, rs.len());
            let _ = response.send(Ok(Receipts69(rs)));
        }
        IncomingEthRequest::GetReceipts70 { request, response, .. } => {
            add(&stats.receipt_reqs, 1);
            let r = src.map(|s| receipts70(s, request)).unwrap_or(Receipts70 { last_block_incomplete: false, receipts: vec![] });
            add(&stats.receipts, r.receipts.len());
            let _ = response.send(Ok(r));
        }
        IncomingEthRequest::GetBlockAccessLists { request, response, .. } => {
            add(&stats.bal_reqs, 1);
            let ls = bals(src, &request.0, SOFT_RESPONSE_LIMIT);
            add(&stats.bals, ls.iter().filter(|b| b.is_some()).count());
            let _ = response.send(Ok(BlockAccessLists(ls)));
        }
        IncomingEthRequest::GetSnap { request, response, .. } => {
            add(&stats.snap_reqs, 1);
            let _ = response.send(Ok(snap(src, stats, request)));
        }
        IncomingEthRequest::GetNodeData { response, .. } => {
            add(&stats.other_reqs, 1);
            let _ = response.send(Ok(NodeData(vec![])));
        }
        IncomingEthRequest::GetCells { request, response, .. } => {
            add(&stats.other_reqs, 1);
            let _ = response.send(Ok(Cells { cell_mask: request.cell_mask, ..Default::default() }));
        }
    }
}

/// Items in request order until the first one we don't have or the size limit (eth semantics:
/// a response is a prefix of the request).
fn prefix<'a, T: Encodable>(hashes: impl Iterator<Item = &'a B256>, get: impl Fn(&B256) -> Option<T>) -> Vec<T> {
    let mut out = Vec::new();
    let mut bytes = 0;
    for h in hashes {
        let Some(v) = get(h) else { break };
        bytes += v.length();
        out.push(v);
        if bytes > SOFT_RESPONSE_LIMIT {
            break;
        }
    }
    out
}

fn headers(src: &dyn ServeSource, req: GetBlockHeaders) -> Vec<Header> {
    let GetBlockHeaders { start_block, limit, skip, direction } = req;
    let mut out = Vec::new();
    let mut bytes = 0;
    let mut at = start_block;
    let skip = skip as u64;
    for _ in 0..limit.min(MAX_HEADERS_SERVE as u64) {
        let Some(h) = src.header(at) else { break };
        let (number, parent) = (h.number, h.parent_hash);
        bytes += h.length();
        out.push(h);
        if bytes > SOFT_RESPONSE_LIMIT {
            break;
        }
        at = match direction {
            HeadersDirection::Rising => match number.checked_add(1 + skip) {
                Some(n) => n.into(),
                None => break,
            },
            HeadersDirection::Falling if skip == 0 => parent.into(),
            HeadersDirection::Falling => match number.checked_sub(1 + skip) {
                Some(n) => n.into(),
                None => break,
            },
        };
    }
    out
}

fn receipts70(src: &dyn ServeSource, req: GetReceipts70) -> Receipts70<Receipt> {
    let GetReceipts70 { first_block_receipt_index, block_hashes } = req;
    let mut receipts = Vec::new();
    let mut bytes = 0usize;
    for (i, h) in block_hashes.iter().take(MAX_RECEIPTS_SERVE).enumerate() {
        let Some(mut block) = src.receipts(*h) else { break };
        if i == 0 && first_block_receipt_index > 0 {
            block.drain(..(first_block_receipt_index as usize).min(block.len()));
        }
        let size = block.length();
        if bytes + size <= SOFT_RESPONSE_LIMIT {
            bytes += size;
            receipts.push(block);
            continue;
        }
        // Too big: send what fits and mark the block incomplete; the peer asks for the rest.
        let mut part = Vec::new();
        for r in block {
            let n = r.length();
            if bytes + n > SOFT_RESPONSE_LIMIT {
                break;
            }
            bytes += n;
            part.push(r);
        }
        receipts.push(part);
        return Receipts70 { last_block_incomplete: true, receipts };
    }
    Receipts70 { last_block_incomplete: false, receipts }
}

/// One entry per requested hash (`None` = unavailable), stopping at the size limit (EIP-8159).
/// Each BAL is re-checked against its header's commitment before it is sent.
fn bals(src: Option<&dyn ServeSource>, hashes: &[B256], limit: usize) -> Vec<Option<Bytes>> {
    let mut out = Vec::new();
    let mut bytes = 0;
    for h in hashes.iter().take(MAX_BALS_SERVE) {
        let b = src.and_then(|s| {
            let raw = s.bal(*h)?;
            let header = s.header((*h).into())?;
            (header.block_access_list_hash == Some(keccak256(&raw))).then_some(raw)
        });
        bytes += b.as_ref().map_or(1, |b| b.len());
        out.push(b);
        if bytes > limit {
            break;
        }
    }
    out
}

fn snap(src: Option<&dyn ServeSource>, stats: &ServeStats, req: SnapProtocolMessage) -> SnapResponse {
    match req {
        // No state tries: an empty range with no proof is snap's "not available".
        SnapProtocolMessage::GetAccountRange(m) => {
            SnapResponse::AccountRange(AccountRangeMessage { request_id: m.request_id, accounts: vec![], proof: vec![] })
        }
        SnapProtocolMessage::GetStorageRanges(m) => {
            SnapResponse::StorageRanges(StorageRangesMessage { request_id: m.request_id, slots: vec![], proof: vec![] })
        }
        SnapProtocolMessage::GetByteCodes(m) => {
            let limit = (m.response_bytes as usize).min(SOFT_RESPONSE_LIMIT);
            let mut codes = Vec::new();
            let mut bytes = 0;
            // Unknown hashes are skipped (snap semantics); the requester matches by hash.
            for h in m.hashes.iter().take(MAX_CODES_SERVE) {
                let Some(c) = src.and_then(|s| s.code(*h)).filter(|c| keccak256(c) == *h) else { continue };
                bytes += c.len();
                codes.push(c);
                if bytes > limit {
                    break;
                }
            }
            add(&stats.codes, codes.len());
            SnapResponse::ByteCodes(ByteCodesMessage { request_id: m.request_id, codes })
        }
        SnapProtocolMessage::GetBlockAccessLists(m) => {
            let ls = bals(src, &m.block_hashes, (m.response_bytes as usize).min(SOFT_RESPONSE_LIMIT));
            add(&stats.bals, ls.iter().filter(|b| b.is_some()).count());
            SnapResponse::BlockAccessLists(BlockAccessListsMessage { request_id: m.request_id, block_access_lists: BlockAccessLists(ls) })
        }
        // A response-shaped message sent as a request: answer with an empty bytecode list so the
        // peer is not left waiting.
        _ => SnapResponse::ByteCodes(ByteCodesMessage { request_id: 0, codes: vec![] }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Mem {
        headers: Vec<Header>,
        bals: HashMap<B256, Bytes>,
        receipts: HashMap<B256, Vec<Receipt>>,
    }

    impl ServeSource for Mem {
        fn header(&self, id: BlockHashOrNumber) -> Option<Header> {
            match id {
                BlockHashOrNumber::Number(n) => self.headers.iter().find(|h| h.number == n).cloned(),
                BlockHashOrNumber::Hash(x) => self.headers.iter().find(|h| h.hash_slow() == x).cloned(),
            }
        }
        fn body(&self, _: B256) -> Option<BlockBody> {
            None
        }
        fn receipts(&self, h: B256) -> Option<Vec<Receipt>> {
            self.receipts.get(&h).cloned()
        }
        fn bal(&self, h: B256) -> Option<Bytes> {
            self.bals.get(&h).cloned()
        }
        fn code(&self, _: B256) -> Option<Bytes> {
            None
        }
        fn earliest(&self) -> Option<u64> {
            self.headers.first().map(|h| h.number)
        }
    }

    /// Chain 100..=109; block 105's stored BAL does not match its header.
    fn chain() -> Mem {
        let mut m = Mem::default();
        let mut parent = B256::ZERO;
        for n in 100..110u64 {
            let raw = Bytes::from(vec![0xc1, n as u8]);
            let h = Header { number: n, parent_hash: parent, block_access_list_hash: Some(keccak256(&raw)), ..Default::default() };
            parent = h.hash_slow();
            m.bals.insert(parent, if n == 105 { Bytes::from_static(&[0xc0]) } else { raw });
            m.receipts.insert(parent, vec![Receipt::default(); 3]);
            m.headers.push(h);
        }
        m
    }

    #[test]
    fn headers_follow_direction_and_skip() {
        let m = chain();
        let nums = |r: GetBlockHeaders| headers(&m, r).iter().map(|h| h.number).collect::<Vec<_>>();
        let up = GetBlockHeaders { start_block: 102u64.into(), limit: 3, skip: 0, direction: HeadersDirection::Rising };
        assert_eq!(nums(up), [102, 103, 104]);
        let down = GetBlockHeaders { start_block: m.headers[9].hash_slow().into(), limit: 4, skip: 0, direction: HeadersDirection::Falling };
        assert_eq!(nums(down), [109, 108, 107, 106]);
        let skip = GetBlockHeaders { start_block: 101u64.into(), limit: 10, skip: 2, direction: HeadersDirection::Rising };
        assert_eq!(nums(skip), [101, 104, 107]);
        let past = GetBlockHeaders { start_block: 108u64.into(), limit: 5, skip: 0, direction: HeadersDirection::Rising };
        assert_eq!(nums(past), [108, 109]);
    }

    #[test]
    fn bals_are_checked_against_the_header_and_keep_request_order() {
        let m = chain();
        let hs: Vec<B256> = [103, 105, 999, 107].iter().map(|n| m.headers.get(n - 100).map_or(B256::ZERO, |h| h.hash_slow())).collect();
        let r = bals(Some(&m), &hs, SOFT_RESPONSE_LIMIT);
        assert_eq!(r.len(), 4);
        assert!(r[0].is_some() && r[3].is_some());
        assert!(r[1].is_none(), "a BAL that does not match its header is never served");
        assert!(r[2].is_none());
        assert!(bals(None, &hs, SOFT_RESPONSE_LIMIT).iter().all(Option::is_none));
    }

    #[test]
    fn receipts_stop_at_the_first_unknown_block() {
        let m = chain();
        let hs = vec![m.headers[0].hash_slow(), B256::ZERO, m.headers[1].hash_slow()];
        let r = receipts70(&m, GetReceipts70 { first_block_receipt_index: 1, block_hashes: hs });
        assert_eq!(r.receipts.len(), 1);
        assert_eq!(r.receipts[0].len(), 2, "first_block_receipt_index skips into the first block");
        assert!(!r.last_block_incomplete);
    }
}
