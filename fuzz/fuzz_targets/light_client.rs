//! Light-client bootstrap and updates (fixed-size SSZ; inputs are fitted to each size so the
//! parsing, branch and BLS paths are reached).
#![no_main]
use std::sync::OnceLock;
use skim_cl::light;

fn committee() -> &'static light::Committees {
    static C: OnceLock<light::Committees> = OnceLock::new();
    C.get_or_init(|| {
        let pks: Vec<_> = (0..light::SYNC_COMMITTEE_SIZE)
            .map(|i| blst::min_pk::SecretKey::key_gen(&[(i % 251) as u8 + 1; 32], &[]).unwrap().sk_to_pk())
            .collect();
        let mut c = light::Committees::new(light::Committee::from_pubkeys(0, pks.clone()).unwrap());
        c.next = light::Committee::from_pubkeys(1, pks);
        c
    })
}

fn fit(data: &[u8], len: usize) -> Vec<u8> {
    let mut v = data.to_vec();
    v.resize(len, 0);
    v
}

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let root = alloy_primitives::B256::from_slice(&fit(data, 32));
    let _ = light::bootstrap(&fit(data, light::BOOTSTRAP_LEN), root);
    let _ = light::bootstrap(data, root);
    let fv = [0x90, 0, 0, 0x76];
    let _ = light::optimistic_update(committee(), &fit(data, light::OPTIMISTIC_UPDATE_LEN), fv);
    let _ = light::finality_update(committee(), &fit(data, light::FINALITY_UPDATE_LEN), fv);
    let _ = light::optimistic_update(committee(), data, fv);
    let _ = light::full_update(committee(), &fit(data, light::UPDATE_LEN), fv);
});
