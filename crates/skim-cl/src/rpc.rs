//! eth2 req/resp (`ssz_snappy` encoding) for the handful of messages a listener needs:
//! Status v1/v2, MetaData v2/v3, Ping, Goodbye, and payload envelopes by range/root (recent ones,
//! see `serve`). Other range/root protocols are registered too, but only ever answered with an
//! empty response (zero chunks), which is a valid "nothing to serve".
//!
//! Wire format (phase0 p2p-interface, "Encoding strategies"):
//! - request:  `varint(len(ssz)) ++ snappy_frames(ssz)`; empty requests (MetaData) send nothing.
//! - response: per chunk `result_byte ++ varint(len(ssz)) ++ snappy_frames(ssz)`.
//!
//! Requesters half-close after writing, responders close after the last chunk (libp2p
//! request-response does both), so each side simply reads to EOF.

use std::io::{self, Read, Write};

use alloy_primitives::B256;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::{StreamProtocol, request_response};

const MAX_RPC_BYTES: u64 = 1 << 20;

pub const STATUS_V2: &str = "/eth2/beacon_chain/req/status/2/ssz_snappy";
pub const STATUS_V1: &str = "/eth2/beacon_chain/req/status/1/ssz_snappy";
pub const METADATA_V3: &str = "/eth2/beacon_chain/req/metadata/3/ssz_snappy";
pub const METADATA_V2: &str = "/eth2/beacon_chain/req/metadata/2/ssz_snappy";
pub const PING_V1: &str = "/eth2/beacon_chain/req/ping/1/ssz_snappy";
pub const GOODBYE_V1: &str = "/eth2/beacon_chain/req/goodbye/1/ssz_snappy";

/// Protocols skim does not serve but answers with zero chunks, so peers do not see an
/// unsupported-protocol failure.
pub const EMPTY_SERVED: &[&str] = &[
    "/eth2/beacon_chain/req/beacon_blocks_by_range/2/ssz_snappy",
    "/eth2/beacon_chain/req/beacon_blocks_by_root/2/ssz_snappy",
    "/eth2/beacon_chain/req/blob_sidecars_by_range/1/ssz_snappy",
    "/eth2/beacon_chain/req/blob_sidecars_by_root/1/ssz_snappy",
    "/eth2/beacon_chain/req/data_column_sidecars_by_range/1/ssz_snappy",
    "/eth2/beacon_chain/req/data_column_sidecars_by_root/1/ssz_snappy",
];

/// Goodbye reasons (phase0 + client conventions).
pub fn goodbye_reason(code: u64) -> &'static str {
    match code {
        1 => "client_shutdown",
        2 => "irrelevant_network",
        3 => "fault_or_error",
        128 => "unable_to_verify_network",
        129 => "too_many_peers",
        250 => "bad_score",
        251 => "banned",
        252 => "banned_ip",
        _ => "other",
    }
}

/// `Status` v1 (84 bytes) / v2 (92 bytes, adds `earliest_available_slot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub fork_digest: [u8; 4],
    pub finalized_root: B256,
    pub finalized_epoch: u64,
    pub head_root: B256,
    pub head_slot: u64,
    pub earliest_available_slot: Option<u64>,
}

impl Status {
    pub fn encode(&self, v2: bool) -> Vec<u8> {
        let mut b = Vec::with_capacity(92);
        b.extend_from_slice(&self.fork_digest);
        b.extend_from_slice(self.finalized_root.as_slice());
        b.extend_from_slice(&self.finalized_epoch.to_le_bytes());
        b.extend_from_slice(self.head_root.as_slice());
        b.extend_from_slice(&self.head_slot.to_le_bytes());
        if v2 {
            b.extend_from_slice(&self.earliest_available_slot.unwrap_or(self.head_slot).to_le_bytes());
        }
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != 84 && b.len() != 92 {
            return None;
        }
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        Some(Self {
            fork_digest: b[0..4].try_into().unwrap(),
            finalized_root: B256::from_slice(&b[4..36]),
            finalized_epoch: u64_at(36),
            head_root: B256::from_slice(&b[44..76]),
            head_slot: u64_at(76),
            earliest_available_slot: (b.len() == 92).then(|| u64_at(84)),
        })
    }
}

/// `MetaData` v3 (Fulu): seq, attnets, syncnets, custody_group_count. v2 omits the last field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetaData {
    pub seq_number: u64,
    pub attnets: [u8; 8],
    pub syncnets: u8,
    pub custody_group_count: u64,
}

impl MetaData {
    pub fn encode(&self, v3: bool) -> Vec<u8> {
        let mut b = Vec::with_capacity(25);
        b.extend_from_slice(&self.seq_number.to_le_bytes());
        b.extend_from_slice(&self.attnets);
        b.push(self.syncnets);
        if v3 {
            b.extend_from_slice(&self.custody_group_count.to_le_bytes());
        }
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < 17 {
            return None;
        }
        Some(Self {
            seq_number: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            attnets: b[8..16].try_into().unwrap(),
            syncnets: b[16],
            custody_group_count: b
                .get(17..25)
                .map(|x| u64::from_le_bytes(x.try_into().unwrap()))
                .unwrap_or(0),
        })
    }
}

/// One response chunk: result code (0 = success) and SSZ payload (or error message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub code: u8,
    pub data: Vec<u8>,
}

/// Same framing for every protocol; the protocol only decides whether a request has a body.
#[derive(Clone, Default)]
pub struct Codec;

fn has_request_body(p: &str) -> bool {
    !p.contains("/metadata/")
}

pub fn write_varint(mut v: usize, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub fn read_varint(b: &[u8]) -> io::Result<(usize, usize)> {
    let mut v: usize = 0;
    for (i, &x) in b.iter().enumerate().take(10) {
        v |= ((x & 0x7f) as usize) << (7 * i);
        if x & 0x80 == 0 {
            return Ok((v, i + 1));
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "bad varint"))
}

/// `varint(len) ++ snappy_frames(ssz)`.
pub fn encode_payload(ssz: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(ssz.len(), &mut out);
    let mut enc = snap::write::FrameEncoder::new(out);
    enc.write_all(ssz).expect("in-memory write");
    enc.into_inner().expect("in-memory flush")
}

/// Decodes one `varint(len) ++ snappy_frames` payload from the start of `b`.
pub fn decode_payload(b: &[u8]) -> io::Result<Vec<u8>> {
    let (len, n) = read_varint(b)?;
    if len as u64 > MAX_RPC_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "rpc payload too large"));
    }
    let mut out = vec![0u8; len];
    snap::read::FrameDecoder::new(&b[n..]).read_exact(&mut out)?;
    Ok(out)
}

async fn read_all<T: AsyncRead + Unpin + Send>(io: &mut T) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    io.take(MAX_RPC_BYTES).read_to_end(&mut buf).await?;
    Ok(buf)
}

impl request_response::Codec for Codec {
    type Protocol = StreamProtocol;
    type Request = Vec<u8>;
    type Response = Vec<Chunk>;

    async fn read_request<T>(&mut self, p: &StreamProtocol, io: &mut T) -> io::Result<Vec<u8>>
    where
        T: AsyncRead + Unpin + Send,
    {
        if !has_request_body(p.as_ref()) {
            return Ok(Vec::new());
        }
        // Range/root requests are never parsed; anything that decodes is fine.
        decode_payload(&read_all(io).await?)
    }

    async fn read_response<T>(&mut self, _p: &StreamProtocol, io: &mut T) -> io::Result<Vec<Chunk>>
    where
        T: AsyncRead + Unpin + Send,
    {
        // Only single-chunk responses (Status, Ping, MetaData) are ever requested by skim.
        let buf = read_all(io).await?;
        let Some((&code, rest)) = buf.split_first() else {
            return Ok(Vec::new());
        };
        Ok(vec![Chunk { code, data: decode_payload(rest)? }])
    }

    async fn write_request<T>(&mut self, p: &StreamProtocol, io: &mut T, req: Vec<u8>) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        if has_request_body(p.as_ref()) {
            let mut req = req;
            // Status is built as v2; v1 is the same without `earliest_available_slot`.
            if p.as_ref() == STATUS_V1 {
                req.truncate(84);
            }
            io.write_all(&encode_payload(&req)).await?;
        }
        io.flush().await
    }

    async fn write_response<T>(&mut self, p: &StreamProtocol, io: &mut T, res: Vec<Chunk>) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        for mut c in res {
            // MetaData is built as v3; v2 is the same without `custody_group_count`.
            if p.as_ref() == METADATA_V2 && c.code == 0 {
                c.data.truncate(17);
            }
            if p.as_ref() == STATUS_V1 && c.code == 0 {
                c.data.truncate(84);
            }
            let mut b = vec![c.code];
            b.extend_from_slice(&encode_payload(&c.data));
            io.write_all(&b).await?;
        }
        io.flush().await
    }
}

pub const ENVELOPES_BY_RANGE: &str = "/eth2/beacon_chain/req/execution_payload_envelopes_by_range/1/ssz_snappy";
pub const ENVELOPES_BY_ROOT: &str = "/eth2/beacon_chain/req/execution_payload_envelopes_by_root/1/ssz_snappy";
pub const LIGHT_CLIENT_BOOTSTRAP: &str = "/eth2/beacon_chain/req/light_client_bootstrap/1/ssz_snappy";
pub const LIGHT_CLIENT_UPDATES_BY_RANGE: &str = "/eth2/beacon_chain/req/light_client_updates_by_range/1/ssz_snappy";
/// Spec limit per request (`MAX_REQUEST_PAYLOADS`).
pub const MAX_REQUEST_PAYLOADS: u64 = 128;
const MAX_ENVELOPE_RESPONSE_BYTES: u64 = 256 << 20;

/// Chunked SSZ protocols (`ExecutionPayloadEnvelopesByRange/ByRoot`, `LightClientBootstrap`):
/// request is raw SSZ, response the SSZ of each chunk. Read: context bytes are skipped (callers
/// check the envelope itself). Write: every chunk carries `digest` (all served envelopes are
/// from the current fork).
#[derive(Clone, Default)]
pub struct EnvelopesCodec {
    pub digest: [u8; 4],
}

/// Decodes one `varint(len) ++ snappy_frames` payload from the start of `b`, returning it and
/// how many input bytes it used (responses carry several chunks back to back).
pub fn decode_payload_len(b: &[u8], max: u64) -> io::Result<(Vec<u8>, usize)> {
    let (len, n) = read_varint(b)?;
    if len as u64 > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk too large"));
    }
    let mut out = vec![0u8; len];
    let mut cur = io::Cursor::new(&b[n..]);
    snap::read::FrameDecoder::new(&mut cur).read_exact(&mut out)?;
    Ok((out, n + cur.position() as usize))
}

/// Splits a ByRange/ByRoot response into chunk payloads. Stops at the first error chunk.
pub fn split_context_chunks(mut b: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    while let Some((&code, rest)) = b.split_first() {
        if code != 0 {
            if out.is_empty() {
                // Only an error: report it (1 InvalidRequest, 2 ServerError, 3 ResourceUnavailable).
                let msg = decode_payload_len(rest, 256).map(|(m, _)| String::from_utf8_lossy(&m).into_owned()).unwrap_or_default();
                return Err(io::Error::other(format!("peer error {code}: {msg}")));
            }
            break; // error chunk after data: what came before is still valid
        }
        if rest.len() < 4 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "missing context bytes"));
        }
        let (ssz, used) = decode_payload_len(&rest[4..], 16 << 20)?;
        out.push(ssz);
        b = &rest[4 + used..];
    }
    Ok(out)
}

impl request_response::Codec for EnvelopesCodec {
    type Protocol = StreamProtocol;
    type Request = Vec<u8>;
    type Response = Vec<Vec<u8>>;

    async fn read_request<T>(&mut self, _p: &StreamProtocol, io: &mut T) -> io::Result<Vec<u8>>
    where
        T: AsyncRead + Unpin + Send,
    {
        // ByRange is 16 bytes, ByRoot at most 128 roots.
        let mut buf = Vec::new();
        io.take(8 << 10).read_to_end(&mut buf).await?;
        let (ssz, _) = decode_payload_len(&buf, (MAX_REQUEST_PAYLOADS * 32).max(16))?;
        Ok(ssz)
    }

    async fn read_response<T>(&mut self, _p: &StreamProtocol, io: &mut T) -> io::Result<Vec<Vec<u8>>>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut buf = Vec::new();
        io.take(MAX_ENVELOPE_RESPONSE_BYTES).read_to_end(&mut buf).await?;
        split_context_chunks(&buf)
    }

    async fn write_request<T>(&mut self, _p: &StreamProtocol, io: &mut T, ssz: Vec<u8>) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(&encode_payload(&ssz)).await?;
        io.flush().await
    }

    async fn write_response<T>(&mut self, _p: &StreamProtocol, io: &mut T, res: Vec<Vec<u8>>) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        for ssz in res {
            let mut b = vec![0u8];
            b.extend_from_slice(&self.digest);
            b.extend_from_slice(&encode_payload(&ssz));
            io.write_all(&b).await?;
        }
        io.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip_v1_v2() {
        let s = Status {
            fork_digest: [0x66, 0x9e, 0x6c, 0x11],
            finalized_root: B256::repeat_byte(1),
            finalized_epoch: 354901,
            head_root: B256::repeat_byte(2),
            head_slot: 11356920,
            earliest_available_slot: Some(11356000),
        };
        assert_eq!(Status::decode(&s.encode(true)), Some(s));
        let v1 = Status::decode(&s.encode(false)).unwrap();
        assert_eq!(v1.earliest_available_slot, None);
        assert_eq!(v1.head_slot, s.head_slot);
        assert_eq!(Status::decode(&[0u8; 50]), None);
    }

    #[test]
    fn metadata_v2_v3() {
        let m = MetaData { seq_number: 7, attnets: [0, 0, 0, 0, 0, 0, 0x30, 0], syncnets: 0, custody_group_count: 4 };
        assert_eq!(m.encode(true).len(), 25);
        assert_eq!(m.encode(false).len(), 17);
        assert_eq!(MetaData::decode(&m.encode(true)), Some(m));
    }

    #[test]
    fn varint_and_framing() {
        for v in [0usize, 1, 127, 128, 300, 1 << 20] {
            let mut b = Vec::new();
            write_varint(v, &mut b);
            assert_eq!(read_varint(&b).unwrap(), (v, b.len()));
        }
        let ssz = vec![7u8; 92];
        let enc = encode_payload(&ssz);
        assert_eq!(enc[0], 92); // varint of the uncompressed length
        assert_eq!(&enc[1..5], &[0xff, 0x06, 0x00, 0x00]); // snappy stream identifier
        assert_eq!(decode_payload(&enc).unwrap(), ssz);
    }

    #[test]
    fn served_envelopes_roundtrip_through_the_codec() {
        use request_response::Codec as _;
        let p = StreamProtocol::new(ENVELOPES_BY_RANGE);
        let mut c = EnvelopesCodec { digest: [0x66, 0x9e, 0x6c, 0x11] };
        futures::executor::block_on(async {
            let mut wire = futures::io::Cursor::new(Vec::new());
            let req = [5u64.to_le_bytes(), 3u64.to_le_bytes()].concat();
            c.write_request(&p, &mut wire, req.clone()).await.unwrap();
            let got = c.read_request(&p, &mut futures::io::Cursor::new(wire.into_inner())).await.unwrap();
            assert_eq!(got, req);

            let chunks = vec![vec![1u8; 300_000], vec![2u8; 7]];
            let mut wire = futures::io::Cursor::new(Vec::new());
            c.write_response(&p, &mut wire, chunks.clone()).await.unwrap();
            let raw = wire.into_inner();
            assert_eq!(&raw[1..5], &[0x66, 0x9e, 0x6c, 0x11], "context bytes = fork digest");
            let back = c.read_response(&p, &mut futures::io::Cursor::new(raw)).await.unwrap();
            assert_eq!(back, chunks);
        });
    }

    #[test]
    fn splits_multi_chunk_responses_with_context_bytes() {
        let mut b = Vec::new();
        for payload in [vec![1u8; 70_000], vec![2u8; 10], vec![]] {
            b.push(0);
            b.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
            b.extend_from_slice(&encode_payload(&payload));
        }
        b.push(3); // an error chunk ends the response
        b.extend_from_slice(&encode_payload(b"unavailable"));
        let chunks = split_context_chunks(&b).unwrap();
        let mut only_err = vec![3u8];
        only_err.extend_from_slice(&encode_payload(b"unavailable"));
        assert!(split_context_chunks(&only_err).unwrap_err().to_string().contains("peer error 3: unavailable"));
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], vec![1u8; 70_000]);
        assert_eq!(chunks[1], vec![2u8; 10]);
        assert!(chunks[2].is_empty());
    }
}
