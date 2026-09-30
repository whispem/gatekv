use crate::asm::crc32c;
use crate::raft::{BATCH, Body, Entry, Msg};
use crate::resp::MAX_BULK;
use crate::{Error, Result};
use std::io::Read;

pub const MAX_FRAME: usize = 16 << 20;
const HEADER: usize = 13;
const SHORT: Error = Error::Invalid("truncated input");

// A full batch of the largest commands (tag, key length, key, value) fits in one frame.
const _: () = assert!(BATCH * (16 + 9 + 2 * MAX_BULK) + 64 <= MAX_FRAME);

/// Little-endian cursor over untrusted bytes; every read is bounds-checked.
pub struct Rd<'a>(pub &'a [u8]);

impl<'a> Rd<'a> {
    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        let (a, rest) = self.0.split_first_chunk().ok_or(SHORT)?;
        self.0 = rest;
        Ok(*a)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let (a, rest) = self.0.split_at_checked(n).ok_or(SHORT)?;
        self.0 = rest;
        Ok(a)
    }

    pub fn u8(&mut self) -> Result<u8> {
        self.arr().map(|[b]| b)
    }

    pub fn u32(&mut self) -> Result<u32> {
        self.arr().map(u32::from_le_bytes)
    }

    pub fn u64(&mut self) -> Result<u64> {
        self.arr().map(u64::from_le_bytes)
    }

    pub fn usize(&mut self) -> Result<usize> {
        usize::try_from(self.u64()?).map_err(|_| Error::Invalid("integer out of range"))
    }
}

/// Entry: term u64 | len u64 | command.
pub fn put_entry(b: &mut Vec<u8>, e: &Entry) {
    put(b, &[e.term, e.cmd.len() as u64]);
    b.extend_from_slice(&e.cmd);
}

pub fn get_entry(rd: &mut Rd) -> Result<Entry> {
    let (term, len) = (rd.u64()?, rd.usize()?);
    let cmd = rd.bytes(len)?.to_vec();
    Ok(Entry { term, cmd })
}

/// Frame: len u64 | type u8 | crc32c(type ++ payload) u32 | payload, where the payload is
/// the sender's term u64 followed by the fields of the message type.
pub fn encode(m: &Msg) -> Vec<u8> {
    let mut b = vec![0; HEADER];
    put(&mut b, &[m.term]);
    let ty = match &m.body {
        Body::Vote(last, last_term) => {
            put(&mut b, &[*last as u64, *last_term]);
            1
        }
        Body::VoteResp(granted) => {
            b.push(u8::from(*granted));
            2
        }
        Body::Append(prev, prev_term, entries, commit) => {
            put(&mut b, &[*prev as u64, *prev_term, *commit as u64]);
            put(&mut b, &[entries.len() as u64]);
            entries.iter().for_each(|e| put_entry(&mut b, e));
            3
        }
        Body::AppendResp(ok, index) => {
            b.push(u8::from(*ok));
            put(&mut b, &[*index as u64]);
            4
        }
    };
    let crc = crc32c(crc32c(0, &[ty]), &b[HEADER..]);
    let len = (b.len() - HEADER) as u64;
    b[..8].copy_from_slice(&len.to_le_bytes());
    b[8] = ty;
    b[9..HEADER].copy_from_slice(&crc.to_le_bytes());
    b
}

/// Reads one frame. After an error the stream is out of sync and must be dropped.
pub fn read(r: &mut impl Read) -> Result<Msg> {
    let mut h = [0; HEADER];
    r.read_exact(&mut h)?;
    let mut rd = Rd(&h);
    let (len, ty, crc) = (rd.usize()?, rd.u8()?, rd.u32()?);
    if len > MAX_FRAME {
        return Err(Error::Invalid("frame too large"));
    }
    let mut p = vec![0; len];
    r.read_exact(&mut p)?;
    if crc32c(crc32c(0, &[ty]), &p) != crc {
        return Err(Error::Invalid("frame checksum mismatch"));
    }
    let mut rd = Rd(&p);
    let term = rd.u64()?;
    let body = match ty {
        1 => Body::Vote(rd.usize()?, rd.u64()?),
        2 => Body::VoteResp(rd.u8()? != 0),
        3 => {
            let (prev, prev_term, commit, n) = (rd.usize()?, rd.u64()?, rd.usize()?, rd.u64()?);
            let entries = (0..n).map(|_| get_entry(&mut rd)).collect::<Result<_>>()?;
            Body::Append(prev, prev_term, entries, commit)
        }
        4 => Body::AppendResp(rd.u8()? != 0, rd.usize()?),
        _ => return Err(Error::Invalid("unknown frame type")),
    };
    match rd.0 {
        [] => Ok(Msg { term, body }),
        _ => Err(Error::Invalid("trailing bytes in frame")),
    }
}

fn put(b: &mut Vec<u8>, xs: &[u64]) {
    for x in xs {
        b.extend(x.to_le_bytes());
    }
}
