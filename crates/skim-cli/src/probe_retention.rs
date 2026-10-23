//! `skim probe-retention` (Sepolia): how far back public EL peers still serve history. For blocks
//! spread from the head back to the Glamsterdam fork, asks every connected eth/71 peer for the
//! block's BAL, body and receipts, verifying each against the header, and prints how many peers
//! served each, by block age. Also lists each peer's advertised `earliest` block (eth/69+ Status).

use std::{collections::BTreeMap, time::Duration};

use alloy_consensus::proofs::{calculate_withdrawals_root, ordered_trie_root_with_encoder};
use alloy_consensus::private::alloy_eips::eip2718::Encodable2718;
use alloy_primitives::keccak256;
use futures::future::join_all;
use skim_el::{ElNet, Receipt};

#[derive(clap::Args)]
pub struct Args {
    #[arg(long, default_value_t = 30411)]
    el_port: u16,
    /// Wait for this many eth/71 peers (or `--wait-secs`) before probing.
    #[arg(long, default_value_t = 16)]
    min_peers: usize,
    #[arg(long, default_value_t = 180)]
    wait_secs: u64,
}

/// One block's answers: (BAL verified, BAL unavailable, body verified, receipts verified, asked).
#[derive(Default)]
struct Row {
    bal: usize,
    bal_none: usize,
    body: usize,
    receipts: usize,
    asked: usize,
    bal_clients: BTreeMap<String, usize>,
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    let net = ElNet::start_sepolia(args.el_port, vec![], false).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(args.wait_secs);
    let eth71 = |net: &ElNet| net.peers().into_iter().filter(|(_, p)| p.eth_version >= 71).collect::<Vec<_>>();
    while eth71(&net).len() < args.min_peers && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let peers = eth71(&net);
    anyhow::ensure!(!peers.is_empty(), "no eth/71 peers");
    let client = |c: &str| c.split('/').next().unwrap_or(c).to_string();

    println!("eth/71 peers: {}", peers.len());
    let mut earliest: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for (_, p) in &peers {
        earliest.entry(client(&p.client)).or_default().push(p.status_earliest.unwrap_or(u64::MAX));
    }
    for (c, mut v) in earliest {
        v.sort();
        println!("  {c}: {} peers, advertised earliest block {:?}", v.len(), v);
    }

    let head = peers.iter().filter_map(|(_, p)| p.status_latest).max().ok_or_else(|| anyhow::anyhow!("no peer head"))?;
    let header = |n: u64| {
        let (net, peers) = (&net, &peers);
        async move {
            for (id, _) in peers {
                if let (Ok(h), _) = net.headers(*id, n.into(), 1).await
                    && let Some(h) = h.into_iter().next()
                {
                    return Some(h);
                }
            }
            None
        }
    };

    // The fork: the first block with a BAL commitment (binary search on headers).
    let (mut lo, mut hi) = (0u64, head);
    anyhow::ensure!(header(hi).await.and_then(|h| h.block_access_list_hash).is_some(), "head has no BAL commitment");
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        match header(mid).await {
            Some(h) if h.block_access_list_hash.is_some() => hi = mid,
            Some(_) => lo = mid,
            None => anyhow::bail!("no peer served header {mid}"),
        }
    }
    let fork = hi;
    println!("head {head}, Glamsterdam fork block {fork} ({} blocks ago)", head - fork);

    let mut ages: Vec<u64> = vec![16];
    let mut a = 64;
    while a < head - fork {
        ages.push(a);
        a *= 2;
    }
    let mut blocks: Vec<u64> = ages.iter().map(|a| head - a).collect();
    blocks.extend([fork + 1, fork]);
    println!("{:>9} {:>9} | {:>12} {:>9} {:>9} | BAL served by", "block", "age", "BAL ok/none", "bodies", "receipts");
    for n in blocks {
        let Some(h) = header(n).await else {
            println!("{n:>9}: no peer served the header");
            continue;
        };
        let hash = h.hash_slow();
        let answers = join_all(peers.iter().map(|(id, p)| {
            let (net, h) = (&net, &h);
            async move {
                let bal = match net.bals(*id, vec![hash]).await.0 {
                    Ok(r) => match r.0.into_iter().next().flatten() {
                        Some(b) if Some(keccak256(&b)) == h.block_access_list_hash => Some(true),
                        Some(_) => Some(false),
                        None => None,
                    },
                    Err(_) => None,
                };
                let body = match net.bodies(*id, vec![hash]).await.0 {
                    Ok(b) => b.first().is_some_and(|body| {
                        let raw: Vec<Vec<u8>> = body.transactions.iter().map(|t| t.encoded_2718()).collect();
                        let txr = ordered_trie_root_with_encoder(&raw, |t: &Vec<u8>, buf| buf.extend_from_slice(t));
                        txr == h.transactions_root && body.withdrawals.as_ref().map(|w| calculate_withdrawals_root(w)) == h.withdrawals_root
                    }),
                    Err(_) => false,
                };
                let receipts = match net.block_receipts(*id, hash, h.gas_used).await.0 {
                    Ok(r) if r.is_empty() && h.receipts_root != alloy_consensus::EMPTY_ROOT_HASH => false,
                    Ok(r) => Receipt::calculate_receipt_root_no_memo(&r) == h.receipts_root,
                    Err(_) => false,
                };
                (p.client.clone(), bal, body, receipts)
            }
        }))
        .await;
        let mut row = Row::default();
        for (c, bal, body, receipts) in answers {
            row.asked += 1;
            match bal {
                Some(true) => {
                    row.bal += 1;
                    *row.bal_clients.entry(client(&c)).or_default() += 1;
                }
                Some(false) => println!("  WRONG BAL from {c} at {n}"),
                None => row.bal_none += 1,
            }
            row.body += body as usize;
            row.receipts += receipts as usize;
        }
        println!(
            "{n:>9} {:>9} | {:>5}/{:<6} {:>4}/{:<4} {:>4}/{:<4} | {:?}",
            head - n,
            row.bal,
            row.bal_none,
            row.body,
            row.asked,
            row.receipts,
            row.asked,
            row.bal_clients
        );
    }
    Ok(())
}
