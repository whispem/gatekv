//! The state machine against std's HashMap, and the RESP subset.

use gatekv::kv::Kv;
use gatekv::resp;
use std::collections::HashMap;

fn args(words: &[&str]) -> Vec<Vec<u8>> {
    words.iter().map(|w| w.as_bytes().to_vec()).collect()
}

#[test]
fn table_matches_a_hashmap_under_random_operations() {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let (mut kv, mut model) = (Kv::default(), HashMap::new());
    for step in 0..300_000u64 {
        // Few distinct keys, so that sets overwrite and deletes shift long probe runs.
        let key = format!("key:{}", next() % 3000).into_bytes();
        match next() % 3 {
            0 => {
                kv.set(key.clone(), step.to_le_bytes().to_vec());
                model.insert(key, step.to_le_bytes().to_vec());
            }
            1 => assert_eq!(kv.del(&key), model.remove(&key).is_some()),
            _ => assert_eq!(kv.get(&key), model.get(&key).map(Vec::as_slice)),
        }
    }
    for (key, val) in &model {
        assert_eq!(kv.get(key), Some(val.as_slice()));
    }
}

#[test]
fn applies_log_commands_and_answers_in_resp() {
    let mut kv = Kv::default();
    let mut run = |words: &[&str]| kv.apply(&resp::command(&args(words)).unwrap());
    assert_eq!(run(&["GET", "k"]), b"$-1\r\n");
    assert_eq!(run(&["set", "k", "hello world"]), b"+OK\r\n");
    assert_eq!(run(&["GET", "k"]), b"$11\r\nhello world\r\n");
    assert_eq!(run(&["SET", "k", ""]), b"+OK\r\n");
    assert_eq!(run(&["get", "k"]), b"$0\r\n\r\n");
    assert_eq!(run(&["DEL", "k"]), b":1\r\n");
    assert_eq!(run(&["DEL", "k"]), b":0\r\n");
    assert_eq!(kv.apply(b""), b"", "a leader's no-op");
    assert_eq!(
        kv.apply(b"S\xff\xff\xff\xff\xff\xff\xff\xffk"),
        b"-ERR malformed entry\r\n"
    );
}

#[test]
fn translates_only_the_supported_commands() {
    for bad in [
        &["PING"][..],
        &["GET"],
        &["GET", "a", "b"],
        &["SET", "a"],
        &["FLUSHALL"],
        &[],
    ] {
        assert_eq!(resp::command(&args(bad)), None, "{bad:?}");
    }
}

#[test]
fn reads_pipelined_arrays_of_bulk_strings() {
    let mut input: &[u8] =
        b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$0\r\n\r\n";
    assert_eq!(resp::read(&mut input).unwrap(), Some(args(&["GET", "k"])));
    assert_eq!(
        resp::read(&mut input).unwrap(),
        Some(args(&["SET", "k", ""]))
    );
    assert_eq!(resp::read(&mut input).unwrap(), None);
}

#[test]
fn rejects_malformed_or_oversized_input() {
    let long_header = format!("*{}\r\n", "1".repeat(40));
    let too_big = format!("*1\r\n${}\r\n", resp::MAX_BULK + 1);
    let bad: [&[u8]; 11] = [
        b"PING\r\n",
        b"*0\r\n",
        b"*4\r\n",
        b"*-1\r\n",
        b"*1\r\n$-1\r\n",
        b"*1\r\n$3\r\nabcd\r\n",
        b"*1\r\n$3\r\nab",
        b"*1\r\n:3\r\n",
        b"*2\r\n$1\r\na\r\n",
        long_header.as_bytes(),
        too_big.as_bytes(),
    ];
    for input in bad {
        assert!(
            resp::read(&mut &input[..]).is_err(),
            "{:?}",
            String::from_utf8_lossy(input)
        );
    }
}

#[test]
fn random_bytes_never_panic_the_parser() {
    let mut x = 7u64;
    for _ in 0..50_000 {
        let bytes: Vec<u8> = (0..(x % 40))
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                b"*$\r\n0123-"[(x % 9) as usize]
            })
            .collect();
        x = x.wrapping_add(1);
        let mut input = &bytes[..];
        while let Ok(Some(args)) = resp::read(&mut input) {
            _ = resp::command(&args);
        }
    }
}
