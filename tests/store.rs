//! Crash recovery of the on-disk log: torn tails at every offset and single corrupt bytes.

use gatekv::raft::Entry;
use gatekv::store::Store;
use std::fs;
use std::path::PathBuf;

fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gatekv-store-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// Entries of assorted sizes, the first with an empty command like a leader's no-op.
fn entries() -> Vec<Entry> {
    let entry = |i: u64| Entry {
        term: 1 + i / 4,
        cmd: vec![i as u8; (i * 7 % 23) as usize],
    };
    (0..12).map(entry).collect()
}

/// Byte offset just past each record: a 20-byte header (crc, term, length), then the command.
fn ends(entries: &[Entry]) -> Vec<usize> {
    let mut end = 0;
    entries
        .iter()
        .map(|e| {
            end += 20 + e.cmd.len();
            end
        })
        .collect()
}

#[test]
fn round_trips_and_rewrites_a_suffix() {
    let dir = dir("round-trip");
    let (mut store, term, vote, log) = Store::open(&dir).unwrap();
    assert_eq!((term, vote, log), (0, None, vec![]));
    store.save_log(1, &entries()).unwrap();
    store.save_meta(7, Some(2)).unwrap();
    let (mut store, term, vote, log) = Store::open(&dir).unwrap();
    assert_eq!((term, vote, log), (7, Some(2), entries()));
    // A follower overwriting a conflicting suffix.
    let mut want = entries()[..5].to_vec();
    want.push(Entry {
        term: 9,
        cmd: b"x".to_vec(),
    });
    store.save_log(6, &want[5..]).unwrap();
    store.save_meta(9, None).unwrap();
    let (_, term, vote, log) = Store::open(&dir).unwrap();
    assert_eq!((term, vote, log), (9, None, want));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_torn_tail_at_any_offset_leaves_the_complete_records() {
    let dir = dir("torn");
    let all = entries();
    Store::open(&dir).unwrap().0.save_log(1, &all).unwrap();
    let bytes = fs::read(dir.join("log")).unwrap();
    let ends = ends(&all);
    assert_eq!(bytes.len(), *ends.last().unwrap());
    for cut in 0..=bytes.len() {
        fs::write(dir.join("log"), &bytes[..cut]).unwrap();
        let (mut store, _, _, log) = Store::open(&dir).unwrap();
        let whole = ends.iter().filter(|&&end| end <= cut).count();
        assert_eq!(log, all[..whole], "cut at byte {cut}");
        // The partial record is cut off, so the next append starts on a record boundary.
        let boundary = if whole == 0 { 0 } else { ends[whole - 1] };
        assert_eq!(
            fs::metadata(dir.join("log")).unwrap().len(),
            boundary as u64
        );
        let extra = Entry {
            term: 20,
            cmd: b"after".to_vec(),
        };
        store
            .save_log(whole + 1, std::slice::from_ref(&extra))
            .unwrap();
        let (_, _, _, log) = Store::open(&dir).unwrap();
        assert_eq!(log[..whole], all[..whole]);
        assert_eq!(log[whole..], [extra], "cut at byte {cut}");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_corrupt_byte_ends_the_log_before_its_record() {
    let dir = dir("corrupt");
    let all = entries();
    Store::open(&dir).unwrap().0.save_log(1, &all).unwrap();
    let bytes = fs::read(dir.join("log")).unwrap();
    let ends = ends(&all);
    for at in 0..bytes.len() {
        for flip in [0x01, 0x80, 0xFF] {
            let mut bad = bytes.clone();
            bad[at] ^= flip;
            fs::write(dir.join("log"), &bad).unwrap();
            let (_, _, _, log) = Store::open(&dir).unwrap();
            let record = ends.iter().filter(|&&end| end <= at).count();
            assert_eq!(log, all[..record], "byte {at} flipped by {flip:#x}");
        }
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn damaged_meta_refuses_to_start() {
    let dir = dir("meta");
    Store::open(&dir).unwrap().0.save_meta(3, Some(1)).unwrap();
    // A leftover temporary file from a crash during save_meta is ignored.
    fs::write(dir.join("meta.tmp"), b"partial").unwrap();
    assert_eq!(Store::open(&dir).unwrap().1, 3);
    let meta = fs::read(dir.join("meta")).unwrap();
    for at in 0..meta.len() {
        let mut bad = meta.clone();
        bad[at] ^= 0x10;
        fs::write(dir.join("meta"), &bad).unwrap();
        assert!(Store::open(&dir).is_err(), "byte {at}");
        fs::write(dir.join("meta"), &meta[..at]).unwrap();
        assert!(Store::open(&dir).is_err(), "truncated to {at} bytes");
    }
    fs::remove_dir_all(&dir).unwrap();
}
