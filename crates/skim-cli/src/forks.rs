//! Forks after Glamsterdam: which ones skim follows, and where it stops.
//!
//! skim implements Glamsterdam. Hegota (Heze on consensus, Bogotá on execution) is followed only
//! with `--experimental-hegota`, under assumptions that will change with its spec
//! (docs/guide/hegota.md). Any other scheduled fork skim doesn't know, and Hegota without the
//! flag, is a stop: skim applies no block from its activation on, rather than serve state under
//! rules it doesn't implement.

use std::sync::OnceLock;

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPlan {
    /// Hegota's activation (Unix seconds), if the network schedules it, and whether skim follows it.
    pub hegota: Option<(u64, bool)>,
    /// The first fork skim won't follow: `(name, activation time)`.
    pub stop: Option<(String, u64)>,
    /// Heze and Bogotá scheduled at different times. Each half activates on its own (consensus
    /// by epoch, execution by timestamp), so skim follows both; worth a warning all the same.
    /// Only one half scheduled is normal on devnets: bogota-devnet-1 runs Bogotá on Gloas.
    pub hegota_mismatch: Option<String>,
}

impl ForkPlan {
    pub fn new(cl: &skim_cl::network::ClNetwork, el: &skim_el::ElNetwork, experimental_hegota: bool) -> Self {
        let cl = skim_cl::network::ClNetwork { experimental_hegota, ..cl.clone() };
        let heze = cl.heze.map(|h| cl.epoch_time(h.epoch));
        let bogota = el.bogota_time();
        let cl_stop = cl.unsupported_fork().map(|(n, e)| (n, cl.epoch_time(e)));
        let el_stop = el.unsupported_fork(experimental_hegota);
        // Heze and Bogotá are one fork to a user: Hegota.
        let stop = cl_stop.into_iter().chain(el_stop).min_by_key(|(_, t)| *t).map(|(n, t)| (if n == "heze" || n == "bogota" { "hegota".into() } else { n }, t));
        let hegota_mismatch = match (heze, bogota) {
            (Some(h), Some(b)) if h != b => Some(format!("Heze at {h} but Bogota at {b}")),
            _ => None,
        };
        let first = heze.into_iter().chain(bogota).min();
        Self { hegota: first.map(|t| (t, experimental_hegota)), stop, hegota_mismatch }
    }

    /// Whether a block at `timestamp` is past where skim stops.
    pub fn stops(&self, timestamp: u64) -> Option<&str> {
        self.stop.as_ref().filter(|(_, t)| timestamp >= *t).map(|(n, _)| n.as_str())
    }

    pub fn json(&self, now: u64) -> Value {
        json!({
            "hegota": self.hegota.map(|(at, on)| json!({"at": at, "support": if on { "experimental" } else { "none" }})),
            "stops_at": self.stop.as_ref().map(|(n, at)| json!({"fork": n, "at": at, "reached": now >= *at})),
        })
    }
}

static PLAN: OnceLock<ForkPlan> = OnceLock::new();

pub fn set(p: ForkPlan) {
    let _ = PLAN.set(p);
}

pub fn get() -> Option<&'static ForkPlan> {
    PLAN.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nets(heze_epoch: Option<u64>, bogota: Option<u64>) -> (skim_cl::network::ClNetwork, skim_el::ElNetwork) {
        let mut cfg = "PRESET_BASE: 'minimal'\nMIN_GENESIS_TIME: 0\nGLOAS_FORK_VERSION: 0x80000038\nGLOAS_FORK_EPOCH: 4\nSLOT_DURATION_MS: 6000\n".to_string();
        if let Some(e) = heze_epoch {
            cfg += &format!("HEZE_FORK_VERSION: 0x90000038\nHEZE_FORK_EPOCH: {e}\n");
        }
        let cl = skim_cl::network::ClNetwork::from_config("devnet", &cfg, [1; 32], Some(1_000), vec![]).unwrap();
        let mut g: alloy_genesis::Genesis = serde_json::from_str(include_str!("../../skim-el/networks/hoodi/genesis.json")).unwrap();
        g.config.bogota_time = bogota;
        let spec = skim_el::ElNetwork::spec_from_genesis(g).unwrap();
        (cl, skim_el::ElNetwork { spec: std::sync::Arc::new(spec), bootnodes: vec![], dns: None })
    }

    #[test]
    fn hegota_is_followed_only_with_the_flag() {
        // Epoch 12 of a 6 s, 8-slot preset from genesis 1000: 1000 + 12 * 48.
        let at = 1_000 + 12 * 48;
        let (cl, el) = nets(Some(12), Some(at));
        let off = ForkPlan::new(&cl, &el, false);
        assert_eq!(off.stop, Some(("hegota".into(), at)), "both halves stop at the same time");
        assert_eq!(off.stops(at - 1), None);
        assert_eq!(off.stops(at), Some("hegota"));
        assert_eq!(off.json(at)["hegota"]["support"], "none");
        let on = ForkPlan::new(&cl, &el, true);
        assert_eq!((on.stop.clone(), on.hegota, on.hegota_mismatch.clone()), (None, Some((at, true)), None));
        assert_eq!(on.json(0)["stops_at"], Value::Null);
    }

    #[test]
    fn either_half_of_hegota_alone_is_followed_with_the_flag() {
        let at = 1_000 + 12 * 48;
        // Bogotá on Gloas (bogota-devnet-1's shape): followed with the flag, a stop without.
        let (cl, el) = nets(None, Some(at));
        let p = ForkPlan::new(&cl, &el, true);
        assert_eq!((p.stop.clone(), p.hegota, p.hegota_mismatch.clone()), (None, Some((at, true)), None));
        assert_eq!(ForkPlan::new(&cl, &el, false).stop, Some(("hegota".into(), at)));
        // Both halves at different times: followed, with a warning; the first one counts.
        let (cl, el) = nets(Some(12), Some(at + 48));
        let p = ForkPlan::new(&cl, &el, true);
        assert!(p.hegota_mismatch.is_some());
        assert_eq!((p.stop, p.hegota), (None, Some((at, true))));
        // Glamsterdam-only networks: nothing to stop at.
        let (cl, el) = nets(None, None);
        assert_eq!(ForkPlan::new(&cl, &el, false), ForkPlan { hegota: None, stop: None, hegota_mismatch: None });
    }
}
