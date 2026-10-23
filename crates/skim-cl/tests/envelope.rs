//! Real Sepolia gossip captured by `skim probe-cl --capture-dir` (slot 11356920, EL block
//! 11916226). Expected values were cross-checked against a local Nethermind
//! (`eth_getBlockByNumber`: hash, stateRoot, blockAccessListHash, slotNumber, tx count) and the
//! message ids against Python `hashlib` over the same bytes.

use alloy_primitives::{B256, b256};
use skim_cl::{
    gossip,
    network::BodyLayout,
    ssz::{BlockInfo, Envelope},
    verify,
};

const ENVELOPE: &[u8] = include_bytes!("fixtures/sepolia-11356920.envelope.ssz");
const BLOCK: &[u8] = include_bytes!("fixtures/sepolia-11356920.block.ssz");
const BLOCK_HASH: B256 = b256!("72ca458b6f4e8d971c9bf65e6de5908b55f31d0b49bf91d8f7feca5254feb99b");

#[test]
fn parses_envelope_fields() {
    let env = Envelope::parse(ENVELOPE).unwrap();
    let p = &env.payload;
    assert_eq!(p.slot_number, 11356920);
    assert_eq!(p.block_number, 11916226);
    assert_eq!(p.block_hash, BLOCK_HASH);
    assert_eq!(p.parent_hash, b256!("74947c7a81606f07c354b78bd32474a5433d04eaa73b996af5716bf458a58c3f"));
    assert_eq!(p.state_root, b256!("b5754930b07618e2a46d8bf3364d19da0b975ada1b4f4ef5cd5fa2a533ec8164"));
    assert_eq!(p.timestamp, 1655733600 + 11356920 * 12);
    assert_eq!(p.transactions.len(), 107);
    assert_eq!(p.withdrawals.len(), 16);
    assert_eq!(p.block_access_list.len(), 35827);
    assert_eq!(env.builder_index, 0);
    assert_eq!(
        env.beacon_block_root,
        b256!("50e30991c9bd70d6046c604137a7e1d2dabe9156863c427b64cd3f4e3e72d9df")
    );
}

#[test]
fn rebuilt_header_matches_block_hash_and_bal_decodes() {
    let env = Envelope::parse(ENVELOPE).unwrap();
    let v = verify::verify_payload(&env);
    assert!(v.ok, "computed {} expected {}", v.computed_hash, env.payload.block_hash);
    assert_eq!(v.bal_hash, b256!("b0f26ce3d5fa85b3e922d370ef938465bc50375ce197410e2886f8127260e2c6"));
    // The commitment check and decode the state follower uses.
    let bal = skim_bal::decode_verified(env.payload.block_access_list, v.bal_hash).unwrap();
    assert_eq!(bal.len(), 284);
}

#[test]
fn tampered_bal_breaks_block_hash() {
    let mut raw = ENVELOPE.to_vec();
    let env = Envelope::parse(ENVELOPE).unwrap();
    // Flip the last byte of the BAL (located via the parsed slice, not a hard-coded offset).
    let off = env.payload.block_access_list.as_ptr() as usize - ENVELOPE.as_ptr() as usize;
    raw[off + env.payload.block_access_list.len() - 1] ^= 1;
    let env = Envelope::parse(&raw).unwrap();
    assert!(!verify::verify_payload(&env).ok);
}

#[test]
fn parses_block_and_bid_links_to_envelope() {
    let b = BlockInfo::parse(BLOCK).unwrap();
    assert_eq!(b.slot, 11356920);
    assert_eq!(b.proposer_index, 830);
    let bid = b.bid.unwrap();
    assert_eq!(bid.block_hash, BLOCK_HASH);
    assert_eq!(bid.slot, 11356920);
    assert_eq!(bid.builder_index, 0);
    let env = Envelope::parse(ENVELOPE).unwrap();
    assert_eq!(bid.parent_block_hash, env.payload.parent_hash);
    assert_eq!(bid.parent_block_root, env.parent_beacon_block_root);
}

/// The fixture's block re-laid out as a Heze body (EIP-8015: no `eth1_data`, no `deposits`).
fn as_heze(block: &[u8]) -> Vec<u8> {
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize;
    let msg_start = u32_at(block, 0);
    let msg = &block[msg_start..];
    let body_start = u32_at(msg, 80);
    let body = &msg[body_start..];
    // Gloas fixed part: randao 96 | eth1_data 72 | graffiti 32 | offsets proposer_slashings,
    // attester_slashings, attestations, deposits, voluntary_exits | sync_aggregate 160 | offsets.
    let fixed = u32_at(body, 200);
    let mut offs: Vec<usize> = vec![200, 204, 208, 212, 216];
    offs.extend((380..fixed).step_by(4));
    let starts: Vec<usize> = offs.iter().map(|&i| u32_at(body, i)).collect();
    let deposits = starts[3]..starts[4];
    let removed_fixed = 72 + 4;
    let mut out = Vec::new();
    out.extend_from_slice(&body[..96]); // randao_reveal
    out.extend_from_slice(&body[168..200]); // graffiti
    for (k, &start) in starts.iter().enumerate() {
        if k == 3 {
            continue;
        }
        let shift = removed_fixed + if start >= deposits.end { deposits.len() } else { 0 };
        out.extend_from_slice(&((start - shift) as u32).to_le_bytes());
        if k == 4 {
            out.extend_from_slice(&body[220..380]); // sync_aggregate
        }
    }
    out.extend_from_slice(&body[fixed..deposits.start]);
    out.extend_from_slice(&body[deposits.end..]);
    let mut b = block[..msg_start].to_vec();
    b.extend_from_slice(&msg[..body_start]);
    b.extend_from_slice(&out);
    b
}

#[test]
fn reads_the_bid_from_a_heze_body() {
    let heze = as_heze(BLOCK);
    assert_eq!(heze.len(), BLOCK.len() - 76, "the fixture has no deposits; eth1_data and an offset are gone");
    let b = BlockInfo::parse_with(&heze, |_| BodyLayout::Heze).unwrap();
    assert_eq!(b.bid, BlockInfo::parse(BLOCK).unwrap().bid);
    assert_eq!(b.bid.unwrap().block_hash, BLOCK_HASH);
    // Each layout rejects the other's bytes instead of misreading them.
    assert_eq!(BlockInfo::parse_with(&heze, |_| BodyLayout::Gloas).unwrap().bid, None);
    assert_eq!(BlockInfo::parse_with(BLOCK, |_| BodyLayout::Heze).unwrap().bid, None);
}

#[test]
fn rejects_truncated_envelope() {
    assert!(Envelope::parse(&ENVELOPE[..ENVELOPE.len() / 2]).is_err());
    assert!(Envelope::parse(&ENVELOPE[..50]).is_err());
}

#[test]
fn message_ids() {
    let topic_env = gossip::topic([0x66, 0x9e, 0x6c, 0x11], "execution_payload");
    assert_eq!(topic_env, "/eth2/669e6c11/execution_payload/ssz_snappy");
    let topic_blk = gossip::topic([0x66, 0x9e, 0x6c, 0x11], "beacon_block");

    // Gossip carries snappy *block* format; the id is over the decompressed bytes.
    let wire = snap::raw::Encoder::new().compress_vec(ENVELOPE).unwrap();
    assert_eq!(gossip::decompress(&wire).unwrap(), ENVELOPE);
    assert_eq!(hex(&gossip::message_id_raw(&topic_env, &wire)), "63e188f687a72cbf0c81336e5eae9fbc1f5a2f1b");
    let wire = snap::raw::Encoder::new().compress_vec(BLOCK).unwrap();
    assert_eq!(hex(&gossip::message_id_raw(&topic_blk, &wire)), "edb5388854aca8ffeb50330a2a113c86bc8fede3");
    // Invalid snappy: invalid-snappy domain over the raw bytes.
    assert_eq!(
        hex(&gossip::message_id_raw(&topic_blk, &[0xff, 0xff, 0xff])),
        "3abd2b589e9efdcdff26012d857813e51b814fca"
    );
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn only_payloads_that_pass_the_commitment_check_are_relayed() {
    // Review: payloads were relayed (gossipsub Accept) once they parsed, before the commitment
    // check; blocks and attestations on a parse or length check. Peers that validate penalise
    // whoever relays an invalid message.
    use skim_cl::node::{SINGLE_ATTESTATION_LEN, check_gossip};
    let lens = (0, 0);
    let ok = check_gossip("execution_payload", ENVELOPE, lens);
    assert!(ok.relay);
    assert_eq!(ok.head, Some((11356920, Envelope::parse(ENVELOPE).unwrap().beacon_block_root)));
    let mut raw = ENVELOPE.to_vec();
    let env = Envelope::parse(ENVELOPE).unwrap();
    let off = env.payload.block_access_list.as_ptr() as usize - ENVELOPE.as_ptr() as usize;
    raw[off] ^= 1; // parses, fails the block hash
    assert!(Envelope::parse(&raw).is_ok());
    assert_eq!(check_gossip("execution_payload", &raw, lens), Default::default(), "not relayed, and no head from it");
    assert!(!check_gossip("beacon_block", BLOCK, lens).relay);
    assert!(!check_gossip("beacon_attestation_3", &[0u8; SINGLE_ATTESTATION_LEN], lens).relay);
}
