//! Execution environment for Sepolia under Amsterdam rules.

use alloy_consensus::Header;
use alloy_primitives::U256;
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::block::BlobExcessGasAndPrice,
    primitives::hardfork::SpecId,
};

pub const SEPOLIA_CHAIN_ID: u64 = 11155111;
pub const SPEC: SpecId = SpecId::AMSTERDAM;

/// Block context for executing *in* block `h` (post-state of its parent) or, for `eth_call` at
/// `h`, on top of `h`'s post-state with `h`'s own header fields, as nodes do.
pub fn block_env(h: &Header) -> BlockEnv {
    BlockEnv {
        number: U256::from(h.number),
        beneficiary: h.beneficiary,
        timestamp: U256::from(h.timestamp),
        gas_limit: h.gas_limit,
        basefee: h.base_fee_per_gas.unwrap_or_default(),
        difficulty: U256::ZERO,
        prevrandao: Some(h.mix_hash),
        blob_excess_gas_and_price: Some(BlobExcessGasAndPrice::new_with_spec(h.excess_blob_gas.unwrap_or_default(), SPEC)),
        ..Default::default()
    }
}

/// `eth_call` semantics: no nonce/balance/basefee/EIP-3607 checks, no fee charge (so the
/// per-block coinbase is never loaded).
pub fn call_cfg() -> CfgEnv {
    let mut cfg = CfgEnv::new_with_spec(SPEC);
    cfg.chain_id = SEPOLIA_CHAIN_ID;
    cfg.disable_nonce_check = true;
    cfg.disable_balance_check = true;
    cfg.disable_base_fee = true;
    cfg.disable_eip3607 = true;
    cfg.disable_block_gas_limit = true;
    cfg.disable_fee_charge = true;
    cfg.tx_chain_id_check = false;
    cfg
}

/// `eth_simulateV1` with `validation: true`: nonce, balance, base-fee and sender checks on, fees
/// charged (to the block's fee recipient), as a block builder would.
pub fn validating_cfg() -> CfgEnv {
    let mut cfg = CfgEnv::new_with_spec(SPEC);
    cfg.chain_id = SEPOLIA_CHAIN_ID;
    cfg
}
