//! TODO 4: a call whose only miss is an ancestor block hash (`BLOCKHASH`) must not wait for a
//! SNAP peer: headers come from any eth peer, and with none it ends in a bounded number of rounds
//! (`Incomplete`), never `NoStatePeers` after `state_wait`.
//!
//! Uses a real `ElNet` restricted to trusted peers with none trusted, so it never gets a peer
//! (no SNAP, no eth): the test needs no network to pass, though the node still binds a local
//! port. A peer that serves headers but not SNAP is stood in for by `Simulator::set_header_source`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use revm::context::TxEnv;
use revm::primitives::{KECCAK_EMPTY, TxKind};
use skim_bal::StateKey;
use revm::context::result::ExecutionResult;
use skim_sim::multi::{Placement, SimBlock, SimCall, TraceBase};
use skim_sim::{Mode, SimError, Simulator, learned::Learned, predictor::Predictor};
use skim_state::{BlockRef, Store};

/// `BLOCKHASH(250)`, returned as one word.
const CODE: [u8; 11] = [0x60, 0xfa, 0x40, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];

/// A simulator whose store holds blocks 290..=300 of a real header chain (240..=300), with every
/// account the call touches local; returns it, the head, the whole chain, and the call.
async fn setup() -> (Arc<Simulator>, BlockRef, Vec<Header>, TxEnv) {
    let net = skim_el::ElNet::start_sepolia(0, vec![], true).await.expect("local node");
    let sched = skim_snap::Scheduler::new(skim_snap::el::ElTransport::new(net), Default::default());
    let store: Arc<Store> = Store::new(64);
    let sim = Simulator::new(
        store.clone(),
        sched,
        Predictor { learned: Learned::with_limits(8, 0.5, 256, Default::default()), v3: vec![], base_words: 2, reach: Default::default() },
    );
    // A state wait far longer than the test may take: reaching it is the failure.
    *sim.state_wait.write() = Duration::from_secs(20);
    let coinbase = Address::repeat_byte(0xcb);
    let mut chain = Vec::new();
    let mut parent = B256::repeat_byte(0x42);
    for n in 240..=300u64 {
        let h = Header { number: n, parent_hash: parent, beneficiary: coinbase, gas_limit: 30_000_000, timestamp: n * 12, base_fee_per_gas: Some(0), ..Default::default() };
        parent = h.hash_slow();
        chain.push(h);
    }
    // Only 290..=300 are in the store (and 289's hash, as 290's parent); 250 is not.
    let mut head = None;
    for h in chain.iter().filter(|h| h.number >= 290) {
        let b = BlockRef { number: h.number, hash: h.hash_slow(), parent_hash: h.parent_hash, state_root: B256::repeat_byte(7), timestamp: h.timestamp };
        store.apply_block(b, Default::default()).unwrap();
        sim.insert_header(h.clone());
        head = Some(b);
    }
    let head = head.unwrap();
    assert_eq!(store.block_hash_at(&head, 250), None, "the only miss will be this block hash");
    // Everything the call touches is local: caller, contract (and its code), fee recipient.
    let (caller, target) = (Address::repeat_byte(0xca), Address::repeat_byte(0x7a));
    store.insert_code(Bytes::from_static(&CODE));
    for (a, code_hash) in [(caller, KECCAK_EMPTY), (target, keccak256(CODE)), (coinbase, KECCAK_EMPTY)] {
        for (k, v) in [(StateKey::Balance(a), U256::from(10u64.pow(18))), (StateKey::Nonce(a), U256::ZERO), (StateKey::CodeHash(a), U256::from_be_bytes(code_hash.0))] {
            store.insert_base(&head, k, v).unwrap();
        }
    }
    let tx = TxEnv::builder().caller(caller).kind(TxKind::Call(target)).gas_limit(1_000_000).gas_price(0).build().unwrap();
    (sim, head, chain, tx)
}

/// An eth peer without SNAP: serves headers falling from a hash, nothing else.
fn eth_only_peer(sim: &Simulator, chain: &[Header]) {
    let by_hash: std::collections::HashMap<B256, Header> = chain.iter().map(|h| (h.hash_slow(), h.clone())).collect();
    let by_hash = Arc::new(by_hash);
    sim.set_header_source(Arc::new(move |from, count| {
        let by_hash = by_hash.clone();
        Box::pin(async move {
            let mut out = Vec::new();
            let mut at = from;
            while (out.len() as u64) < count {
                let Some(h) = by_hash.get(&at) else { break };
                at = h.parent_hash;
                out.push(h.clone());
            }
            Some(out)
        })
    }));
}

/// The call's output: `BLOCKHASH(250)`, one word.
fn word(out: &[u8]) -> B256 {
    B256::from_slice(&out[..32])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blockhash_only_miss_never_waits_for_state_peers() {
    let (sim, head, _, tx) = setup().await;
    let t = Instant::now();
    let r = sim.call(&head, tx, Mode::Resolve { max_rounds: 4 }).await;
    let took = t.elapsed();
    match r {
        // No eth peer can serve the header either: the header rounds run out.
        Err(SimError::Incomplete(_)) => {}
        other => panic!("expected Incomplete after bounded header rounds, got {other:?}"),
    }
    assert!(took < Duration::from_secs(10), "waited {took:?}: it waited for a SNAP peer");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_eth_only_peer_serving_the_header_completes_the_call() {
    let (sim, head, chain, tx) = setup().await;
    eth_only_peer(&sim, &chain);
    let t = Instant::now();
    let out = sim.call(&head, tx, Mode::Resolve { max_rounds: 4 }).await.expect("the header comes from the eth peer");
    assert!(t.elapsed() < Duration::from_secs(10), "waited {:?}: it waited for a SNAP peer", t.elapsed());
    let ExecutionResult::Success { output, .. } = out.result else { panic!("{:?}", out.result) };
    assert_eq!(word(output.data()), chain[10].hash_slow(), "BLOCKHASH(250)");
}

#[tokio::test(flavor = "multi_thread")]
async fn eth_simulate_with_a_header_only_miss_doesnt_wait_for_state_peers() {
    let (sim, head, chain, tx) = setup().await;
    eth_only_peer(&sim, &chain);
    let blocks = [SimBlock { block: None, state: None, calls: vec![SimCall { tx, ..Default::default() }] }];
    let t = Instant::now();
    let out = sim.simulate(&head, &blocks, Placement::NextBlocks).await.expect("simulate completes");
    assert!(t.elapsed() < Duration::from_secs(10), "waited {:?}: it waited for a SNAP peer", t.elapsed());
    let ExecutionResult::Success { output, .. } = &out[0].calls[0] else { panic!("{:?}", out[0].calls[0]) };
    assert_eq!(word(output.data()), chain[10].hash_slow());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trace_with_a_header_only_miss_doesnt_wait_for_state_peers() {
    let (sim, head, chain, tx) = setup().await;
    eth_only_peer(&sim, &chain);
    let base = TraceBase { state_at: head, env: Arc::new(chain.last().unwrap().clone()), prestate: None, overrides: None, block_overrides: None, validating: false };
    let t = Instant::now();
    let (_, result) = sim.trace(&base, tx, Default::default()).await.expect("trace completes");
    assert!(t.elapsed() < Duration::from_secs(10), "waited {:?}: it waited for a SNAP peer", t.elapsed());
    let ExecutionResult::Success { output, .. } = result else { panic!("{result:?}") };
    assert_eq!(word(output.data()), chain[10].hash_slow());
}
