//! Explicit conversion boundary between revm 43 and the pinned EVM2 revision.
use alloy_consensus::{
    TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxLegacy, transaction::Recovered,
};
use alloy_primitives::U256;
use evm2::{
    EvmFeatures, Version,
    ethereum::{LazyTxEip7702, TxEnvelope},
    interpreter::InstrStop,
};
use revm::{
    context::{
        BlockEnv, CfgEnv, TxEnv,
        result::{ExecutionResult, HaltReason, OutOfGasError, Output, ResultGas, SuccessReason},
    },
    primitives::TxKind,
};

/// Unsupported envelope shapes stay on revm rather than losing transaction fields.
pub fn transaction(tx: &TxEnv) -> Option<Recovered<TxEnvelope>> {
    let chain_id = tx.chain_id.unwrap_or_default();
    let envelope = match tx.tx_type {
        0 => TxEnvelope::Legacy(TxLegacy {
            chain_id: tx.chain_id,
            nonce: tx.nonce,
            gas_price: tx.gas_price,
            gas_limit: tx.gas_limit,
            to: tx.kind,
            value: tx.value,
            input: tx.data.clone(),
        }),
        1 => TxEnvelope::Eip2930(TxEip2930 {
            chain_id,
            nonce: tx.nonce,
            gas_price: tx.gas_price,
            gas_limit: tx.gas_limit,
            to: tx.kind,
            value: tx.value,
            access_list: tx.access_list.clone(),
            input: tx.data.clone(),
        }),
        2 => TxEnvelope::Eip1559(TxEip1559 {
            chain_id,
            nonce: tx.nonce,
            max_fee_per_gas: tx.gas_price,
            max_priority_fee_per_gas: tx.gas_priority_fee.unwrap_or(tx.gas_price),
            gas_limit: tx.gas_limit,
            to: tx.kind,
            value: tx.value,
            access_list: tx.access_list.clone(),
            input: tx.data.clone(),
        }),
        3 => {
            let TxKind::Call(to) = tx.kind else {
                return None;
            };
            TxEnvelope::Eip4844(
                TxEip4844 {
                    chain_id,
                    nonce: tx.nonce,
                    max_fee_per_gas: tx.gas_price,
                    max_priority_fee_per_gas: tx.gas_priority_fee.unwrap_or(tx.gas_price),
                    gas_limit: tx.gas_limit,
                    to,
                    value: tx.value,
                    access_list: tx.access_list.clone(),
                    blob_versioned_hashes: tx.blob_hashes.clone(),
                    max_fee_per_blob_gas: tx.max_fee_per_blob_gas,
                    input: tx.data.clone(),
                }
                .into(),
            )
        }
        4 => {
            let TxKind::Call(to) = tx.kind else {
                return None;
            };
            let auth = tx
                .authorization_list
                .iter()
                .cloned()
                .map(|a| match a {
                    revm::context_interface::either::Either::Left(a) => a.into(),
                    revm::context_interface::either::Either::Right(a) => a.into(),
                })
                .collect();
            TxEnvelope::Eip7702(LazyTxEip7702::from_authorizations(
                TxEip7702 {
                    chain_id,
                    nonce: tx.nonce,
                    max_fee_per_gas: tx.gas_price,
                    max_priority_fee_per_gas: tx.gas_priority_fee.unwrap_or(tx.gas_price),
                    gas_limit: tx.gas_limit,
                    to,
                    value: tx.value,
                    access_list: tx.access_list.clone(),
                    authorization_list: vec![],
                    input: tx.data.clone(),
                },
                auth,
            ))
        }
        _ => return None,
    };
    Some(Recovered::new_unchecked(envelope, tx.caller))
}

pub fn block(block: &BlockEnv) -> evm2::env::BlockEnv<evm2::BaseEvmTypes> {
    evm2::env::BlockEnv::<evm2::BaseEvmTypes> {
        number: block.number,
        beneficiary: block.beneficiary,
        timestamp: block.timestamp,
        gas_limit: U256::from(block.gas_limit),
        basefee: U256::from(block.basefee),
        difficulty: block.difficulty,
        prevrandao: block.prevrandao.unwrap_or_default().into(),
        blob_basefee: U256::from(
            block
                .blob_excess_gas_and_price
                .map_or(0, |b| b.blob_gasprice),
        ),
        slot_num: U256::from(block.slot_num),
        ..Default::default()
    }
}

pub fn version(spec: evm2::SpecId, cfg: &CfgEnv) -> Version {
    let mut version = Version::new(spec);
    version.chain_id = cfg.chain_id;
    for (feature, enabled) in [
        (EvmFeatures::NONCE_CHECK, !cfg.disable_nonce_check),
        (EvmFeatures::BALANCE_CHECK, !cfg.disable_balance_check),
        (EvmFeatures::BASE_FEE_CHECK, !cfg.disable_base_fee),
        (EvmFeatures::EIP3607, !cfg.disable_eip3607),
        (
            EvmFeatures::BLOCK_GAS_LIMIT_CHECK,
            !cfg.disable_block_gas_limit,
        ),
        (EvmFeatures::FEE_CHARGE, !cfg.disable_fee_charge),
        (EvmFeatures::TX_CHAIN_ID_CHECK, cfg.tx_chain_id_check),
        (
            EvmFeatures::PRIORITY_FEE_CHECK,
            !<CfgEnv as revm::context_interface::Cfg>::is_priority_fee_check_disabled(cfg),
        ),
    ] {
        version.features.set(feature, enabled);
    }
    if let Some(cap) = cfg.tx_gas_limit_cap {
        version.tx_gas_limit_cap = cap;
    }
    if let Some(size) = cfg.limit_contract_code_size {
        version.max_code_size = size;
    }
    version.max_blobs_per_tx = cfg.max_blobs_per_tx.map_or(usize::MAX, |n| n as usize);
    version.memory_limit = <CfgEnv as revm::context_interface::Cfg>::memory_limit(cfg);
    version
}

pub fn result(result: evm2::TxResult, create: bool) -> Result<ExecutionResult, String> {
    let gas = ResultGas::new_with_state_gas(
        result.total_gas_spent,
        result.refunded,
        result.floor_gas,
        result.state_gas_spent,
    );
    if result.status {
        let reason = match result.stop {
            InstrStop::Stop => SuccessReason::Stop,
            InstrStop::Return => SuccessReason::Return,
            InstrStop::SelfDestruct => SuccessReason::SelfDestruct,
            stop => {
                return Err(format!(
                    "EVM2 successful transaction has invalid stop {stop:?}"
                ));
            }
        };
        let output = if create {
            Output::Create(result.output, result.created_address)
        } else {
            Output::Call(result.output)
        };
        return Ok(ExecutionResult::Success {
            reason,
            gas,
            output,
            logs: result.logs,
        });
    }
    if result.is_revert() {
        return Ok(ExecutionResult::Revert {
            gas,
            output: result.output,
            logs: result.logs,
        });
    }
    let reason = match result.stop {
        InstrStop::OutOfGas => HaltReason::OutOfGas(OutOfGasError::Basic),
        InstrStop::MemoryOOG => HaltReason::OutOfGas(OutOfGasError::Memory),
        InstrStop::MemoryLimitOOG => HaltReason::OutOfGas(OutOfGasError::MemoryLimit),
        InstrStop::PrecompileOOG => HaltReason::OutOfGas(OutOfGasError::Precompile),
        InstrStop::InvalidOperandOOG => HaltReason::OutOfGas(OutOfGasError::InvalidOperand),
        InstrStop::ReentrancySentryOOG => HaltReason::OutOfGas(OutOfGasError::ReentrancySentry),
        InstrStop::CallNotAllowedInsideStatic => HaltReason::CallNotAllowedInsideStatic,
        InstrStop::StateChangeDuringStaticCall => HaltReason::StateChangeDuringStaticCall,
        InstrStop::OpcodeNotFound => HaltReason::OpcodeNotFound,
        InstrStop::InvalidFEOpcode => HaltReason::InvalidFEOpcode,
        InstrStop::InvalidJump => HaltReason::InvalidJump,
        InstrStop::NotActivated => HaltReason::NotActivated,
        InstrStop::StackUnderflow => HaltReason::StackUnderflow,
        InstrStop::StackOverflow => HaltReason::StackOverflow,
        InstrStop::OutOfOffset => HaltReason::OutOfOffset,
        InstrStop::CreateCollision => HaltReason::CreateCollision,
        InstrStop::PrecompileError => HaltReason::PrecompileError,
        InstrStop::NonceOverflow => HaltReason::NonceOverflow,
        InstrStop::CreateContractSizeLimit => HaltReason::CreateContractSizeLimit,
        InstrStop::CreateContractStartingWithEF => HaltReason::CreateContractStartingWithEF,
        InstrStop::CreateInitCodeSizeLimit => HaltReason::CreateInitCodeSizeLimit,
        InstrStop::OutOfFunds => HaltReason::OutOfFunds,
        InstrStop::CallTooDeep => HaltReason::CallTooDeep,
        stop => {
            return Err(format!(
                "EVM2 stop cannot be represented by revm43: {stop:?}"
            ));
        }
    };
    Ok(ExecutionResult::Halt {
        reason,
        gas,
        logs: result.logs,
    })
}
