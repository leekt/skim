//! Execution environment under Amsterdam rules, for the chain set at startup (Sepolia by default).

use alloy_consensus::Header;
use alloy_primitives::U256;
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::block::BlobExcessGasAndPrice,
    primitives::hardfork::SpecId,
};

pub const SEPOLIA_CHAIN_ID: u64 = 11155111;

static CHAIN_ID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Sets the chain id this process serves (once, at startup; later calls are ignored).
pub fn set_chain_id(id: u64) {
    let _ = CHAIN_ID.set(id);
}

/// The chain id this process serves: what [`set_chain_id`] set, else Sepolia's.
pub fn chain_id() -> u64 {
    *CHAIN_ID.get_or_init(|| SEPOLIA_CHAIN_ID)
}
pub const SPEC: SpecId = SpecId::AMSTERDAM;

type FractionAt = Box<dyn Fn(u64) -> Option<u64> + Send + Sync>;
static BLOB_FRACTION: std::sync::OnceLock<FractionAt> = std::sync::OnceLock::new();

/// Sets the blob base-fee update fraction in force at a timestamp, from the chain's blob
/// schedule (once, at startup). Without it, Sepolia's.
pub fn set_blob_update_fraction(f: impl Fn(u64) -> Option<u64> + Send + Sync + 'static) {
    let _ = BLOB_FRACTION.set(Box::new(f));
}

/// Sepolia's (and mainnet's) since BPO2.
pub const BPO2_UPDATE_FRACTION: u64 = 11_684_671;

/// The blob base-fee update fraction at `timestamp`. (Review: `BLOBBASEFEE` used revm's Prague
/// fraction, 5,007,716, whatever the chain's blob schedule said: wrong on BPO networks whenever
/// `excess_blob_gas > 0`.)
pub fn blob_update_fraction(timestamp: u64) -> u64 {
    BLOB_FRACTION.get().and_then(|f| f(timestamp)).unwrap_or(BPO2_UPDATE_FRACTION)
}

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
        blob_excess_gas_and_price: Some(BlobExcessGasAndPrice::new(h.excess_blob_gas.unwrap_or_default(), blob_update_fraction(h.timestamp))),
        ..Default::default()
    }
}

/// `eth_call` semantics: no nonce/balance/basefee/EIP-3607 checks, no fee charge (so the
/// per-block coinbase is never loaded).
pub fn call_cfg() -> CfgEnv {
    let mut cfg = CfgEnv::new_with_spec(SPEC);
    cfg.chain_id = chain_id();
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
    cfg.chain_id = chain_id();
    cfg
}
