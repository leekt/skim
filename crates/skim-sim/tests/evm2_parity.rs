#![cfg(feature = "evm2")]
//! Offline differential fixtures and an opt-in paired warm-call/estimate CPU benchmark.
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use revm::{
    Context, ExecuteEvm, MainBuilder, MainContext,
    context::{TxEnv, result::ExecutionResult},
    database::WrapDatabaseRef,
    primitives::TxKind,
};
use skim_bal::StateKey;
use skim_sim::{
    Access,
    db::{CodeCache, SnapDb},
    env, evm2_backend,
};
use skim_state::{BlockRef, Snapshot, Store};
use std::{sync::Arc, time::Instant};

struct Fixture {
    store: Arc<Store>,
    header: Header,
    at: BlockRef,
    revm_code: CodeCache,
    evm2_code: CodeCache<evm2::bytecode::Bytecode>,
}
impl Fixture {
    fn new(code: &[u8]) -> (Self, TxEnv) {
        let store = Store::new(8);
        let header = Header {
            number: 100,
            timestamp: 1200,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(7),
            excess_blob_gas: Some(8_000_000),
            beneficiary: Address::repeat_byte(0xcb),
            mix_hash: B256::repeat_byte(0x42),
            ..Default::default()
        };
        let at = BlockRef {
            number: header.number,
            hash: header.hash_slow(),
            parent_hash: header.parent_hash,
            state_root: B256::ZERO,
            timestamp: header.timestamp,
        };
        store.apply_block(at, Default::default()).unwrap();
        let caller = Address::repeat_byte(0xca);
        let target = Address::repeat_byte(0x7a);
        let f = Self {
            store,
            header,
            at,
            revm_code: Default::default(),
            evm2_code: Default::default(),
        };
        f.account(caller, &[], 1_000_000_000, 0);
        f.account(target, code, 10, 1);
        f.account(f.header.beneficiary, &[], 0, 0);
        for slot in 0..4 {
            f.store
                .insert_base(
                    &at,
                    StateKey::Storage(target, U256::from(slot)),
                    U256::from(slot),
                )
                .unwrap();
        }
        let tx = TxEnv {
            caller,
            kind: TxKind::Call(target),
            gas_limit: 5_000_000,
            gas_price: 0,
            ..Default::default()
        };
        (f, tx)
    }
    fn account(&self, address: Address, code: &[u8], balance: u64, nonce: u64) {
        self.store.insert_code(Bytes::copy_from_slice(code));
        for (key, value) in [
            (StateKey::Balance(address), U256::from(balance)),
            (StateKey::Nonce(address), U256::from(nonce)),
            (
                StateKey::CodeHash(address),
                U256::from_be_bytes(keccak256(code).0),
            ),
        ] {
            self.store.insert_base(&self.at, key, value).unwrap();
        }
    }
    fn run(&self, snapshot: &Snapshot, tx: TxEnv, use_evm2: bool) -> (ExecutionResult, Access) {
        if use_evm2 {
            return evm2_backend::execute(
                snapshot,
                &self.header,
                tx,
                &self.store,
                &self.revm_code,
                &self.evm2_code,
            )
            .unwrap();
        }
        let block_hash = |n| self.store.block_hash_at(&self.at, n);
        let db = SnapDb {
            snap: snapshot,
            block_hash: &block_hash,
            access: Default::default(),
            code_cache: &self.revm_code,
        };
        let result = Context::mainnet()
            .with_db(WrapDatabaseRef(&db))
            .with_block(env::block_env(&self.header))
            .with_cfg(env::call_cfg())
            .build_mainnet()
            .transact(tx)
            .unwrap()
            .result;
        let mut access = db.access.into_inner().unwrap();
        access.dedup();
        (result, access)
    }
    fn parity(&self, tx: TxEnv) {
        let snap = self.store.snapshot(&self.at).unwrap();
        let (a, ar) = self.run(&snap, tx.clone(), false);
        let (b, br) = self.run(&snap, tx, true);
        assert_eq!(a, b, "execution result");
        assert_eq!(ar.accounts, br.accounts, "account reads");
        assert_eq!(ar.storage, br.storage, "storage reads");
        assert_eq!(ar.block_hashes, br.block_hashes, "blockhash reads");
        assert_eq!(ar.miss_accounts, br.miss_accounts, "missing accounts");
        assert_eq!(ar.miss_storage, br.miss_storage, "missing storage");
        assert_eq!(ar.miss_code, br.miss_code, "missing code");
        assert_eq!(
            ar.miss_block_hashes, br.miss_block_hashes,
            "missing headers"
        );
    }
}

#[test]
fn execution_output_gas_logs_and_accesses_match() {
    for code in [
        vec![0x00],                                                 // STOP
        vec![0x60, 0x01, 0x54, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3], // storage
        vec![0x60, 0x42, 0x5f, 0x55, 0x00],                         // EIP-8037 state creation
        vec![0x5f, 0x60, 0x01, 0x55, 0x00],                         // refund
        vec![0x60, 0x42, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xa0, 0x00], // log
        vec![0x60, 0x42, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xfd],       // revert
        vec![0xfe],
        vec![0x50],
        vec![0x60, 0x01, 0x56],                         // halt reasons
        vec![0x46, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3], // CHAINID
        vec![0x48, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3], // BASEFEE
        vec![0x4a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3], // BLOBBASEFEE
        vec![0x60, 0x63, 0x40, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3], // parent BLOCKHASH
        vec![0x5b, 0x5f, 0x56],                         // gas exhaustion
    ] {
        let (f, mut tx) = Fixture::new(&code);
        tx.gas_limit = 200_000;
        f.parity(tx);
    }
}

#[test]
fn envelope_types_and_create_match() {
    let (f, tx) = Fixture::new(&[0x60, 1, 0x54, 0x5f, 0x52, 0x60, 32, 0x5f, 0xf3]);
    for ty in 0..=2 {
        let mut tx = tx.clone();
        tx.tx_type = ty;
        tx.chain_id = Some(env::chain_id());
        if ty > 0 {
            tx.access_list
                .0
                .push(revm::context::transaction::AccessListItem {
                    address: *tx.kind.to().unwrap(),
                    storage_keys: vec![B256::with_last_byte(1)],
                });
        }
        if ty == 2 {
            tx.gas_priority_fee = Some(0);
        }
        f.parity(tx);
    }
    let mut create = tx;
    create.kind = TxKind::Create;
    create.data = Bytes::from_static(&[0x60, 0, 0x5f, 0x53, 0x60, 1, 0x5f, 0xf3]);
    let created = create.caller.create(create.nonce);
    f.account(created, &[], 0, 0);
    f.parity(create);
}

#[test]
fn precompiles_and_missing_storage_match() {
    for id in [1, 2, 3, 4, 5, 6, 7, 8, 9] {
        let (f, mut tx) = Fixture::new(&[]);
        tx.kind = TxKind::Call(Address::with_last_byte(id));
        f.account(*tx.kind.to().unwrap(), &[], 0, 0);
        f.parity(tx);
    }
    let (f, tx) = Fixture::new(&[0x60, 0x7f, 0x54, 0x00]);
    f.parity(tx);
}

#[test]
fn blob_set_code_and_value_transfer_logs_match() {
    let (f, tx) = Fixture::new(&[0x60, 0x42, 0x5f, 0x52, 0x60, 32, 0x5f, 0xf3]);
    let mut blob = tx.clone();
    blob.tx_type = 3;
    blob.chain_id = Some(env::chain_id());
    blob.blob_hashes = vec![B256::repeat_byte(1)];
    blob.max_fee_per_blob_gas = 1_000_000;
    blob.gas_priority_fee = Some(0);
    f.parity(blob);
    let authority = Address::repeat_byte(0xa7);
    f.account(authority, &[], 0, 0);
    let mut delegated = tx.clone();
    delegated.tx_type = 4;
    delegated.chain_id = Some(env::chain_id());
    delegated.gas_priority_fee = Some(0);
    delegated.kind = TxKind::Call(authority);
    use revm::context::transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization};
    let authorization = Authorization {
        chain_id: U256::from(env::chain_id()),
        address: *tx.kind.to().unwrap(),
        nonce: 0,
    };
    delegated.authorization_list = vec![revm::context_interface::either::Either::Right(
        RecoveredAuthorization::new_unchecked(authorization, RecoveredAuthority::Valid(authority)),
    )];
    f.parity(delegated);
    for code in [vec![0x00], vec![0x5f, 0x5f, 0xfd], vec![0xfe]] {
        let (f, mut value_tx) = Fixture::new(&code);
        value_tx.value = U256::from(100);
        f.parity(value_tx);
    }
}

fn estimate(f: &Fixture, snap: &Snapshot, tx: &TxEnv, use_evm2: bool) -> u64 {
    let (first, _) = f.run(snap, tx.clone(), use_evm2);
    assert!(first.is_success());
    let (mut lo, mut hi) = (first.tx_gas_used().saturating_sub(1), tx.gas_limit);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        let mut probe = tx.clone();
        probe.gas_limit = mid;
        if f.run(snap, probe, use_evm2).0.is_success() {
            hi = mid
        } else {
            lo = mid
        }
    }
    hi
}

#[test]
fn estimate_binary_search_matches() {
    let (f, tx) = Fixture::new(&arithmetic());
    let snap = f.store.snapshot(&f.at).unwrap();
    assert_eq!(
        estimate(&f, &snap, &tx, false),
        estimate(&f, &snap, &tx, true)
    );
}

fn arithmetic() -> Vec<u8> {
    // Countdown from 1000. Each loop executes arithmetic, stack and conditional-jump work.
    vec![
        0x61, 0x03, 0xe8, 0x5b, 0x60, 1, 0x90, 0x03, 0x80, 0x60, 3, 0x57, 0x5f, 0x52, 0x60, 32,
        0x5f, 0xf3,
    ]
}

#[test]
#[ignore = "paired CPU benchmark; run optimized with --ignored --nocapture"]
fn benchmark_warm_calls_and_estimates() {
    for (name, code) in [
        ("arithmetic1000", arithmetic()),
        (
            "sload",
            vec![0x60, 1, 0x54, 0x5f, 0x52, 0x60, 32, 0x5f, 0xf3],
        ),
    ] {
        let (f, tx) = Fixture::new(&code);
        let snap = f.store.snapshot(&f.at).unwrap();
        f.parity(tx.clone());
        for estimates in [false, true] {
            let count = if estimates { 30 } else { 500 };
            let mut samples = [Vec::new(), Vec::new()];
            for round in 0..7 {
                for index in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
                    let t = Instant::now();
                    for _ in 0..count {
                        if estimates {
                            std::hint::black_box(estimate(&f, &snap, &tx, index == 1));
                        } else {
                            std::hint::black_box(f.run(&snap, tx.clone(), index == 1));
                        }
                    }
                    samples[index].push(t.elapsed().as_secs_f64() * 1e6 / count as f64);
                }
            }
            for s in &mut samples {
                s.sort_by(f64::total_cmp);
            }
            println!(
                "{name} {} revm_us={:.3} evm2_us={:.3} speedup={:.3} samples={:?}",
                if estimates { "estimate" } else { "call" },
                samples[0][3],
                samples[1][3],
                samples[0][3] / samples[1][3],
                samples
            );
        }
    }
}
