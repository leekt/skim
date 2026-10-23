//! snap/1 client as an RLPx satellite protocol, so peers that only speak snap/1 (Geth v1.17)
//! can serve state. reth v2.7 handles snap/2 natively, but its client side has no snap/1.
//!
//! Messages 0x00–0x05 (`GetAccountRange`, `GetStorageRanges`, `GetByteCodes` and their
//! responses) have the same wire format in snap/1 and snap/2, slim account bodies included, so
//! reth's message types are reused. When a peer supports both, RLPx negotiates only the higher
//! version, so each peer ends up on exactly one path.

use std::{
    collections::HashMap,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

use alloy_primitives::bytes::{BufMut, BytesMut};
use alloy_rlp::{Decodable, Encodable};
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use reth_eth_wire::{
    capability::SharedCapabilities, multiplex::ProtocolConnection, protocol::Protocol, Capability,
};
use reth_eth_wire_types::snap::{
    AccountRangeMessage, ByteCodesMessage, GetAccountRangeMessage, GetByteCodesMessage,
    GetStorageRangesMessage, StorageRangesMessage,
};
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network_api::{Direction, PeerId};
use tokio::sync::{mpsc, oneshot};

const GET_ACCOUNT_RANGE: u8 = 0x00;
const ACCOUNT_RANGE: u8 = 0x01;
const GET_STORAGE_RANGES: u8 = 0x02;
const STORAGE_RANGES: u8 = 0x03;
const GET_BYTE_CODES: u8 = 0x04;
const BYTE_CODES: u8 = 0x05;

pub(crate) fn protocol() -> Protocol {
    Protocol::new(Capability::new_static("snap", 1), 8)
}

#[derive(Debug)]
pub enum Snap1Response {
    AccountRange(AccountRangeMessage),
    StorageRanges(StorageRangesMessage),
    ByteCodes(ByteCodesMessage),
}

pub enum Snap1Request {
    AccountRange(GetAccountRangeMessage),
    StorageRanges(GetStorageRangesMessage),
    ByteCodes(GetByteCodesMessage),
}

struct Command {
    request: Snap1Request,
    response: oneshot::Sender<Snap1Response>,
}

/// Live snap/1 sessions, keyed by peer.
#[derive(Default, Clone)]
pub(crate) struct Snap1Peers {
    /// Peer -> (session id, command channel). The session id keeps a closing old session from
    /// unregistering a newer one for the same peer.
    inner: Arc<Mutex<HashMap<PeerId, (u64, mpsc::UnboundedSender<Command>)>>>,
    next_id: Arc<AtomicU64>,
}

impl Snap1Peers {
    pub(crate) fn contains(&self, peer: &PeerId) -> bool {
        self.inner.lock().get(peer).is_some_and(|(_, tx)| !tx.is_closed())
    }

    /// Sends a request. The receiver errors if the session closes first.
    pub(crate) fn send(
        &self,
        peer: PeerId,
        mut request: Snap1Request,
    ) -> Option<oneshot::Receiver<Snap1Response>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        match &mut request {
            Snap1Request::AccountRange(m) => m.request_id = id,
            Snap1Request::StorageRanges(m) => m.request_id = id,
            Snap1Request::ByteCodes(m) => m.request_id = id,
        }
        let (tx, rx) = oneshot::channel();
        self.inner.lock().get(&peer)?.1.send(Command { request, response: tx }).ok()?;
        Some(rx)
    }
}

#[derive(Clone)]
pub(crate) struct Snap1Handler {
    pub(crate) peers: Snap1Peers,
}

impl std::fmt::Debug for Snap1Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Snap1Handler")
    }
}

impl ProtocolHandler for Snap1Handler {
    type ConnectionHandler = Self;

    fn on_incoming(&self, _: SocketAddr) -> Option<Self> {
        Some(self.clone())
    }

    fn on_outgoing(&self, _: SocketAddr, _: PeerId) -> Option<Self> {
        Some(self.clone())
    }
}

impl ConnectionHandler for Snap1Handler {
    type Connection = Snap1Connection;

    fn protocol(&self) -> Protocol {
        protocol()
    }

    fn on_unsupported_by_peer(self, _: &SharedCapabilities, _: Direction, _: PeerId) -> OnNotSupported {
        OnNotSupported::KeepAlive
    }

    fn into_connection(self, _: Direction, peer: PeerId, conn: ProtocolConnection) -> Snap1Connection {
        let (tx, rx) = mpsc::unbounded_channel();
        let session = self.peers.next_id.fetch_add(1, Ordering::Relaxed);
        self.peers.inner.lock().insert(peer, (session, tx));
        tracing::debug!(%peer, "snap/1 session");
        Snap1Connection { conn, commands: rx, pending: HashMap::new(), peers: self.peers, peer, session, replies: Default::default() }
    }
}

/// Yields outgoing frames; consumes incoming responses and routes them by request id.
pub(crate) struct Snap1Connection {
    conn: ProtocolConnection,
    commands: mpsc::UnboundedReceiver<Command>,
    pending: HashMap<u64, oneshot::Sender<Snap1Response>>,
    peers: Snap1Peers,
    peer: PeerId,
    session: u64,
    /// Answers to the peer's own requests, waiting to be sent.
    replies: std::collections::VecDeque<BytesMut>,
}

impl Drop for Snap1Connection {
    fn drop(&mut self) {
        // Pending senders drop with us, so their receivers observe the disconnect.
        let mut map = self.peers.inner.lock();
        if map.get(&self.peer).is_some_and(|(s, _)| *s == self.session) {
            map.remove(&self.peer);
        }
    }
}

fn frame(id: u8, msg: &impl Encodable) -> BytesMut {
    let mut buf = BytesMut::with_capacity(1 + msg.length());
    buf.put_u8(id);
    msg.encode(&mut buf);
    buf
}

pub(crate) fn fuzz_decode(raw: &[u8]) -> bool {
    decode_response(raw).is_some()
}

/// The answer to a request the peer sent: skim holds no state tries (an empty range with no
/// proof is snap's "not available") and serves no code over snap/1, but answers at once, as on
/// snap/2, rather than letting the peer time out. (Review: requests were dropped.)
fn answer_request(raw: &[u8]) -> Option<BytesMut> {
    let (&id, mut body) = raw.split_first()?;
    Some(match id {
        GET_ACCOUNT_RANGE => {
            let m = GetAccountRangeMessage::decode(&mut body).ok()?;
            frame(ACCOUNT_RANGE, &AccountRangeMessage { request_id: m.request_id, accounts: vec![], proof: vec![] })
        }
        GET_STORAGE_RANGES => {
            let m = GetStorageRangesMessage::decode(&mut body).ok()?;
            frame(STORAGE_RANGES, &StorageRangesMessage { request_id: m.request_id, slots: vec![], proof: vec![] })
        }
        GET_BYTE_CODES => {
            let m = GetByteCodesMessage::decode(&mut body).ok()?;
            frame(BYTE_CODES, &ByteCodesMessage { request_id: m.request_id, codes: vec![] })
        }
        _ => return None,
    })
}

fn decode_response(raw: &[u8]) -> Option<(u64, Snap1Response)> {
    let (&id, mut body) = raw.split_first()?;
    let r = match id {
        ACCOUNT_RANGE => {
            let m = AccountRangeMessage::decode(&mut body).ok()?;
            (m.request_id, Snap1Response::AccountRange(m))
        }
        STORAGE_RANGES => {
            let m = StorageRangesMessage::decode(&mut body).ok()?;
            (m.request_id, Snap1Response::StorageRanges(m))
        }
        BYTE_CODES => {
            let m = ByteCodesMessage::decode(&mut body).ok()?;
            (m.request_id, Snap1Response::ByteCodes(m))
        }
        _ => return None, // requests to us, or trie nodes: we serve nothing
    };
    Some(r)
}

impl Stream for Snap1Connection {
    type Item = BytesMut;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<BytesMut>> {
        let this = self.get_mut();
        // Inbound responses first, so a burst of requests can't starve them.
        loop {
            match this.conn.poll_next_unpin(cx) {
                Poll::Ready(Some(raw)) => {
                    if let Some((id, resp)) = decode_response(&raw) {
                        if let Some(tx) = this.pending.remove(&id) {
                            let _ = tx.send(resp);
                        }
                    } else if this.replies.len() < 64
                        && let Some(reply) = answer_request(&raw)
                    {
                        this.replies.push_back(reply);
                    }
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => break,
            }
        }
        if let Some(reply) = this.replies.pop_front() {
            return Poll::Ready(Some(reply));
        }
        loop {
            let Poll::Ready(cmd) = this.commands.poll_recv(cx) else { return Poll::Pending };
            let Some(Command { request, response }) = cmd else { return Poll::Ready(None) };
            // The caller already gave up (timed out while queued): don't send it.
            if response.is_closed() {
                continue;
            }
            let (id, out) = match &request {
                Snap1Request::AccountRange(m) => (m.request_id, frame(GET_ACCOUNT_RANGE, m)),
                Snap1Request::StorageRanges(m) => (m.request_id, frame(GET_STORAGE_RANGES, m)),
                Snap1Request::ByteCodes(m) => (m.request_id, frame(GET_BYTE_CODES, m)),
            };
            // Requests whose callers timed out are never answered by some peers: without this
            // they would stay here for the whole session.
            this.pending.retain(|_, tx| !tx.is_closed());
            this.pending.insert(id, response);
            return Poll::Ready(Some(out));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn a_peers_request_gets_an_empty_answer_with_its_id() {
        // Review: snap/1 requests were dropped, so Geth peers waited for their timeout.
        let req = GetAccountRangeMessage { request_id: 42, root_hash: B256::ZERO, starting_hash: B256::ZERO, limit_hash: B256::repeat_byte(0xff), response_bytes: 1 << 19 };
        let reply = answer_request(&frame(GET_ACCOUNT_RANGE, &req)).expect("answered");
        match decode_response(&reply) {
            Some((42, Snap1Response::AccountRange(m))) => assert!(m.accounts.is_empty() && m.proof.is_empty()),
            other => panic!("{other:?}"),
        }
        let codes = GetByteCodesMessage { request_id: 7, hashes: vec![B256::ZERO], response_bytes: 1 << 19 };
        assert!(matches!(decode_response(&answer_request(&frame(GET_BYTE_CODES, &codes)).unwrap()), Some((7, Snap1Response::ByteCodes(_)))));
        // Responses aren't requests.
        assert!(answer_request(&frame(ACCOUNT_RANGE, &AccountRangeMessage { request_id: 1, accounts: vec![], proof: vec![] })).is_none());
    }
}
