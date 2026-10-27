#![cfg(feature = "evm2-async")]
//! Controlled async-I/O experiment. This is intentionally not the production SNAP adapter:
//! demand-at-a-time fetching can split one predicted batch into several network round trips.
use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use evm2::{
    BaseEvmTypes, Evm, ExecutionConfig, SpecId,
    bytecode::Bytecode,
    ethereum::{TxEnvelope, ethereum_tx_registry},
    evm::{
        AccountInfo, DynDatabase, InMemoryDB,
        r#async::{AsyncDatabase, AsyncDb},
    },
};
use skim_sim::evm2_bridge;
use std::{
    collections::HashSet,
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

struct DemandDb {
    db: InMemoryDB,
    slots: HashSet<U256>,
    fetched: Arc<AtomicUsize>,
    code_reads: Arc<AtomicUsize>,
    delay: Duration,
    hang: bool,
}
impl AsyncDatabase for DemandDb {
    type Error = Infallible;
    async fn get_account(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(DynDatabase::get_account(&mut self.db, &address).unwrap())
    }
    async fn get_code_by_hash(&mut self, hash: B256) -> Result<Bytecode, Self::Error> {
        self.code_reads.fetch_add(1, Ordering::Relaxed);
        Ok(DynDatabase::get_code_by_hash(&mut self.db, &hash).unwrap())
    }
    async fn get_storage(&mut self, _address: Address, key: U256) -> Result<U256, Self::Error> {
        if self.slots.insert(key) {
            self.fetched.fetch_add(1, Ordering::Relaxed);
            if self.hang {
                std::future::pending::<()>().await;
            }
            if self.delay.is_zero() {
                tokio::task::yield_now().await;
            } else {
                tokio::time::sleep(self.delay).await;
            }
        }
        Ok(U256::from(42))
    }
    async fn get_block_hash(&mut self, number: U256) -> Result<B256, Self::Error> {
        Ok(DynDatabase::get_block_hash(&mut self.db, &number).unwrap())
    }
}
fn setup(
    hang: bool,
    delay: Duration,
) -> (
    Evm<'static, BaseEvmTypes>,
    Recovered<TxEnvelope>,
    Address,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let caller = Address::repeat_byte(0xca);
    let target = Address::repeat_byte(0x7a);
    // A long arithmetic prefix, then three distinct missing storage slots.
    let code = Bytecode::new_legacy(Bytes::from(vec![
        0x61, 3, 0xe8, 0x5b, 0x60, 1, 0x90, 3, 0x80, 0x60, 3, 0x57, 0x50, 0x5f, 0x54, 0x50, 0x60,
        1, 0x54, 0x50, 0x60, 2, 0x54, 0x5f, 0x52, 0x60, 32, 0x5f, 0xf3,
    ]));
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &caller,
        AccountInfo::default().with_balance(U256::from(1_000_000)),
    );
    db.insert_account_info(
        &target,
        AccountInfo::new(U256::ZERO, 1, code.hash_slow(), None),
    );
    let mut cached_code = AccountInfo::default().with_code(code);
    db.insert_contract(&mut cached_code);
    let fetched = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let database = DemandDb {
        db,
        slots: Default::default(),
        fetched: fetched.clone(),
        code_reads: reads.clone(),
        delay,
        hang,
    };
    let spec = SpecId::AMSTERDAM;
    let config = ExecutionConfig::<BaseEvmTypes>::for_spec_and_version(
        spec,
        evm2_bridge::version(spec, &skim_sim::env::call_cfg()),
    );
    let evm = Evm::<BaseEvmTypes>::new_with_execution_config(
        config,
        spec,
        Default::default(),
        ethereum_tx_registry(spec),
        AsyncDb::new(database),
        evm2::Precompiles::base(spec),
    );
    let tx = Recovered::new_unchecked(
        TxEnvelope::Legacy(TxLegacy {
            gas_limit: 1_000_000,
            to: TxKind::Call(target),
            ..Default::default()
        }),
        caller,
    );
    (evm, tx, caller, fetched, reads)
}

#[tokio::test]
async fn storage_waits_resume_one_execution_and_discard_writes() {
    let (mut evm, tx, caller, fetched, reads) = setup(false, Duration::ZERO);
    let result = evm.transact_async(&tx).await.unwrap().discard();
    assert!(result.status);
    assert_eq!(U256::from_be_slice(&result.output), U256::from(42));
    assert_eq!(fetched.load(Ordering::Relaxed), 3);
    assert_eq!(
        reads.load(Ordering::Relaxed),
        1,
        "three awaits must not restart execution"
    );
    assert_eq!(
        evm.read_account_info(&caller).unwrap().unwrap().nonce,
        0,
        "eth_call must discard the sender nonce"
    );
}

#[tokio::test]
async fn cancellation_discards_the_incomplete_transaction() {
    let (mut evm, tx, caller, fetched, _) = setup(true, Duration::ZERO);
    let timed_out = tokio::time::timeout(Duration::from_millis(5), async {
        evm.transact_async(&tx).await.unwrap().discard()
    })
    .await;
    assert!(timed_out.is_err());
    assert_eq!(fetched.load(Ordering::Relaxed), 1);
    assert_eq!(evm.read_account_info(&caller).unwrap().unwrap().nonce, 0);
}

#[tokio::test]
#[ignore = "controlled timing experiment; synthetic latency is not a live SNAP measurement"]
async fn demand_io_preserves_cpu_prefix_but_serializes_independent_misses() {
    let delay = Duration::from_millis(10);
    let (mut evm, tx, _, fetched, reads) = setup(false, delay);
    let start = Instant::now();
    assert!(evm.transact_async(&tx).await.unwrap().discard().status);
    let demand = start.elapsed();
    let start = Instant::now();
    futures::future::join_all((0..3).map(|_| tokio::time::sleep(delay))).await;
    println!(
        "synthetic demand={demand:?} batched_wait={:?} fetches={} code_reads={}",
        start.elapsed(),
        fetched.load(Ordering::Relaxed),
        reads.load(Ordering::Relaxed)
    );
    assert_eq!(fetched.load(Ordering::Relaxed), 3);
    assert_eq!(reads.load(Ordering::Relaxed), 1);
}
