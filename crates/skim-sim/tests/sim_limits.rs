//! Review: `eth_simulateV1` had no limit on blocks, calls or total gas, so one request could
//! buy execution without bound (each call is bounded by revm's per-transaction cap, but not
//! their number), holding a CPU slot.
//!
//! Uses a real `ElNet` restricted to trusted peers with none trusted: no network is needed, as
//! everything the calls touch is local.

use std::sync::Arc;

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rpc_types_eth::BlockOverrides;
use revm::context::TxEnv;
use revm::primitives::{KECCAK_EMPTY, TxKind};
use skim_bal::StateKey;
use skim_sim::multi::{MAX_SIM_BLOCKS, Placement, SimBlock, SimCall};
use skim_sim::{SimError, Simulator, learned::Learned, predictor::Predictor};
use skim_state::{BlockRef, Store};

/// `JUMPDEST; PUSH1 0; JUMP`: loops until it runs out of gas.
const LOOP: [u8; 4] = [0x5b, 0x60, 0x00, 0x56];
/// `BLOCKHASH(101)`, returned as one word: the first simulated block after base block 100.
const HASH_OF_101: [u8; 11] = [0x60, 0x65, 0x40, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];
const HASHER: Address = Address::repeat_byte(0x7b);

async fn setup() -> (Arc<Simulator>, BlockRef, TxEnv) {
    let net = skim_el::ElNet::start_sepolia(0, vec![], true).await.expect("local node");
    let sched = skim_snap::Scheduler::new(skim_snap::el::ElTransport::new(net), Default::default());
    let store: Arc<Store> = Store::new(64);
    let sim = Simulator::new(
        store.clone(),
        sched,
        Predictor { learned: Learned::with_limits(8, 0.5, 256, Default::default()), v3: vec![], base_words: 2, reach: Default::default() },
    );
    let coinbase = Address::repeat_byte(0xcb);
    let h = Header { number: 100, beneficiary: coinbase, gas_limit: 30_000_000, timestamp: 1200, base_fee_per_gas: Some(0), ..Default::default() };
    let head = BlockRef { number: 100, hash: h.hash_slow(), parent_hash: h.parent_hash, state_root: B256::repeat_byte(7), timestamp: h.timestamp };
    store.apply_block(head, Default::default()).unwrap();
    sim.insert_header(h);
    let (caller, target) = (Address::repeat_byte(0xca), Address::repeat_byte(0x7a));
    store.insert_code(Bytes::from_static(&LOOP));
    store.insert_code(Bytes::from_static(&HASH_OF_101));
    for (a, code_hash) in [(caller, KECCAK_EMPTY), (target, keccak256(LOOP)), (coinbase, KECCAK_EMPTY), (HASHER, keccak256(HASH_OF_101))] {
        for (k, v) in [(StateKey::Balance(a), U256::from(10u64.pow(18))), (StateKey::Nonce(a), U256::ZERO), (StateKey::CodeHash(a), U256::from_be_bytes(code_hash.0))] {
            store.insert_base(&head, k, v).unwrap();
        }
    }
    let tx = TxEnv::builder().caller(caller).kind(TxKind::Call(target)).gas_limit(30_000_000).gas_price(0).build().unwrap();
    (sim, head, tx)
}

fn auto_gas(tx: &TxEnv) -> SimCall {
    SimCall { tx: tx.clone(), auto_nonce: true, auto_fee: true, auto_gas: true }
}

#[tokio::test(flavor = "multi_thread")]
async fn all_calls_share_one_gas_budget() {
    let (sim, head, tx) = setup().await;
    // Each looping call burns what revm lets one transaction execute (2^24, EIP-7825), even with
    // a raised block gas limit; the budget is four 30M calls' worth (120M), so ten calls can't
    // all run, and five can.
    let o = BlockOverrides { gas_limit: Some(u64::MAX), ..Default::default() };
    let blocks: Vec<SimBlock> = (0..10).map(|_| SimBlock { block: Some(o.clone()), state: None, calls: vec![auto_gas(&tx)] }).collect();
    match sim.simulate(&head, &blocks, Placement::NextBlocks).await {
        Err(SimError::Invalid { code: -38026, .. }) => {}
        other => panic!("expected the budget to run out, got {other:?}"),
    }
    let out = sim.simulate(&head, &blocks[..5], Placement::NextBlocks).await.expect("five fit");
    assert!(out.iter().all(|b| b.calls[0].tx_gas_used() <= 30_000_000));
}

#[tokio::test(flavor = "multi_thread")]
async fn too_many_blocks_are_refused_before_anything_runs() {
    let (sim, head, _) = setup().await;
    let blocks: Vec<SimBlock> = (0..=MAX_SIM_BLOCKS).map(|_| SimBlock { block: None, state: None, calls: vec![] }).collect();
    assert!(matches!(sim.simulate(&head, &blocks, Placement::NextBlocks).await, Err(SimError::Invalid { code: -38026, .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn simulated_blocks_follow_each_other_and_answer_blockhash_for_earlier_ones() {
    // Review: a block's time came from the base block whatever the block before it was
    // overridden to, and BLOCKHASH of an earlier simulated block was a miss no fetch could fill
    // ("state still incomplete").
    let (sim, head, tx) = setup().await;
    let o = BlockOverrides { time: Some(head.timestamp + 100), ..Default::default() };
    let mut call = tx.clone();
    call.kind = TxKind::Call(HASHER);
    call.gas_limit = 100_000;
    let blocks = [
        SimBlock { block: Some(o), state: None, calls: vec![] },
        SimBlock { block: None, state: None, calls: vec![SimCall { tx: call, ..Default::default() }] },
    ];
    let out = sim.simulate(&head, &blocks, Placement::NextBlocks).await.expect("simulates");
    assert_eq!(out[1].env.timestamp, U256::from(head.timestamp + 112), "12 s after the overridden block");
    assert_eq!(out[1].env.number, U256::from(102));
    let r = &out[1].calls[0];
    assert!(r.is_success(), "{r:?}");
    assert_ne!(B256::from_slice(&r.output().unwrap()[..32]), B256::ZERO, "a stand-in hash, never zero");
}
