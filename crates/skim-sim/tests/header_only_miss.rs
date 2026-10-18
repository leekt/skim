//! TODO 4: a call whose only miss is an ancestor block hash (`BLOCKHASH`) must not wait for a
//! SNAP peer: headers come from any eth peer, and with none it ends in a bounded number of rounds
//! (`Incomplete`), never `NoStatePeers` after `state_wait`.
//!
//! Uses a real `ElNet` restricted to trusted peers with none trusted, so it never gets a peer
//! (no SNAP, no eth): the test needs no network to pass, though the node still binds a local
//! port.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use revm::context::TxEnv;
use revm::primitives::{KECCAK_EMPTY, TxKind};
use skim_bal::StateKey;
use skim_sim::{Mode, SimError, Simulator, learned::Learned, predictor::Predictor};
use skim_state::{BlockRef, Store};

/// `BLOCKHASH(250)`, returned as one word.
const CODE: [u8; 11] = [0x60, 0xfa, 0x40, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];

#[tokio::test(flavor = "multi_thread")]
async fn a_blockhash_only_miss_never_waits_for_state_peers() {
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

    // Blocks 290..=300: only their hashes (and 289's) are known; 250 is not.
    let coinbase = Address::repeat_byte(0xcb);
    let mut parent = B256::repeat_byte(0x42);
    let mut head = None;
    for n in 290..=300u64 {
        let h = Header { number: n, parent_hash: parent, beneficiary: coinbase, gas_limit: 30_000_000, timestamp: n * 12, base_fee_per_gas: Some(0), ..Default::default() };
        let hash = h.hash_slow();
        let b = BlockRef { number: n, hash, parent_hash: parent, state_root: B256::repeat_byte(7), timestamp: n * 12 };
        store.apply_block(b, Default::default()).unwrap();
        sim.insert_header(h);
        parent = hash;
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
