//! `skim probe-serve`: checks what a skim node serves to the network. Connects to that node only
//! (EL as the sole trusted peer, CL as an explicit peer) and requests recent headers, bodies,
//! receipts, access lists, bytecode and payload envelopes, verifying every answer against the
//! headers it got.

use std::time::Duration;

use alloy_consensus::proofs::{calculate_withdrawals_root, ordered_trie_root_with_encoder};
use alloy_consensus::private::alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{B256, keccak256};
use skim_cl::{ClNode, NodeConfig, ssz::Envelope, verify};
use skim_el::{ElNet, Receipt};

#[derive(clap::Args)]
pub struct Args {
    /// The node's EL enode (from `skim_status.served.el_enode`, with a reachable address).
    #[arg(long)]
    enode: String,
    /// The node's CL multiaddr including `/p2p/<peer id>`.
    #[arg(long)]
    cl_addr: Option<String>,
    /// A code hash the node holds (bytecode request).
    #[arg(long)]
    code_hash: Option<B256>,
    #[arg(long, default_value_t = 30399)]
    el_port: u16,
    #[arg(long, default_value_t = 9330)]
    cl_port: u16,
    /// Recent blocks to request.
    #[arg(long, default_value_t = 32)]
    blocks: u64,
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    let trusted = vec![args.enode.parse().map_err(|e| anyhow::anyhow!("enode: {e:?}"))?];
    let net = ElNet::start_sepolia(args.el_port, trusted, true).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let (peer, info) = loop {
        if let Some(p) = net.peers().into_iter().next() {
            break p;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    println!("EL session: {} eth/{} snap2={}", info.client, info.eth_version, info.snap2);
    let latest = info.status_latest.ok_or_else(|| anyhow::anyhow!("peer advertised no block range"))?;

    // Headers: falling from the advertised head, must be hash-linked.
    let (hs, d) = net.headers_falling(peer, latest.into(), args.blocks).await;
    let hs = hs.map_err(|e| anyhow::anyhow!("headers: {e}"))?;
    let linked = hs.windows(2).all(|w| w[0].parent_hash == w[1].hash_slow());
    println!("headers: {} from {latest} down, linked={linked} ({} ms)", hs.len(), d.as_millis());
    anyhow::ensure!(!hs.is_empty() && linked, "headers missing or not linked");
    let hashes: Vec<B256> = hs.iter().map(|h| h.hash_slow()).collect();

    // BALs: each must hash to its header's commitment.
    let (r, d) = net.bals(peer, hashes.clone()).await;
    let r = r.map_err(|e| anyhow::anyhow!("bals: {e}"))?;
    let (mut ok, mut bad, mut none) = (0, 0, 0);
    for (raw, h) in r.0.iter().zip(&hs) {
        match raw {
            Some(b) if Some(keccak256(b)) == h.block_access_list_hash => ok += 1,
            Some(_) => bad += 1,
            None => none += 1,
        }
    }
    println!("bals: {ok} verified, {none} unavailable, {bad} WRONG of {} ({} ms)", hashes.len(), d.as_millis());

    // Bodies: a prefix of the request; transactions and withdrawals checked against the header.
    let (b, d) = net.bodies(peer, hashes.clone()).await;
    let b = b.map_err(|e| anyhow::anyhow!("bodies: {e}"))?;
    let (mut ok, mut bad) = (0, 0);
    for (body, h) in b.iter().zip(&hs) {
        let raw: Vec<Vec<u8>> = body.transactions.iter().map(|t| t.encoded_2718()).collect();
        let txr = ordered_trie_root_with_encoder(&raw, |t: &Vec<u8>, buf| buf.extend_from_slice(t));
        let wr = body.withdrawals.as_ref().map(|w| calculate_withdrawals_root(w));
        if txr == h.transactions_root && wr == h.withdrawals_root { ok += 1 } else { bad += 1 }
    }
    println!("bodies: {ok} verified, {bad} WRONG, {} returned of {} ({} ms)", b.len(), hashes.len(), d.as_millis());

    // Receipts: only blocks the node fetched for its own users are held.
    let (mut ok, mut bad, mut none) = (0, 0, 0);
    for (hash, h) in hashes.iter().zip(&hs).take(8) {
        match net.block_receipts(peer, *hash).await.0 {
            Ok(r) if r.is_empty() && h.receipts_root != alloy_consensus::EMPTY_ROOT_HASH => none += 1,
            Ok(r) if Receipt::calculate_receipt_root_no_memo(&r) == h.receipts_root => ok += 1,
            Ok(_) => bad += 1,
            Err(_) => none += 1,
        }
    }
    println!("receipts (8 newest): {ok} verified, {none} not held, {bad} WRONG");

    if let Some(c) = args.code_hash {
        let (m, d) = net.byte_codes(peer, vec![c, B256::repeat_byte(0x11)], 1 << 20).await;
        let m = m.map_err(|e| anyhow::anyhow!("bytecodes: {e}"))?;
        let ok = m.codes.iter().filter(|x| keccak256(x) == c).count();
        println!("bytecodes: {} returned, {ok} match the requested hash ({} ms)", m.codes.len(), d.as_millis());
    }

    let Some(addr) = args.cl_addr else { return Ok(()) };
    let mut cfg = NodeConfig::sepolia(args.cl_port);
    cfg.target_peers = 0;
    cfg.topics = vec![];
    cfg.explicit_peers = vec![addr.parse()?];
    let (cl, _rx) = ClNode::start(cfg).await?;
    let by_hash: std::collections::HashMap<B256, u64> = hs.iter().map(|h| (h.hash_slow(), h.number)).collect();
    let check = |v: &[Vec<u8>]| {
        let (mut ok, mut bad, mut roots) = (0, 0, vec![]);
        for e in v {
            match Envelope::parse(e) {
                Ok(env) if verify::verify_payload(&env).ok && by_hash.contains_key(&env.payload.block_hash) => {
                    ok += 1;
                    roots.push((env.payload.slot_number, env.beacon_block_root));
                }
                _ => bad += 1,
            }
        }
        (ok, bad, roots)
    };
    let slot = NodeConfig::sepolia(0).current_slot();
    let start = slot.saturating_sub(16);
    let mut got = None;
    for attempt in 0..15 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match cl.envelopes_by_range(start, 16).await {
            Ok(v) => {
                got = Some(v);
                break;
            }
            Err(e) => println!("envelopes by range attempt {attempt}: {e}"),
        }
    }
    let v = got.ok_or_else(|| anyhow::anyhow!("no envelopes-by-range answer"))?;
    let (ok, bad, roots) = check(&v);
    println!("envelopes by range {start}..+16: {} returned, {ok} verified and on the EL chain, {bad} WRONG/unknown", v.len());
    if let Some((s, r)) = roots.first() {
        let v = cl.envelopes_by_root(vec![*r, B256::repeat_byte(0x22)]).await?;
        let (ok, bad, _) = check(&v);
        println!("envelopes by root (slot {s} + an unknown root): {} returned, {ok} verified, {bad} WRONG", v.len());
    }
    Ok(())
}
