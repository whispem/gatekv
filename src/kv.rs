use crate::asm::hash;
use crate::wire::Rd;

#[derive(Clone)]
struct Slot {
    hash: u64,
    key: Vec<u8>,
    val: Vec<u8>,
}

/// Open addressing with linear probing over a power-of-two table at most 3/4 full. Deletion
/// shifts later entries of the probe run back instead of leaving tombstones.
#[derive(Default)]
pub struct Kv {
    slots: Vec<Option<Slot>>,
    len: usize,
}

impl Kv {
    /// Applies a log command and returns its RESP reply. Commands are `G key`, `D key` and
    /// `S klen:u64 key value`; anything else, such as a leader's empty no-op, does nothing.
    pub fn apply(&mut self, cmd: &[u8]) -> Vec<u8> {
        match cmd.split_first() {
            Some((b'G', key)) => match self.get(key) {
                Some(v) => [format!("${}\r\n", v.len()).as_bytes(), v, b"\r\n"].concat(),
                None => b"$-1\r\n".to_vec(),
            },
            Some((b'D', key)) => format!(":{}\r\n", u8::from(self.del(key))).into_bytes(),
            Some((b'S', rest)) => {
                let mut rd = Rd(rest);
                match rd.usize().and_then(|n| rd.bytes(n)) {
                    Ok(key) => {
                        self.set(key.to_vec(), rd.0.to_vec());
                        b"+OK\r\n".to_vec()
                    }
                    Err(_) => b"-ERR malformed entry\r\n".to_vec(),
                }
            }
            _ => Vec::new(),
        }
    }

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        let i = self.find(key, hash(key)).ok()?;
        self.slots[i].as_ref().map(|s| s.val.as_slice())
    }

    pub fn set(&mut self, key: Vec<u8>, val: Vec<u8>) {
        if 4 * (self.len + 1) > 3 * self.slots.len() {
            let cap = (2 * self.slots.len()).max(16);
            let old = std::mem::replace(&mut self.slots, vec![None; cap]);
            for s in old.into_iter().flatten() {
                let (Ok(i) | Err(i)) = self.find(&s.key, s.hash);
                self.slots[i] = Some(s);
            }
        }
        let hash = hash(&key);
        let (Ok(i) | Err(i)) = self.find(&key, hash);
        self.len += usize::from(self.slots[i].is_none());
        self.slots[i] = Some(Slot { hash, key, val });
    }

    pub fn del(&mut self, key: &[u8]) -> bool {
        let Ok(mut hole) = self.find(key, hash(key)) else {
            return false;
        };
        self.slots[hole] = None;
        self.len -= 1;
        let mask = self.slots.len() - 1;
        let mut i = hole;
        loop {
            i = (i + 1) & mask;
            let Some(s) = &self.slots[i] else {
                return true;
            };
            // Shift the entry into the hole unless its home slot lies cyclically in (hole, i].
            if (i.wrapping_sub(s.hash as usize) & mask) >= (i.wrapping_sub(hole) & mask) {
                self.slots[hole] = self.slots[i].take();
                hole = i;
            }
        }
    }

    /// Ok(slot holding `key`), or Err(the empty slot that ends its probe run).
    fn find(&self, key: &[u8], hash: u64) -> Result<usize, usize> {
        let Some(mask) = self.slots.len().checked_sub(1) else {
            return Err(0);
        };
        let mut i = hash as usize & mask;
        loop {
            match &self.slots[i] {
                None => return Err(i),
                Some(s) if s.hash == hash && s.key == key => return Ok(i),
                Some(_) => i = (i + 1) & mask,
            }
        }
    }
}
