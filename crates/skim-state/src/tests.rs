use super::*;

const A: Address = Address::repeat_byte(0xaa);

fn blk(n: u64, branch: u8) -> BlockRef {
    let h = |n: u64, b: u8| {
        let mut x = [0u8; 32];
        x[..8].copy_from_slice(&n.to_be_bytes());
        x[31] = b;
        B256::from(x)
    };
    BlockRef { number: n, hash: h(n, branch), parent_hash: h(n.wrapping_sub(1), if n > 10 { branch } else { 0 }), state_root: B256::ZERO, timestamp: n * 12 }
}

fn slot(i: u64) -> StateKey {
    StateKey::Storage(A, U256::from(i))
}

fn writes(w: &[(StateKey, u64)]) -> Arc<BlockWrites> {
    Arc::new(BlockWrites {
        writes: w.iter().map(|(k, v)| (*k, U256::from(*v))).collect(),
        touched: vec![A],
        ..Default::default()
    })
}

fn known(s: &Snapshot, k: StateKey) -> Option<u64> {
    match s.get(&k).unwrap() {
        StateValue::Known(v) => Some(v.to()),
        StateValue::Unknown => None,
    }
}

#[test]
fn follows_writes_and_unknown_is_not_zero() {
    let s = Store::new(64);
    s.watch_address(A);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    let r = s.apply_block(blk(2, 0), writes(&[(slot(1), 10)])).unwrap();
    assert_eq!(*r.changed_keys, vec![slot(1)]);
    s.apply_block(blk(3, 0), writes(&[(slot(1), 30)])).unwrap();

    assert_eq!(known(&s.snapshot(&blk(1, 0)).unwrap(), slot(1)), None);
    assert_eq!(known(&s.snapshot(&blk(2, 0)).unwrap(), slot(1)), Some(10));
    assert_eq!(known(&s.snapshot(&blk(3, 0)).unwrap(), slot(1)), Some(30));
    assert_eq!(known(&s.snapshot(&blk(3, 0)).unwrap(), slot(2)), None);
}

#[test]
fn rejects_non_child() {
    let s = Store::new(64);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    assert!(matches!(s.apply_block(blk(3, 0), writes(&[])), Err(StateError::NotChild(..))));
}

#[test]
fn untracked_writes_are_indexed_not_tracked() {
    let s = Store::new(64);
    s.apply_block(blk(1, 0), writes(&[(slot(1), 5)])).unwrap();
    // Known from the recent BAL (the block's own post-state), without becoming tracked.
    assert_eq!(known(&s.snapshot(&blk(1, 0)).unwrap(), slot(1)), Some(5));
    assert!(s.inner.read().versions.get(&slot(1)).is_none());
    assert_eq!(s.sizes().resolved_from_bal, 1);
}

#[test]
fn recent_bal_answers_only_inside_the_indexed_window() {
    let s = Store::new(4); // keeps the last 4 blocks' BALs
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.apply_block(blk(2, 0), writes(&[(slot(9), 7)])).unwrap();
    // Before the write, the BAL says nothing: unknown, never zero.
    assert_eq!(known(&s.snapshot(&blk(1, 0)).unwrap(), slot(9)), None);
    for n in 3..=5 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    // Written at 2, untouched since: that write is the value at 5.
    assert_eq!(known(&s.snapshot(&blk(5, 0)).unwrap(), slot(9)), Some(7));
    // A later write wins over an earlier one.
    s.apply_block(blk(6, 0), writes(&[(slot(9), 8)])).unwrap();
    assert_eq!(known(&s.snapshot(&blk(6, 0)).unwrap(), slot(9)), Some(8));
    // Once the writing blocks leave the window, the key is unknown again (a fetch is needed).
    for n in 7..=12 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    assert_eq!(known(&s.snapshot(&blk(12, 0)).unwrap(), slot(9)), None);
}

#[test]
fn recent_bal_lookup_follows_reorgs() {
    let s = Store::new(64);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.apply_block(blk(2, 0), writes(&[(slot(3), 30)])).unwrap();
    s.rollback_to(1);
    s.apply_block(blk(2, 1), writes(&[])).unwrap();
    // The orphaned block's write is gone with it.
    assert_eq!(known(&s.snapshot(&blk(2, 1)).unwrap(), slot(3)), None);
}

#[test]
fn base_insert_replays_later_writes_and_never_reaches_back() {
    let s = Store::new(64);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.apply_block(blk(2, 0), writes(&[])).unwrap();
    s.apply_block(blk(3, 0), writes(&[(slot(7), 50)])).unwrap(); // untracked yet: indexed only
    s.apply_block(blk(4, 0), writes(&[])).unwrap();

    // A SNAP fetch at block 2 returns 20 (arriving after block 3 wrote 50).
    s.insert_base(&blk(2, 0), slot(7), U256::from(20)).unwrap();

    assert_eq!(known(&s.snapshot(&blk(1, 0)).unwrap(), slot(7)), None);
    assert_eq!(known(&s.snapshot(&blk(2, 0)).unwrap(), slot(7)), Some(20));
    assert_eq!(known(&s.snapshot(&blk(3, 0)).unwrap(), slot(7)), Some(50));
    assert_eq!(known(&s.snapshot(&blk(4, 0)).unwrap(), slot(7)), Some(50));

    // Tracked now: new writes are recorded.
    s.apply_block(blk(5, 0), writes(&[(slot(7), 60)])).unwrap();
    assert_eq!(known(&s.snapshot(&blk(5, 0)).unwrap(), slot(7)), Some(60));
}

#[test]
fn bal_write_at_base_block_wins() {
    let s = Store::new(64);
    s.watch_address(A);
    s.apply_block(blk(1, 0), writes(&[(slot(1), 9)])).unwrap();
    s.insert_base(&blk(1, 0), slot(1), U256::from(1)).unwrap(); // stale fetch
    assert_eq!(known(&s.snapshot(&blk(1, 0)).unwrap(), slot(1)), Some(9));
}

#[test]
fn reorg_rolls_back_and_invalidates_snapshots() {
    let s = Store::new(64);
    s.watch_address(A);
    for n in 10..=12 {
        s.apply_block(blk(n, 0), writes(&[(slot(1), n)])).unwrap();
    }
    let old = s.snapshot(&blk(12, 0)).unwrap();
    s.rollback_to(11);
    assert!(matches!(old.get(&slot(1)), Err(StateError::Reorged { .. })));
    let mut b = blk(12, 1);
    b.parent_hash = blk(11, 0).hash;
    s.apply_block(b, writes(&[(slot(1), 99)])).unwrap();
    assert_eq!(known(&s.snapshot(&b).unwrap(), slot(1)), Some(99));
    assert_eq!(known(&s.snapshot(&blk(11, 0)).unwrap(), slot(1)), Some(11));
}

#[test]
fn gc_collapses_old_versions_into_a_base() {
    let s = Store::new(4);
    s.watch_address(A);
    for n in 1..=20 {
        s.apply_block(blk(n, 0), writes(&[(slot(1), n)])).unwrap();
    }
    assert!(matches!(s.snapshot(&blk(10, 0)), Err(StateError::OutOfWindow(10))));
    assert_eq!(known(&s.snapshot(&blk(20, 0)).unwrap(), slot(1)), Some(20));
    assert_eq!(known(&s.snapshot(&blk(17, 0)).unwrap(), slot(1)), Some(17));
    assert!(s.inner.read().versions[&slot(1)].len() <= 5);
}

#[tokio::test]
async fn emits_state_ready() {
    let s = Store::new(64);
    s.watch_address(A);
    let mut rx = s.subscribe();
    s.apply_block(blk(1, 0), writes(&[(slot(3), 1)])).unwrap();
    let ev = rx.recv().await.unwrap();
    assert_eq!(ev.block.number, 1);
    assert_eq!(*ev.changed_keys, vec![slot(3)]);
}

#[test]
fn on_demand_keys_stay_current_during_ttl_then_expire() {
    let s = Store::new(64);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.insert_base_with(&blk(1, 0), slot(4), U256::from(1), Retention::Ttl(2)).unwrap();
    // Within the TTL, BAL writes keep it current (no stale reuse).
    s.apply_block(blk(2, 0), writes(&[(slot(4), 2)])).unwrap();
    assert_eq!(known(&s.snapshot(&blk(2, 0)).unwrap(), slot(4)), Some(2));
    s.apply_block(blk(3, 0), writes(&[])).unwrap();
    assert_eq!(known(&s.snapshot(&blk(3, 0)).unwrap(), slot(4)), Some(2));
    // Past head+TTL it is evicted: no stored versions. Reads still get the true value from the
    // recent BAL (written at 2, untouched since), never the stale fetched one.
    s.apply_block(blk(4, 0), writes(&[])).unwrap();
    assert!(s.inner.read().versions.get(&slot(4)).is_none());
    assert_eq!(known(&s.snapshot(&blk(4, 0)).unwrap(), slot(4)), Some(2));
    assert_eq!(known(&s.snapshot(&blk(1, 0)).unwrap(), slot(4)), None); // the fetched base is gone
    // No longer tracked: a later write is not stored, only indexed.
    s.apply_block(blk(5, 0), writes(&[(slot(4), 5)])).unwrap();
    assert!(s.inner.read().versions.get(&slot(4)).is_none());
    assert_eq!(known(&s.snapshot(&blk(5, 0)).unwrap(), slot(4)), Some(5));
    assert_eq!(s.sizes().temp_keys, 0);
}

#[test]
fn ttl_never_downgrades_a_subscription_and_promotion_sticks() {
    let s = Store::new(64);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.watch_key(slot(1));
    s.insert_base_with(&blk(1, 0), slot(1), U256::from(7), Retention::Ttl(1)).unwrap();
    s.insert_base_with(&blk(1, 0), slot(2), U256::from(8), Retention::Ttl(1)).unwrap();
    s.watch_key(slot(2)); // promote
    for n in 2..=6 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    let snap = s.snapshot(&blk(6, 0)).unwrap();
    assert_eq!(known(&snap, slot(1)), Some(7));
    assert_eq!(known(&snap, slot(2)), Some(8));
    assert!(s.is_subscribed(&slot(2)));
}

#[test]
fn archived_keys_answer_blocks_outside_the_window() {
    let d = tempfile::tempdir().unwrap();
    let ar = archive::Archive::open(d.path().join("a.redb")).unwrap();
    let s = Store::with_archive(4, ar).unwrap();
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.insert_base(&blk(1, 0), slot(1), U256::from(100)).unwrap();
    s.insert_base(&blk(1, 0), slot(2), U256::from(200)).unwrap();
    s.archive_key(slot(1)); // slot 2 is subscribed but not archived
    for n in 2..=20 {
        let w: Vec<(StateKey, u64)> = if n % 5 == 0 { vec![(slot(1), n), (slot(2), n)] } else { vec![] };
        s.apply_block(blk(n, 0), writes(&w)).unwrap();
    }
    // Block 3 is far outside the 4-block window: served from the archive.
    let old = s.snapshot(&blk(3, 0)).unwrap();
    assert!(old.is_archived());
    assert_eq!(known(&old, slot(1)), Some(100));
    assert_eq!(known(&old, slot(2)), None); // not archived: never guessed
    assert_eq!(known(&s.snapshot(&blk(12, 0)).unwrap(), slot(1)), Some(10));
    assert_eq!(known(&s.snapshot(&blk(19, 0)).unwrap(), slot(1)), Some(15));
    assert_eq!(known(&s.snapshot(&blk(20, 0)).unwrap(), slot(1)), Some(20));
    assert_eq!(s.archive_errors(), 0);
    assert_eq!(s.block_hash(3), Some(blk(3, 0).hash));
}

#[test]
fn archive_follows_reorgs() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::with_archive(64, archive::Archive::open(d.path().join("a.redb")).unwrap()).unwrap();
    s.apply_block(blk(10, 0), writes(&[])).unwrap();
    s.insert_base(&blk(10, 0), slot(1), U256::from(1)).unwrap();
    s.archive_key(slot(1));
    s.apply_block(blk(11, 0), writes(&[(slot(1), 11)])).unwrap();
    s.apply_block(blk(12, 0), writes(&[(slot(1), 12)])).unwrap();
    s.rollback_to(11);
    let mut b = blk(12, 1);
    b.parent_hash = blk(11, 0).hash;
    s.apply_block(b, writes(&[(slot(1), 99)])).unwrap();
    let ar = s.archive().unwrap();
    assert_eq!(ar.get(&slot(1), 12).unwrap(), archive::ArchiveRead::Value(U256::from(99)));
    assert_eq!(ar.get(&slot(1), 11).unwrap(), archive::ArchiveRead::Value(U256::from(11)));
}

#[test]
fn code_is_cached_only_for_tracked_accounts() {
    let s = Store::new(64);
    let b = Address::repeat_byte(0xbb);
    s.watch_key(StateKey::CodeHash(A));
    let (ca, cb) = (Bytes::from_static(b"code-a"), Bytes::from_static(b"code-b"));
    let w = BlockWrites {
        writes: vec![
            (StateKey::CodeHash(A), U256::from_be_bytes(keccak256(&ca).0)),
            (StateKey::CodeHash(b), U256::from_be_bytes(keccak256(&cb).0)),
        ],
        code: vec![(keccak256(&ca), ca.clone()), (keccak256(&cb), cb)],
        ..Default::default()
    };
    s.apply_block(blk(1, 0), Arc::new(w)).unwrap();
    assert_eq!(s.code(&keccak256(&ca)), Some(ca));
    assert_eq!(s.sizes().code, 1); // the untracked deployment isn't cached
}

#[test]
fn blockhash_ring_outlives_a_short_window() {
    // Review finding: with the window holding the head, H-256 was gone after GC, and an unknown
    // hash read as zero. The ring keeps every hash BLOCKHASH can ask for.
    let s = Store::new(16);
    for n in 1..=300 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    assert_eq!(s.block_hash(300 - 256), Some(blk(300 - 256, 0).hash));
    assert_eq!(s.block_hash(299), Some(blk(299, 0).hash));
    assert_eq!(s.block_hash(300 - 258), None); // out of BLOCKHASH range: unknown, not zero
}

#[test]
fn ancestor_hashes_are_accepted_only_when_hash_linked() {
    let s = Store::new(16);
    for n in 100..=105 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    // The ring knows 99 (parent of 100) but nothing older.
    assert_eq!(s.oldest_known_hash(), Some((99, blk(99, 0).hash)));
    assert_eq!(s.block_hash(98), None);
    let h = |n: u64| blk(n, 0).hash;
    let forged = B256::repeat_byte(0xee);
    let chain = [(99, h(99), h(98)), (98, h(98), h(97)), (97, forged, h(96)), (96, h(96), h(95))];
    assert_eq!(s.insert_ancestor_hashes(&chain), 1); // 98 links; 97 doesn't, so the walk stops
    assert_eq!(s.block_hash(98), Some(h(98)));
    assert_eq!(s.block_hash(97), None);
    assert_eq!(s.block_hash(96), None);
}

#[test]
fn subscribed_key_limit_is_enforced_at_admission() {
    // Review finding: the limit only trimmed bookkeeping; fetched keys were already subscribed.
    let s = Store::new(64);
    s.set_max_subscribed(2);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    assert!(s.watch_key(slot(1)));
    s.insert_base(&blk(1, 0), slot(2), U256::from(2)).unwrap(); // Subscribed: admitted (2/2)
    assert!(!s.watch_key(slot(3))); // over the limit: refused
    s.insert_base(&blk(1, 0), slot(4), U256::from(4)).unwrap(); // Subscribed fetch: kept on demand
    assert!(s.is_subscribed(&slot(2)));
    assert!(!s.is_subscribed(&slot(3)));
    assert!(!s.is_subscribed(&slot(4)));
    assert_eq!(s.sizes().watched_keys, 2);
    assert_eq!(s.sizes().admissions_refused, 2);
    assert!(s.watch_key(slot(1))); // already subscribed: always fine
    // The refused key is still correct while cached, then expires instead of growing the store.
    assert_eq!(known(&s.snapshot(&blk(1, 0)).unwrap(), slot(4)), Some(4));
    for n in 2..=10 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    assert_eq!(s.sizes().temp_keys, 0);
}

#[test]
fn bytecode_is_dropped_with_its_last_account() {
    let s = Store::new(64);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    let c = Bytes::from_static(b"some-code");
    let h = s.insert_code(c.clone());
    s.insert_base_with(&blk(1, 0), StateKey::CodeHash(A), U256::from_be_bytes(h.0), Retention::Ttl(2)).unwrap();
    s.apply_block(blk(2, 0), writes(&[])).unwrap();
    assert_eq!(s.code(&h), Some(c));
    for n in 3..=6 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    assert_eq!(s.code(&h), None);
    assert_eq!(s.sizes().code, 0);
}

#[test]
fn a_base_write_and_a_rollback_never_interleave() {
    // Review finding: insert_base_with checked the fork and collected replay values under the
    // lock, released it, then wrote the archive. A rollback + new fork in between left the old
    // fork's replayed value in the archive. Now the write happens under the same lock as the
    // check, so the rollback can only run before or after it, never in between.
    use std::sync::{Barrier, atomic::{AtomicBool, Ordering}};
    let d = tempfile::tempdir().unwrap();
    let s = Store::with_archive(64, archive::Archive::open(d.path().join("a.redb")).unwrap()).unwrap();
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.archive_key(slot(1));
    // Fork A's block 2 writes K = 20 (seen in the BAL index before K's base arrives).
    s.apply_block(blk(2, 0), writes(&[(slot(1), 20)])).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let once = Arc::new(AtomicBool::new(false));
    {
        let (barrier, once) = (barrier.clone(), once.clone());
        *s.test_hook.lock() = Some(Box::new(move || {
            if !once.swap(true, Ordering::SeqCst) {
                barrier.wait(); // let the rollback thread go...
                std::thread::sleep(std::time::Duration::from_millis(100)); // ...and try to cut in
            }
        }));
    }
    let s2 = s.clone();
    let b2 = barrier.clone();
    let rollback = std::thread::spawn(move || {
        b2.wait();
        s2.rollback_to(1);
        s2.apply_block(blk(2, 1), writes(&[])).unwrap(); // fork B's block 2 leaves K alone
    });
    // Base for K at block 1 (= 10); replays fork A's write at 2 while the rollback waits.
    s.insert_base(&blk(1, 0), slot(1), U256::from(10)).unwrap();
    rollback.join().unwrap();
    let ar = s.archive().unwrap();
    assert_eq!(ar.get_at(&slot(1), 2, blk(2, 1).hash).unwrap(), archive::ArchiveRead::Value(U256::from(10)));
    assert_eq!(ar.get_at(&slot(1), 2, blk(2, 0).hash).unwrap(), archive::ArchiveRead::OtherFork);
    assert_eq!(known(&s.snapshot(&blk(2, 1)).unwrap(), slot(1)), Some(10));
}

#[test]
fn archive_admission_and_insert_are_atomic() {
    // Review finding: archive_key_checked admitted under a read lock and inserted later, so two
    // callers could both take the last slot. Now one write lock covers both.
    let d = tempfile::tempdir().unwrap();
    let s = Store::with_archive(64, archive::Archive::open(d.path().join("a.redb")).unwrap()).unwrap();
    s.set_max_subscribed(50);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    let handles: Vec<_> = (0..8)
        .map(|t| {
            let s = s.clone();
            std::thread::spawn(move || (0..20).filter(|i| s.archive_key_checked(slot(t * 100 + i))).count())
        })
        .collect();
    let admitted: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(admitted, 50);
    assert_eq!(s.sizes().watched_keys, 50);
}

#[test]
fn on_demand_keys_are_capped() {
    let s = Store::new(64);
    s.set_max_on_demand(100);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    for i in 0..1000 {
        s.insert_base_with(&blk(1, 0), slot(i), U256::from(i), Retention::Ttl(4)).unwrap();
        assert!(s.sizes().temp_keys <= 100);
    }
    assert!(s.sizes().on_demand_evicted >= 900);
    // The newest key is always kept (the call that fetched it can run).
    assert_eq!(known(&s.snapshot(&blk(1, 0)).unwrap(), slot(999)), Some(999));
}

#[test]
fn complete_storage_answers_every_slot_and_follows_writes() {
    let s = Store::new(64);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.watch_address(A);
    s.apply_block(blk(2, 0), writes(&[(slot(5), 55)])).unwrap(); // a write after the download block
    // Downloaded at block 1: slot 1 = 10, slot 5 = 50; everything else absent.
    let h = |i: u64| keccak256(U256::from(i).to_be_bytes::<32>());
    s.install_complete_storage(&blk(1, 0), A, vec![(h(1), U256::from(10)), (h(5), U256::from(50))]).unwrap();
    let at1 = s.snapshot(&blk(1, 0)).unwrap();
    let at2 = s.snapshot(&blk(2, 0)).unwrap();
    assert_eq!(known(&at1, slot(1)), Some(10));
    assert_eq!(known(&at1, slot(5)), Some(50));
    assert_eq!(known(&at2, slot(5)), Some(55)); // the later write wins
    assert_eq!(known(&at2, slot(999)), Some(0)); // absent from a complete trie: proven zero
    s.apply_block(blk(3, 0), writes(&[(slot(7), 70)])).unwrap(); // a brand-new slot
    assert_eq!(known(&s.snapshot(&blk(3, 0)).unwrap(), slot(7)), Some(70));
    // Slot 7 was first written after the download: it is held too and counts toward limits.
    assert_eq!(s.sizes().complete_slots, 3);
    assert_eq!(s.complete_storage_len(&A), Some(3));
    s.apply_block(blk(4, 0), writes(&[(slot(7), 71), (slot(1), 11)])).unwrap(); // known slots: no growth
    assert_eq!(s.complete_storage_len(&A), Some(3));
    // Dropping it (grew past its limit) stops tracking the contract.
    s.drop_complete_storage(&A);
    assert_eq!(s.complete_storage_len(&A), None);
    s.apply_block(blk(5, 0), writes(&[])).unwrap();
    assert!(s.inner.read().versions.get(&slot(7)).is_none());
    // Other contracts are unaffected: unknown stays unknown.
    assert_eq!(known(&at2, StateKey::Storage(Address::repeat_byte(0xcc), U256::from(1))), None);
}

#[test]
fn complete_storage_is_dropped_when_its_block_is_reorged_out_and_counts_toward_the_limit() {
    let s = Store::new(64);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.apply_block(blk(2, 0), writes(&[])).unwrap();
    s.watch_address(A);
    let h = |i: u64| keccak256(U256::from(i).to_be_bytes::<32>());
    s.install_complete_storage(&blk(2, 0), A, vec![(h(1), U256::from(10))]).unwrap();
    s.rollback_to(1);
    s.apply_block(blk(2, 1), writes(&[])).unwrap();
    assert_eq!(s.complete_storage_block(&A), None);
    assert_eq!(known(&s.snapshot(&blk(2, 1)).unwrap(), slot(1)), None); // unknown again, not 10
    // Limit: 3 slots don't fit under a limit of 2.
    s.set_max_subscribed(2);
    let r = s.install_complete_storage(&blk(2, 1), A, (1..=3).map(|i| (h(i), U256::from(i))).collect());
    assert!(matches!(r, Err(StateError::LimitReached(3))));
}

// ---- Third review: transitions between storage modes ----------------------------------------

#[test]
fn ttl_key_promoted_to_whole_storage_keeps_its_newer_versions() {
    // Review scenario: K fetched with a short TTL at 1, whole storage installed at 1 (K = 10),
    // block 2 writes K = 20, then the old TTL expires. K must read 20, never the download's 10.
    let s = Store::new(64);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.insert_base_with(&blk(1, 0), slot(4), U256::from(10), Retention::Ttl(2)).unwrap();
    s.watch_address(A);
    let h = keccak256(U256::from(4).to_be_bytes::<32>());
    s.install_complete_storage(&blk(1, 0), A, vec![(h, U256::from(10))]).unwrap();
    s.apply_block(blk(2, 0), writes(&[(slot(4), 20)])).unwrap();
    for n in 3..=8 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap(); // the TTL (1 + 2) is long gone
    }
    assert_eq!(known(&s.snapshot(&blk(8, 0)).unwrap(), slot(4)), Some(20));
}

#[test]
fn capacity_eviction_never_deletes_versions_a_subscription_owns() {
    let s = Store::new(4);
    s.set_max_on_demand(4);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.insert_base_with(&blk(1, 0), slot(1), U256::from(11), Retention::Ttl(100)).unwrap();
    s.watch_address(A); // A owns slot 1 now
    s.apply_block(blk(2, 0), writes(&[(slot(1), 12)])).unwrap();
    for n in 3..=8 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap(); // block 2's BAL leaves the window
    }
    let b = Address::repeat_byte(0xbb);
    for i in 0..20u64 {
        s.insert_base_with(&blk(8, 0), StateKey::Storage(b, U256::from(i)), U256::from(i), Retention::Ttl(100)).unwrap();
    }
    assert_eq!(known(&s.snapshot(&blk(8, 0)).unwrap(), slot(1)), Some(12));
}

#[test]
fn a_bal_only_value_is_promoted_when_a_key_is_subscribed() {
    let s = Store::new(4);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.apply_block(blk(2, 0), writes(&[(slot(6), 60)])).unwrap(); // untracked: BAL index only
    assert!(s.watch_key(slot(6)));
    assert_eq!(s.sizes().bal_promotions, 1);
    for n in 3..=12 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap(); // block 2 leaves the 4-block window
    }
    // Still known: the BAL value became a version when the key was subscribed.
    assert_eq!(known(&s.snapshot(&blk(12, 0)).unwrap(), slot(6)), Some(60));
}

#[test]
fn a_bal_only_value_is_archived_from_the_block_that_wrote_it() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::with_archive(4, archive::Archive::open(d.path().join("a.redb")).unwrap()).unwrap();
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.apply_block(blk(2, 0), writes(&[(slot(6), 60)])).unwrap();
    s.apply_block(blk(3, 0), writes(&[])).unwrap();
    assert!(s.archive_key_checked(slot(6)));
    for n in 4..=12 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    let old = s.snapshot(&blk(3, 0)).unwrap();
    assert!(old.is_archived());
    assert_eq!(known(&old, slot(6)), Some(60)); // coverage starts at block 2, where it was written
    assert_eq!(known(&s.snapshot(&blk(12, 0)).unwrap(), slot(6)), Some(60));
    assert_eq!(s.archive_errors(), 0);
}

#[test]
fn an_on_demand_read_of_a_bal_only_value_keeps_it() {
    let s = Store::new(4);
    s.apply_block(blk(1, 0), writes(&[(slot(8), 80)])).unwrap();
    for n in 2..=12 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
        s.touch_ttl(&[slot(8)], 4); // read every block
    }
    assert_eq!(known(&s.snapshot(&blk(12, 0)).unwrap(), slot(8)), Some(80));
    assert_eq!(s.sizes().temp_keys, 1);
}

#[test]
fn whole_storage_growth_is_held_to_the_global_budget() {
    let s = Store::new(64);
    s.set_max_subscribed(6);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    let (b, c) = (Address::repeat_byte(0xb1), Address::repeat_byte(0xc1));
    let h = |i: u64| keccak256(U256::from(i).to_be_bytes::<32>());
    for x in [b, c] {
        s.watch_address(x);
        s.install_complete_storage(&blk(1, 0), x, vec![(h(1), U256::from(1)), (h(2), U256::from(2))]).unwrap();
    }
    // 4 of 6 used. Each contract grows by 2 new slots: the second pushes the total past 6.
    let w = |x: Address, i: u64| (StateKey::Storage(x, U256::from(i)), i);
    s.apply_block(blk(2, 0), writes(&[w(b, 10), w(b, 11)])).unwrap();
    assert!(s.take_complete_drop(&b).is_none());
    s.apply_block(blk(3, 0), writes(&[w(c, 10), w(c, 11)])).unwrap();
    let why = s.take_complete_drop(&c).expect("c was dropped");
    assert!(why.contains("max_subscribed_keys"), "{why}");
    assert_eq!(s.complete_storage_len(&c), None);
    assert_eq!(s.complete_storage_len(&b), Some(4));
    // c no longer claims completeness: an unknown slot of c is unknown, not a proven zero.
    assert_eq!(known(&s.snapshot(&blk(3, 0)).unwrap(), StateKey::Storage(c, U256::from(999))), None);
}

#[test]
fn install_counts_slots_written_during_the_download_toward_the_budget() {
    let s = Store::new(64);
    s.set_max_subscribed(3);
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.watch_address(A);
    s.apply_block(blk(2, 0), writes(&[(slot(10), 1), (slot(11), 1)])).unwrap();
    let h = |i: u64| keccak256(U256::from(i).to_be_bytes::<32>());
    // 2 downloaded + 2 written since = 4 > 3.
    let r = s.install_complete_storage(&blk(1, 0), A, vec![(h(1), U256::from(1)), (h(2), U256::from(2))]);
    assert!(matches!(r, Err(StateError::LimitReached(4))), "{r:?}");
}

#[test]
fn the_recent_index_keeps_only_writes() {
    let s = Store::new(64);
    let full = BlockWrites {
        writes: vec![(slot(1), U256::from(5))],
        storage_reads: vec![(A, U256::from(9)); 10],
        touched: vec![A; 4],
        code: vec![(B256::repeat_byte(1), alloy_primitives::Bytes::from(vec![0u8; 1000]))],
    };
    s.apply_block(blk(1, 0), Arc::new(full)).unwrap();
    let z = s.sizes();
    assert_eq!((z.recent_writes, z.recent_reads, z.recent_touched, z.recent_code_bytes), (1, 0, 0, 0));
    // The writes still answer lookups.
    assert_eq!(known(&s.snapshot(&blk(1, 0)).unwrap(), slot(1)), Some(5));
}

// ---- Sixth review: a restart at a height the archive already holds -----------------------------

fn archive_with_a(path: &std::path::Path) {
    // Disk: A99 -> A100, K = 10 known at A99.
    let s = Store::with_archive(64, archive::Archive::open(path).unwrap()).unwrap();
    s.apply_block(blk(99, 0), writes(&[])).unwrap();
    s.insert_base(&blk(99, 0), slot(5), U256::from(10)).unwrap();
    s.archive_key(slot(5));
    s.apply_block(blk(100, 0), writes(&[])).unwrap();
    assert_eq!(s.archive_errors(), 0);
}

#[test]
fn a_restart_at_an_archived_height_never_stores_the_new_fork_under_the_old_hash() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.redb");
    archive_with_a(&path);
    // Offline, B99 -> B100 replaced A99 -> A100, and K became 20 in B99. The restarted store's
    // first block is B100; a fresh, verified lookup then proves K = 20 at B100.
    let s = Store::with_archive(64, archive::Archive::open(&path).unwrap()).unwrap();
    s.apply_block(blk(100, 1), writes(&[])).unwrap();
    s.insert_base_with(&blk(100, 1), slot(5), U256::from(20), Retention::Subscribed).unwrap();
    let ar = s.archive().unwrap();
    let (a99, a100, b100) = (blk(99, 0).hash, blk(100, 0).hash, blk(100, 1).hash);
    assert_ne!(ar.get_at(&slot(5), 100, a100).unwrap(), archive::ArchiveRead::Value(U256::from(20)), "B100's state under A100's hash");
    assert_eq!(ar.get_at(&slot(5), 99, a99).unwrap(), archive::ArchiveRead::Value(U256::from(10)));
    assert_eq!(ar.get_at(&slot(5), 100, b100).unwrap(), archive::ArchiveRead::Value(U256::from(20)));
}

#[test]
fn a_restart_at_the_same_archived_block_keeps_continuity() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.redb");
    archive_with_a(&path);
    let s = Store::with_archive(64, archive::Archive::open(&path).unwrap()).unwrap();
    s.apply_block(blk(100, 0), writes(&[])).unwrap(); // A100 again: already archived
    s.apply_block(blk(101, 0), writes(&[])).unwrap();
    let ar = s.archive().unwrap();
    assert_eq!(ar.segments().unwrap(), vec![(99, 101)]);
    assert_eq!(ar.get_at(&slot(5), 101, blk(101, 0).hash).unwrap(), archive::ArchiveRead::Value(U256::from(10)));
    assert_eq!(s.archive_errors(), 0);
}

/// Disk: A98 -> A99 -> A100 (seventh review, P1).
fn archive_a98_a100(path: &std::path::Path) {
    let s = Store::with_archive(64, archive::Archive::open(path).unwrap()).unwrap();
    for n in 98..=100 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    assert_eq!(s.archive().unwrap().segments().unwrap(), vec![(98, 100)]);
}

#[test]
fn blockhash_never_returns_an_archived_hash_from_an_abandoned_fork() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.redb");
    archive_a98_a100(&path);
    // Offline, A98 -> B99 -> B100 -> B101 replaced A99 -> A100. The restarted store's first
    // block is B101; reconciliation hasn't run, so A99 and A100 are still archived.
    let s = Store::with_archive(64, archive::Archive::open(&path).unwrap()).unwrap();
    let b101 = blk(101, 1);
    s.apply_block(b101, writes(&[])).unwrap();
    assert_eq!(s.archive().unwrap().hash(99).unwrap(), Some(blk(99, 0).hash), "the old fork is still on disk");
    assert_eq!(s.block_hash_at(&b101, 100), Some(blk(100, 1).hash));
    assert_eq!(s.block_hash_at(&b101, 99), None, "A99 is not an ancestor of B101: unknown, not A99");
    assert_eq!(s.block_hash_at(&b101, 98), None);
    // Once B99's hash is established by linked headers, it is answered.
    s.insert_ancestor_hashes(&[(100, blk(100, 1).hash, blk(99, 1).hash), (99, blk(99, 1).hash, blk(98, 0).hash)]);
    assert_eq!(s.block_hash_at(&b101, 99), Some(blk(99, 1).hash));
}

#[test]
fn blockhash_uses_archived_ancestors_that_link_to_the_simulated_block() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.redb");
    archive_a98_a100(&path);
    let s = Store::with_archive(64, archive::Archive::open(&path).unwrap()).unwrap();
    let a101 = blk(101, 0);
    s.apply_block(a101, writes(&[])).unwrap();
    assert_eq!(s.block_hash_at(&a101, 99), Some(blk(99, 0).hash));
    assert_eq!(s.block_hash_at(&a101, 98), Some(blk(98, 0).hash));
    assert_eq!(s.block_hash_at(&a101, 101), Some(a101.hash));
    assert_eq!(s.block_hash_at(&a101, 102), None);
    // A block that isn't on the chain the store holds gets nothing from it.
    assert_eq!(s.block_hash_at(&blk(101, 7), 99), None);
}

#[test]
fn archiving_a_long_unchanged_key_keeps_its_value_from_a_known_block() {
    // Seventh review, P2: K = 10 since block 1, unchanged and tracked past both the state window
    // and the hash ring, then promoted to archive retention.
    let d = tempfile::tempdir().unwrap();
    let s = Store::with_archive(64, archive::Archive::open(&d.path().join("a.redb")).unwrap()).unwrap();
    s.watch_key(slot(5));
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    s.insert_base(&blk(1, 0), slot(5), U256::from(10)).unwrap();
    for n in 2..=400 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    assert_eq!(known(&s.snapshot(&blk(400, 0)).unwrap(), slot(5)), Some(10), "the live value survives");
    s.archive_key(slot(5));
    s.apply_block(blk(401, 0), writes(&[])).unwrap();
    assert_eq!(s.archive_errors(), 0);
    let ar = s.archive().unwrap();
    for n in [400, 401] {
        assert_eq!(ar.get_at(&slot(5), n, blk(n, 0).hash).unwrap(), archive::ArchiveRead::Value(U256::from(10)), "block {n}");
    }
    // Coverage starts no earlier than a block whose hash the store still knew.
    assert!(matches!(ar.get_at(&slot(5), 1, blk(1, 0).hash).unwrap(), archive::ArchiveRead::NotCovered { .. }));
}

// ---- Eighth review: a pending whole-storage download keeps nothing ----------------------------

#[test]
fn a_failing_whole_storage_download_does_not_accumulate_the_contracts_writes() {
    // `storage: all` with a budget the download never fits: installation fails every time, and
    // meanwhile blocks keep writing new slots of the contract. Nothing may be retained for it.
    let s = Store::new(64);
    s.set_max_subscribed(2);
    let h = |i: u64| keccak256(U256::from(i).to_be_bytes::<32>());
    s.apply_block(blk(1, 0), writes(&[])).unwrap();
    for n in 2..=40 {
        let r = s.install_complete_storage(&blk(n - 1, 0), A, (1..=3).map(|i| (h(i), U256::from(i))).collect());
        assert!(matches!(r, Err(StateError::LimitReached(_))));
        s.apply_block(blk(n, 0), writes(&[(slot(100 + n), n)])).unwrap();
    }
    let z = s.sizes();
    assert_eq!((z.keys, z.versions, z.complete_slots), (0, 0, 0), "{z:?}");
    // Once it fits, the writes since the download block are replayed from the BAL index.
    s.set_max_subscribed(1000);
    s.install_complete_storage(&blk(30, 0), A, vec![(h(1), U256::from(1))]).unwrap();
    let at = s.snapshot(&blk(40, 0)).unwrap();
    assert_eq!(known(&at, slot(135)), Some(35));
    assert_eq!(known(&at, slot(1)), Some(1));
    assert_eq!(known(&at, slot(129)), Some(0), "written before the download block: the download's value (absent)");
}

#[test]
fn a_download_older_than_the_replay_window_is_refused() {
    let s = Store::new(16);
    let h = |i: u64| keccak256(U256::from(i).to_be_bytes::<32>());
    for n in 1..=400 {
        s.apply_block(blk(n, 0), writes(&[(slot(n), n)])).unwrap();
    }
    let oldest_indexed = *s.inner.read().recent.keys().next().unwrap();
    let r = s.install_complete_storage(&blk(oldest_indexed - 2, 0), A, vec![(h(1), U256::from(1))]);
    assert!(matches!(r, Err(StateError::OutOfWindow(_)) | Err(StateError::Reorged { .. })), "{r:?}");
    assert_eq!(s.complete_storage_len(&A), None);
}

#[test]
fn blocks_from_before_the_start_seed_the_bal_index_and_promote_waiting_keys() {
    // Started at block 20: a key subscribed now, last written at 17, is unknown (no BAL for it).
    let s = Store::new(8);
    s.apply_block(blk(20, 0), writes(&[])).unwrap();
    s.apply_block(blk(21, 0), writes(&[])).unwrap();
    assert!(s.watch_key(slot(1)));
    assert_eq!(known(&s.snapshot(&blk(21, 0)).unwrap(), slot(1)), None);
    // A run that doesn't end at a block the store knows is refused (another fork).
    let other: Vec<_> = (17..=19).map(|n| (blk(n, 7), writes(&[]))).collect();
    assert!(s.seed_recent(&other).is_err());
    // A gap in the run is refused too.
    let gappy = vec![(blk(17, 0), writes(&[(slot(1), 5)])), (blk(19, 0), writes(&[]))];
    assert!(s.seed_recent(&gappy).is_err());
    assert_eq!(known(&s.snapshot(&blk(21, 0)).unwrap(), slot(1)), None);
    // Blocks 13..=19, linked to the first applied block: 13 is outside the 8-block window.
    let run: Vec<_> = (13..=19).map(|n| (blk(n, 0), if n == 17 { writes(&[(slot(1), 5)]) } else { writes(&[]) })).collect();
    assert_eq!(s.seed_recent(&run).unwrap(), 6);
    // The waiting key is promoted from block 17's write, and stays current with later writes.
    assert_eq!(known(&s.snapshot(&blk(21, 0)).unwrap(), slot(1)), Some(5));
    assert_eq!(s.sizes().bal_promotions, 1);
    s.apply_block(blk(22, 0), writes(&[(slot(1), 6)])).unwrap();
    assert_eq!(known(&s.snapshot(&blk(22, 0)).unwrap(), slot(1)), Some(6));
    // A key never written in the seeded range stays unknown, never zero.
    assert!(s.watch_key(slot(2)));
    assert_eq!(known(&s.snapshot(&blk(22, 0)).unwrap(), slot(2)), None);
}

#[test]
fn an_archived_key_seeded_from_earlier_bals_gets_archive_coverage() {
    // Review finding: start at 20 with an archive; K is archived but unknown; startup seeding
    // supplies K = 5 from block 17. Its history must be readable from the archive's first block
    // (20, not 17), long after the memory window moved on.
    let d = tempfile::tempdir().unwrap();
    let s = Store::with_archive(8, archive::Archive::open(d.path().join("a.redb")).unwrap()).unwrap();
    s.apply_block(blk(20, 0), writes(&[])).unwrap();
    s.archive_key(slot(1));
    let run: Vec<_> = (13..=19).map(|n| (blk(n, 0), if n == 17 { writes(&[(slot(1), 5)]) } else { writes(&[]) })).collect();
    s.seed_recent(&run).unwrap();
    assert_eq!(known(&s.snapshot(&blk(20, 0)).unwrap(), slot(1)), Some(5));
    for n in 21..=40 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    let ar = s.archive().unwrap();
    assert_eq!(ar.get(&slot(1), 20).unwrap(), archive::ArchiveRead::Value(U256::from(5)));
    assert_eq!(ar.get(&slot(1), 30).unwrap(), archive::ArchiveRead::Value(U256::from(5)));
    assert!(matches!(ar.get(&slot(1), 17).unwrap(), archive::ArchiveRead::NotCovered { .. }), "never backdated before the archive");
}

#[test]
fn archived_keys_reloaded_after_a_restart_hold_no_slots_until_asked_for() {
    // Fifth soak: each restart archived a new account, the previous ones reloaded from the
    // archive as subscribed, and the limit filled with keys nothing asked for any more.
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.redb");
    {
        let s = Store::with_archive(64, archive::Archive::open(&path).unwrap()).unwrap();
        s.apply_block(blk(1, 0), writes(&[])).unwrap();
        for i in 1..=3 {
            s.archive_key(slot(i));
        }
    }
    let s = Store::with_archive(64, archive::Archive::open(&path).unwrap()).unwrap();
    s.set_max_subscribed(2);
    assert_eq!(s.archived_keys().len(), 3, "still archived");
    assert_eq!(s.sizes().watched_keys, 0, "but holding no subscribed slot");
    // The current config asks for slot 1 (archived before) and a new slot 9: both fit.
    assert!(s.archive_key_checked(slot(1)));
    assert!(s.archive_key_checked(slot(9)));
    assert_eq!(s.sizes().watched_keys, 2);
    assert!(!s.archive_key_checked(slot(10)), "the limit still holds");
}

#[test]
fn a_reloaded_archived_key_promoted_after_seeding_gets_a_base_in_the_new_segment() {
    // Review (reverse ordering): K was archived in an earlier run. After the restart (block 20)
    // the recent BALs are seeded first, with K = 6 from block 17, and only then does a
    // subscription ask for K. Its value must be archived in the new segment too.
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("a.redb");
    {
        let s = Store::with_archive(8, archive::Archive::open(&path).unwrap()).unwrap();
        s.apply_block(blk(1, 0), writes(&[(slot(1), 4)])).unwrap();
        s.archive_key(slot(1));
        s.apply_block(blk(2, 0), writes(&[])).unwrap();
    }
    let s = Store::with_archive(8, archive::Archive::open(&path).unwrap()).unwrap();
    s.apply_block(blk(20, 0), writes(&[])).unwrap();
    let run: Vec<_> = (13..=19).map(|n| (blk(n, 0), if n == 17 { writes(&[(slot(1), 6)]) } else { writes(&[]) })).collect();
    s.seed_recent(&run).unwrap();
    assert!(s.archive_key_checked(slot(1)), "subscribed again after the seeding");
    assert_eq!(known(&s.snapshot(&blk(20, 0)).unwrap(), slot(1)), Some(6));
    for n in 21..=40 {
        s.apply_block(blk(n, 0), writes(&[])).unwrap();
    }
    let ar = s.archive().unwrap();
    assert_eq!(ar.get(&slot(1), 20).unwrap(), archive::ArchiveRead::Value(U256::from(6)), "covered in the new segment");
    assert_eq!(ar.get(&slot(1), 35).unwrap(), archive::ArchiveRead::Value(U256::from(6)));
    assert_eq!(ar.get(&slot(1), 2).unwrap(), archive::ArchiveRead::Value(U256::from(4)), "the earlier segment is untouched");
}
