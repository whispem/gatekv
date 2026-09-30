use crate::asm::crc32c;
use crate::raft::Entry;
use crate::wire::{Rd, get_entry, put_entry};
use crate::{Error, Result};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Durable Raft state in `dir`: `log` holds one record per entry; `meta` holds a single
/// record whose term is the current term and whose command is the vote, or empty.
pub struct Store {
    dir: PathBuf,
    log: File,
    ends: Vec<u64>,
}

impl Store {
    /// Opens or creates the store and returns the persisted term, vote and log with it.
    pub fn open(dir: &Path) -> Result<(Store, u64, Option<usize>, Vec<Entry>)> {
        fs::create_dir_all(dir)?;
        let (term, vote) = match fs::read(dir.join("meta")) {
            Ok(b) => {
                let e = get_record(&mut Rd(&b))?;
                (e.term, Rd(&e.cmd).usize().ok())
            }
            Err(e) if e.kind() == ErrorKind::NotFound => (0, None),
            Err(e) => return Err(e.into()),
        };
        let path = dir.join("log");
        let log = File::options().append(true).create(true).open(&path)?;
        let buf = fs::read(&path)?;
        let (mut entries, mut ends, mut rd) = (Vec::new(), Vec::new(), Rd(&buf));
        // The longest valid prefix survives: a torn or corrupt record ends the log.
        while let Ok(e) = get_record(&mut rd) {
            entries.push(e);
            ends.push((buf.len() - rd.0.len()) as u64);
        }
        log.set_len(ends.last().copied().unwrap_or(0))?;
        log.sync_all()?;
        let dir = dir.to_path_buf();
        Ok((Store { dir, log, ends }, term, vote, entries))
    }

    pub fn save_meta(&self, term: u64, vote: Option<usize>) -> Result<()> {
        let cmd = vote.map_or(Vec::new(), |v| (v as u64).to_le_bytes().to_vec());
        let mut b = Vec::new();
        put_record(&mut b, &Entry { term, cmd });
        let tmp = self.dir.join("meta.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&b)?;
        f.sync_all()?;
        fs::rename(&tmp, self.dir.join("meta"))?;
        // The rename is durable only once the directory itself is synced.
        Ok(File::open(&self.dir)?.sync_all()?)
    }

    /// Durably replaces the entries from index `from` (1-based) on with `entries`.
    pub fn save_log(&mut self, from: usize, entries: &[Entry]) -> Result<()> {
        self.ends.truncate(from.saturating_sub(1));
        let start = self.ends.last().copied().unwrap_or(0);
        let mut b = Vec::new();
        for e in entries {
            put_record(&mut b, e);
            self.ends.push(start + b.len() as u64);
        }
        // The file is in append mode: after the truncation, writes land at `start`.
        self.log.set_len(start)?;
        self.log.write_all(&b)?;
        Ok(self.log.sync_data()?)
    }
}

/// Record: crc32c of the rest u32 | entry (see `wire::put_entry`).
fn put_record(b: &mut Vec<u8>, e: &Entry) {
    let at = b.len();
    b.extend([0; 4]);
    put_entry(b, e);
    let crc = crc32c(0, &b[at + 4..]);
    b[at..at + 4].copy_from_slice(&crc.to_le_bytes());
}

fn get_record(rd: &mut Rd) -> Result<Entry> {
    let crc = rd.u32()?;
    let rest = rd.0;
    let e = get_entry(rd)?;
    if crc32c(0, &rest[..rest.len() - rd.0.len()]) != crc {
        return Err(Error::Invalid("record checksum mismatch"));
    }
    Ok(e)
}
