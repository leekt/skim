//! Which consensus network skim follows: the preset's sizes and the chain's parameters.
//!
//! Loaded from a standard network-config directory (what consensus clients use, and what
//! ethereum-package and eth-clients publish): `config.yaml` (preset, fork versions and epochs, slot
//! duration, blob schedule), `genesis_validators_root.txt` or `genesis.ssz`, and
//! `bootstrap_nodes.txt`. Sepolia is built in from its published config.

use std::path::Path;

use crate::light::LcConfig;

/// The consensus preset: sizes compiled into SSZ containers and the light-client protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub slots_per_epoch: u64,
    pub sync_committee_size: usize,
    pub epochs_per_sync_committee_period: u64,
}

impl Preset {
    pub const MAINNET: Self = Self { name: "mainnet", slots_per_epoch: 32, sync_committee_size: 512, epochs_per_sync_committee_period: 256 };
    pub const MINIMAL: Self = Self { name: "minimal", slots_per_epoch: 8, sync_committee_size: 32, epochs_per_sync_committee_period: 8 };

    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "mainnet" => Some(Self::MAINNET),
            "minimal" => Some(Self::MINIMAL),
            _ => None,
        }
    }

    pub fn slots_per_period(&self) -> u64 {
        self.slots_per_epoch * self.epochs_per_sync_committee_period
    }
}

/// A consensus network skim can follow. skim follows only Gloas (Glamsterdam) blocks, so the
/// network is described at its Gloas fork.
#[derive(Debug, Clone)]
pub struct ClNetwork {
    pub name: String,
    pub preset: Preset,
    pub genesis_time: u64,
    pub slot_ms: u64,
    pub genesis_validators_root: [u8; 32],
    pub gloas_fork_epoch: u64,
    pub gloas_fork_version: [u8; 4],
    /// `BLOB_SCHEDULE`: `(epoch, max_blobs_per_block)`, part of the fork digest since Fulu.
    pub blob_schedule: Vec<(u64, u64)>,
    /// Fallback blob parameters before the first schedule entry (Electra's).
    pub electra_fork_epoch: u64,
    pub electra_max_blobs: u64,
    pub bootnodes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("{0}: {1}")]
    File(String, String),
    #[error("config.yaml: {0}")]
    Config(String),
}

/// Sepolia's published consensus config (eth-clients/sepolia `metadata/config.yaml`).
const SEPOLIA_CONFIG: &str = include_str!("../networks/sepolia/config.yaml");

impl ClNetwork {
    pub fn sepolia() -> Self {
        let mut n = Self::from_config(
            "sepolia",
            SEPOLIA_CONFIG,
            crate::light::SEPOLIA_GENESIS_VALIDATORS_ROOT,
            None,
            crate::node::SEPOLIA_BOOTNODES.iter().map(|s| s.to_string()).collect(),
        )
        .expect("built-in Sepolia config parses");
        // MIN_GENESIS_TIME + GENESIS_DELAY, as config.yaml gives it.
        debug_assert_eq!(n.genesis_time, crate::node::SEPOLIA_GENESIS_TIME);
        n.name = "sepolia".into();
        n
    }

    /// A network-config directory: `config.yaml`; `genesis_validators_root.txt` (or the root and
    /// genesis time from `genesis.ssz`); `bootstrap_nodes.txt` (ENRs, one per line), optional.
    pub fn from_dir(dir: &Path) -> Result<Self, NetworkError> {
        let read = |f: &str| std::fs::read(dir.join(f)).map_err(|e| NetworkError::File(dir.join(f).display().to_string(), e.to_string()));
        let config = String::from_utf8_lossy(&read("config.yaml")?).into_owned();
        // genesis.ssz is a BeaconState: genesis_time (u64) then genesis_validators_root (32 bytes).
        let state = read("genesis.ssz").ok();
        let gvr = match read("genesis_validators_root.txt") {
            Ok(t) => parse_hex32(String::from_utf8_lossy(&t).trim()).ok_or_else(|| NetworkError::File("genesis_validators_root.txt".into(), "not a 32-byte hex root".into()))?,
            Err(e) => match &state {
                Some(s) if s.len() >= 40 => s[8..40].try_into().unwrap(),
                _ => return Err(e),
            },
        };
        let genesis_time = state.as_ref().filter(|s| s.len() >= 8).map(|s| u64::from_le_bytes(s[..8].try_into().unwrap()));
        let bootnodes = read("bootstrap_nodes.txt")
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .lines()
                    .map(|l| l.trim().trim_start_matches("- ").trim_matches('"').to_string())
                    .filter(|l| l.starts_with("enr:"))
                    .collect()
            })
            .unwrap_or_default();
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "custom".into());
        Self::from_config(&name, &config, gvr, genesis_time, bootnodes)
    }

    /// From `config.yaml`'s text and what isn't in it. Without `genesis_time`, uses
    /// `MIN_GENESIS_TIME + GENESIS_DELAY` (right for networks that started on time, like Sepolia).
    pub fn from_config(name: &str, config: &str, genesis_validators_root: [u8; 32], genesis_time: Option<u64>, bootnodes: Vec<String>) -> Result<Self, NetworkError> {
        let kv = parse_config(config);
        let get = |k: &str| kv.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        let num = |k: &str| -> Result<u64, NetworkError> {
            let v = get(k).ok_or_else(|| NetworkError::Config(format!("{k} missing")))?;
            v.parse().map_err(|_| NetworkError::Config(format!("{k}: not a number: {v}")))
        };
        let preset_name = get("PRESET_BASE").unwrap_or("mainnet");
        let preset = Preset::by_name(preset_name).ok_or_else(|| NetworkError::Config(format!("unknown PRESET_BASE {preset_name}")))?;
        let slot_ms = match get("SLOT_DURATION_MS") {
            Some(_) => num("SLOT_DURATION_MS")?,
            None => num("SECONDS_PER_SLOT")? * 1000,
        };
        let gloas_fork_version = parse_hex4(get("GLOAS_FORK_VERSION").ok_or_else(|| NetworkError::Config("GLOAS_FORK_VERSION missing (skim follows Glamsterdam networks)".into()))?)
            .ok_or_else(|| NetworkError::Config("GLOAS_FORK_VERSION: not 4 hex bytes".into()))?;
        let genesis_time = match genesis_time {
            Some(t) => t,
            None => num("MIN_GENESIS_TIME")? + get("GENESIS_DELAY").and_then(|v| v.parse().ok()).unwrap_or(0),
        };
        Ok(Self {
            name: name.to_string(),
            preset,
            genesis_time,
            slot_ms,
            genesis_validators_root,
            gloas_fork_epoch: num("GLOAS_FORK_EPOCH")?,
            gloas_fork_version,
            blob_schedule: parse_blob_schedule(config),
            electra_fork_epoch: get("ELECTRA_FORK_EPOCH").and_then(|v| v.parse().ok()).unwrap_or(0),
            electra_max_blobs: get("MAX_BLOBS_PER_BLOCK_ELECTRA").and_then(|v| v.parse().ok()).unwrap_or(9),
            bootnodes,
        })
    }

    pub fn seconds_per_slot(&self) -> u64 {
        (self.slot_ms / 1000).max(1)
    }

    pub fn slot_at(&self, unix_time: u64) -> u64 {
        unix_time.saturating_sub(self.genesis_time) * 1000 / self.slot_ms
    }

    pub fn epoch_of(&self, slot: u64) -> u64 {
        slot / self.preset.slots_per_epoch
    }

    /// The blob parameters in force at `epoch` (`get_blob_parameters`).
    fn blob_parameters(&self, epoch: u64) -> (u64, u64) {
        let mut s = self.blob_schedule.clone();
        s.sort();
        s.into_iter().rev().find(|(e, _)| epoch >= *e).unwrap_or((self.electra_fork_epoch, self.electra_max_blobs))
    }

    /// The gossip fork digest at `epoch` (Fulu's `compute_fork_digest`: the fork data root XOR
    /// the hash of the blob parameters in force), for the Gloas fork version.
    pub fn fork_digest_at(&self, epoch: u64) -> [u8; 4] {
        use sha2::{Digest, Sha256};
        let mut v = [0u8; 32];
        v[..4].copy_from_slice(&self.gloas_fork_version);
        let base: [u8; 32] = Sha256::new().chain_update(v).chain_update(self.genesis_validators_root).finalize().into();
        let (e, m) = self.blob_parameters(epoch);
        let h: [u8; 32] = Sha256::new().chain_update(e.to_le_bytes()).chain_update(m.to_le_bytes()).finalize().into();
        let mut d = [0u8; 4];
        for i in 0..4 {
            d[i] = base[i] ^ h[i];
        }
        d
    }

    /// The fork digest now (or at the Gloas fork, if that's later).
    pub fn current_fork_digest(&self) -> [u8; 4] {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        self.fork_digest_at(self.epoch_of(self.slot_at(now)).max(self.gloas_fork_epoch))
    }

    pub fn light_client(&self) -> LcConfig {
        LcConfig { committee_size: self.preset.sync_committee_size, slots_per_period: self.preset.slots_per_period(), genesis_validators_root: self.genesis_validators_root }
    }
}

fn parse_hex4(v: &str) -> Option<[u8; 4]> {
    let b = alloy_primitives::hex::decode(v.trim().trim_matches('\'').trim_matches('"')).ok()?;
    b.try_into().ok()
}

fn parse_hex32(v: &str) -> Option<[u8; 32]> {
    let b = alloy_primitives::hex::decode(v.trim().trim_matches('\'').trim_matches('"')).ok()?;
    b.try_into().ok()
}

/// Top-level `KEY: value` pairs of `config.yaml` (quotes and comments stripped).
fn parse_config(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.starts_with(' ') && !l.starts_with('-') && !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let v = v.split('#').next().unwrap_or("").trim().trim_matches('\'').trim_matches('"');
            (!v.is_empty()).then(|| (k.trim().to_string(), v.to_string()))
        })
        .collect()
}

/// `BLOB_SCHEDULE:` followed by `- EPOCH: n` / `MAX_BLOBS_PER_BLOCK: m` items.
fn parse_blob_schedule(text: &str) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut inside = false;
    let mut epoch: Option<u64> = None;
    for l in text.lines() {
        if l.starts_with("BLOB_SCHEDULE:") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        let t = l.trim().trim_start_matches("- ").trim();
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        if !l.starts_with(' ') && !l.starts_with('-') {
            break; // the next top-level key
        }
        if let Some(v) = t.strip_prefix("EPOCH:") {
            epoch = v.trim().parse().ok();
        } else if let Some(v) = t.strip_prefix("MAX_BLOBS_PER_BLOCK:")
            && let (Some(e), Ok(m)) = (epoch.take(), v.trim().parse())
        {
            out.push((e, m));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sepolia_from_its_published_config_gives_the_digest_peers_use() {
        let n = ClNetwork::sepolia();
        assert_eq!(n.preset, Preset::MAINNET);
        assert_eq!(n.genesis_time, crate::node::SEPOLIA_GENESIS_TIME);
        assert_eq!(n.slot_ms, 12_000);
        assert_eq!(n.gloas_fork_version, crate::node::SEPOLIA_GLOAS_VERSION);
        assert_eq!(n.blob_schedule, vec![(274176, 15), (275712, 21)]);
        // Checked against a local Lighthouse's topics and ENR (the constant this replaces).
        assert_eq!(n.fork_digest_at(n.gloas_fork_epoch), crate::node::SEPOLIA_GLOAS_DIGEST);
        assert_eq!(n.light_client(), LcConfig::SEPOLIA);
    }

    #[test]
    fn a_minimal_preset_devnet_config_parses() {
        let cfg = "PRESET_BASE: 'minimal'\nMIN_GENESIS_TIME: 1792377224\nGENESIS_DELAY: 0\nELECTRA_FORK_EPOCH: 0\nGLOAS_FORK_VERSION: 0x80000038\nGLOAS_FORK_EPOCH: 4\nSLOT_DURATION_MS: 6000\n# a comment\nBLOB_SCHEDULE:\n  - EPOCH: 0\n    MAX_BLOBS_PER_BLOCK: 21\n\n# next section\nGAS_LIMIT_SCHEDULE: []\n";
        let n = ClNetwork::from_config("devnet", cfg, [7; 32], Some(1792377300), vec![]).unwrap();
        assert_eq!(n.preset, Preset::MINIMAL);
        assert_eq!((n.slot_ms, n.seconds_per_slot(), n.gloas_fork_epoch), (6000, 6, 4));
        assert_eq!(n.blob_schedule, vec![(0, 21)]);
        assert_eq!(n.genesis_time, 1792377300, "genesis.ssz's time wins over the config's");
        assert_eq!(n.slot_at(1792377300 + 61), 10);
        let lc = n.light_client();
        assert_eq!((lc.committee_size, lc.slots_per_period), (32, 64));
    }

    #[test]
    fn a_config_without_gloas_is_refused() {
        assert!(ClNetwork::from_config("x", "PRESET_BASE: mainnet\nSECONDS_PER_SLOT: 12\nMIN_GENESIS_TIME: 1\n", [0; 32], None, vec![]).is_err());
    }
}
