use crate::{Error, Result};
use std::io::{BufRead, Read};

pub const MAX_BULK: usize = 64 << 10;

/// Reads one command, sent as an array of bulk strings; `None` at a clean end of stream.
pub fn read(r: &mut impl BufRead) -> Result<Option<Vec<Vec<u8>>>> {
    let Some(n) = header(r, b'*')? else {
        return Ok(None);
    };
    if !(1..=3).contains(&n) {
        return Err(Error::Invalid("commands take one to three arguments"));
    }
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        let len = header(r, b'$')?.ok_or(Error::Invalid("truncated command"))?;
        if len > MAX_BULK {
            return Err(Error::Invalid("bulk string too long"));
        }
        let mut arg = vec![0; len + 2];
        r.read_exact(&mut arg)?;
        if arg.split_off(len) != b"\r\n" {
            return Err(Error::Invalid("bulk string without CRLF"));
        }
        args.push(arg);
    }
    Ok(Some(args))
}

/// Translates GET, SET and DEL into the log commands that `Kv::apply` executes.
pub fn command(args: &[Vec<u8>]) -> Option<Vec<u8>> {
    let [name, rest @ ..] = args else {
        return None;
    };
    let mut cmd = match (name.to_ascii_uppercase().as_slice(), rest) {
        (b"GET", [_]) => b"G".to_vec(),
        (b"DEL", [_]) => b"D".to_vec(),
        (b"SET", [key, _]) => [b"S".as_slice(), &(key.len() as u64).to_le_bytes()].concat(),
        _ => return None,
    };
    rest.iter().for_each(|a| cmd.extend_from_slice(a));
    Some(cmd)
}

fn header(r: &mut impl BufRead, tag: u8) -> Result<Option<usize>> {
    let mut line = Vec::new();
    // Bounded, so that a peer sending no newline cannot grow `line` forever.
    Read::take(&mut *r, 32).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Ok(None);
    }
    let digits = line
        .strip_prefix(&[tag])
        .and_then(|l| l.strip_suffix(b"\r\n"));
    let n = digits.and_then(|d| std::str::from_utf8(d).ok()?.parse().ok());
    n.map(Some).ok_or(Error::Invalid("malformed RESP header"))
}
