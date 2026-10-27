//! Opt-in EVM2 plain-call backend. State discovery and verification remain in Simulator.
use crate::{
    Access, SimError,
    db::{CodeCache, SnapDb},
    env, evm2_bridge,
};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, U256};
use evm2::{
    BaseEvmTypes, Evm, ExecutionConfig, SpecId,
    bytecode::Bytecode,
    ethereum::ethereum_tx_registry,
    evm::{AccountInfo, Database, Db},
};
use revm::{
    DatabaseRef,
    context::{TxEnv, result::ExecutionResult},
};
use skim_state::{Snapshot, Store};
use std::sync::Arc;

pub fn supports(tx: &TxEnv) -> bool {
    tx.tx_type <= 4 && (tx.tx_type < 3 || !tx.kind.is_create())
}

struct DatabaseAdapter<'a> {
    db: &'a SnapDb<'a>,
    code: &'a CodeCache<Bytecode>,
}

impl Database for DatabaseAdapter<'_> {
    type Error = std::convert::Infallible;
    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.db
            .basic_ref(*address)
            .map(|a| a.map(|a| AccountInfo::new(a.balance, a.nonce, a.code_hash, None)))
    }
    fn get_code_by_hash(&mut self, hash: &B256) -> Result<Bytecode, Self::Error> {
        if let Some(code) = self.code.get(hash) {
            return Ok(code);
        }
        if let Some(code) = self.db.snap.code(hash) {
            let code = Bytecode::new_raw(code);
            self.code.insert(*hash, code.clone());
            Ok(code)
        } else {
            self.db.access.lock().unwrap().miss_code.push(*hash);
            Ok(Bytecode::default())
        }
    }
    fn get_storage(&mut self, address: &Address, key: &U256) -> Result<U256, Self::Error> {
        self.db.storage_ref(*address, *key)
    }
    fn get_block_hash(&mut self, number: &U256) -> Result<B256, Self::Error> {
        self.db.block_hash_ref(number.saturating_to())
    }
}

pub fn execute(
    snap: &Snapshot,
    header: &Header,
    tx: TxEnv,
    store: &Arc<Store>,
    revm_code: &CodeCache,
    code: &CodeCache<Bytecode>,
) -> Result<(ExecutionResult, Access), SimError> {
    let at = *snap.block();
    let block_hash = |n| store.block_hash_at(&at, n);
    let db = SnapDb {
        snap,
        block_hash: &block_hash,
        access: Default::default(),
        code_cache: revm_code,
    };
    let spec = SpecId::AMSTERDAM;
    let cfg = ExecutionConfig::<BaseEvmTypes>::for_spec_and_version(
        spec,
        evm2_bridge::version(spec, &env::call_cfg()),
    );
    let mut evm = Evm::<BaseEvmTypes>::new_with_execution_config(
        cfg,
        spec,
        evm2_bridge::block(&env::block_env(header)),
        ethereum_tx_registry(spec),
        Db::new(DatabaseAdapter { db: &db, code }),
        evm2::Precompiles::base(spec),
    );
    let envelope = evm2_bridge::transaction(&tx).ok_or(SimError::Unsupported("EVM2 envelope"))?;
    let result = evm
        .transact(&envelope)
        .map_err(|e| SimError::Evm(format!("evm2: {e:?}")))?
        .discard();
    drop(evm);
    let result = if result.stop == evm2::interpreter::InstrStop::PrecompileError {
        // EVM2 drops the precompile error text. revm's RPC-visible halt includes
        // that context, so ask the reference engine for this uncommon result.
        use revm::{ExecuteEvm, MainBuilder, MainContext};
        revm::Context::mainnet()
            .with_db(revm::database::WrapDatabaseRef(&db))
            .with_block(env::block_env(header))
            .with_cfg(env::call_cfg())
            .build_mainnet()
            .transact(tx)
            .map_err(|e| SimError::Evm(format!("{e:?}")))?
            .result
    } else {
        evm2_bridge::result(result, tx.kind.is_create()).map_err(SimError::Evm)?
    };
    let mut access = db.access.into_inner().unwrap();
    access.dedup();
    Ok((result, access))
}
