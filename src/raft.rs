// In ticks: the heartbeat period and the election timeout's lower bound (it is drawn from
// [ELECTION, 2 * ELECTION)). BATCH caps the entries carried by one Append.
pub const HEARTBEAT: u64 = 2;
pub const ELECTION: u64 = 10;
pub const BATCH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub term: u64,
    pub cmd: Vec<u8>,
}

/// A message stamped with its sender's term. Log indices start at 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Msg {
    pub term: u64,
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Candidate's last log index and term.
    Vote(usize, u64),
    /// Vote granted.
    VoteResp(bool),
    /// Index and term of the entry preceding `entries`, the entries, and the leader's commit.
    Append(usize, u64, Vec<Entry>, usize),
    /// Success and the last index known to match, or failure and where to retry from.
    AppendResp(bool, usize),
}

/// Output accumulated since the previous `ready()`. The driver makes `term` and `vote`
/// durable if `save_hard`, then `log[save_from - 1..]` if set (`save_from <= log.len()`),
/// before sending `msgs`; entries up to `Raft::commit` may then be applied.
#[derive(Debug, Default)]
pub struct Ready {
    pub msgs: Vec<(usize, Msg)>,
    pub save_hard: bool,
    pub save_from: Option<usize>,
}

/// Node `id` of an `n`-node cluster (1 <= n <= 64), without I/O: time advances by `tick()`,
/// messages arrive through `step()`, and effects leave through `ready()`. After a restart,
/// set `term`, `vote` and `log` to their persisted values before the first call.
pub struct Raft {
    pub id: usize,
    pub n: usize,
    pub term: u64,
    pub vote: Option<usize>,
    pub log: Vec<Entry>,
    /// Never above `log.len()`: committed entries are never truncated.
    pub commit: usize,
    pub role: Role,
    pub leader: Option<usize>,
    votes: u64,
    /// While leading, 1 <= next[p] <= log.len() + 1.
    next: Vec<usize>,
    matched: Vec<usize>,
    /// Ticks left before the next heartbeat (leader) or election.
    timer: u64,
    rng: u64,
    out: Ready,
}

impl Raft {
    pub fn new(id: usize, n: usize, seed: u64) -> Self {
        let mut r = Raft {
            id,
            n,
            term: 0,
            vote: None,
            log: Vec::new(),
            commit: 0,
            role: Role::Follower,
            leader: None,
            votes: 0,
            next: Vec::new(),
            matched: Vec::new(),
            timer: 0,
            rng: seed | 1,
            out: Ready::default(),
        };
        r.reset_timer();
        r
    }

    pub fn tick(&mut self) {
        self.timer = self.timer.saturating_sub(1);
        if self.timer == 0 && self.role != Role::Leader {
            self.campaign();
        } else if self.timer == 0 {
            self.timer = HEARTBEAT;
            for p in self.peers() {
                self.send_append(p);
            }
        }
    }

    /// Appends `cmd` to the log if this node leads, and returns its index.
    pub fn propose(&mut self, cmd: Vec<u8>) -> Option<usize> {
        if self.role != Role::Leader {
            return None;
        }
        let term = self.term;
        self.log.push(Entry { term, cmd });
        self.dirty(self.log.len());
        self.advance();
        Some(self.log.len())
    }

    pub fn ready(&mut self) -> Ready {
        if self.role == Role::Leader {
            for p in self.peers() {
                if self.next[p] <= self.log.len() {
                    self.send_append(p);
                }
            }
        }
        // A reply queued before a term change in this batch may acknowledge entries that a
        // newer leader has since overwritten. Dropping it keeps every acknowledgment true of
        // the persisted log, and losing a message is always safe.
        let term = self.term;
        self.out.msgs.retain(|(_, m)| m.term == term);
        std::mem::take(&mut self.out)
    }

    pub fn step(&mut self, from: usize, m: Msg) {
        if from >= self.n || from == self.id {
            return;
        }
        if m.term > self.term {
            self.observe(m.term);
        }
        if m.term < self.term {
            // Answer stale requests so that their sender learns the newer term.
            match m.body {
                Body::Vote(..) => self.send(from, Body::VoteResp(false)),
                Body::Append(..) => self.send(from, Body::AppendResp(false, 0)),
                _ => {}
            }
            return;
        }
        match m.body {
            Body::Vote(last, last_term) => {
                let up_to_date = (last_term, last) >= (self.last_term(), self.log.len());
                let granted = up_to_date && self.vote.is_none_or(|v| v == from);
                if granted {
                    self.vote = Some(from);
                    self.out.save_hard = true;
                    self.reset_timer();
                }
                self.send(from, Body::VoteResp(granted));
            }
            Body::VoteResp(granted) => {
                if granted && self.role == Role::Candidate {
                    self.votes |= 1 << from;
                    self.tally();
                }
            }
            Body::Append(prev, prev_term, entries, commit) => {
                self.role = Role::Follower;
                self.leader = Some(from);
                self.reset_timer();
                let reply = self.accept(prev, prev_term, entries, commit);
                self.send(from, reply);
            }
            Body::AppendResp(ok, index) => {
                // An index past our log could only come from a faulty peer; it would break
                // the bounds on next[from].
                if self.role != Role::Leader || index > self.log.len() {
                    return;
                }
                if ok {
                    self.matched[from] = self.matched[from].max(index);
                    self.next[from] = self.next[from].max(index + 1);
                    self.advance();
                } else {
                    self.next[from] = self.next[from].min(index + 1);
                }
            }
        }
    }

    fn term_at(&self, i: usize) -> Option<u64> {
        match i {
            0 => Some(0),
            _ => self.log.get(i - 1).map(|e| e.term),
        }
    }

    fn accept(&mut self, prev: usize, prev_term: u64, entries: Vec<Entry>, commit: usize) -> Body {
        if self.term_at(prev) != Some(prev_term) {
            let hint = self.log.len().min(prev.saturating_sub(1));
            return Body::AppendResp(false, hint);
        }
        let last = prev + entries.len();
        for (i, e) in (prev + 1..).zip(entries) {
            if self.term_at(i) != Some(e.term) {
                // Committed entries are final: only a faulty peer could contradict them.
                if i <= self.commit {
                    return Body::AppendResp(false, self.commit);
                }
                self.log.truncate(i - 1);
                self.log.push(e);
                self.dirty(i);
            }
        }
        // Entries past `last` may predate this leader, so they must not count as matching.
        self.commit = self.commit.max(commit.min(last));
        Body::AppendResp(true, last)
    }

    fn campaign(&mut self) {
        self.term += 1;
        self.role = Role::Candidate;
        self.vote = Some(self.id);
        self.votes = 1 << self.id;
        self.leader = None;
        self.out.save_hard = true;
        self.reset_timer();
        let (last, last_term) = (self.log.len(), self.last_term());
        for p in self.peers() {
            self.send(p, Body::Vote(last, last_term));
        }
        self.tally();
    }

    fn tally(&mut self) {
        if 2 * self.votes.count_ones() as usize > self.n {
            self.role = Role::Leader;
            self.leader = Some(self.id);
            self.next = vec![self.log.len() + 1; self.n];
            self.matched = vec![0; self.n];
            self.timer = HEARTBEAT;
            // A no-op of the new term lets entries of earlier terms commit (Raft §5.4.2).
            self.propose(Vec::new());
        }
    }

    fn observe(&mut self, term: u64) {
        self.term = term;
        self.vote = None;
        self.role = Role::Follower;
        self.leader = None;
        self.out.save_hard = true;
        self.reset_timer();
    }

    fn advance(&mut self) {
        let mut acked = self.matched.clone();
        acked[self.id] = self.log.len();
        acked.sort_unstable();
        // A majority of nodes hold every entry up to the lower median.
        let c = acked[(self.n - 1) / 2];
        // Only an entry of the current term is committed by counting replicas (Raft §5.4.2).
        if c > self.commit && self.term_at(c) == Some(self.term) {
            self.commit = c;
        }
    }

    fn send_append(&mut self, p: usize) {
        let prev = self.next[p] - 1;
        let entries: Vec<Entry> = self.log[prev..].iter().take(BATCH).cloned().collect();
        // Pipelined: assume delivery, and let a rejection move next[p] back.
        self.next[p] = prev + entries.len() + 1;
        let prev_term = self.term_at(prev).unwrap_or(0);
        self.send(p, Body::Append(prev, prev_term, entries, self.commit));
    }

    fn send(&mut self, to: usize, body: Body) {
        let term = self.term;
        self.out.msgs.push((to, Msg { term, body }));
    }

    fn dirty(&mut self, i: usize) {
        self.out.save_from = Some(self.out.save_from.map_or(i, |f| f.min(i)));
    }

    fn last_term(&self) -> u64 {
        self.log.last().map_or(0, |e| e.term)
    }

    fn peers(&self) -> Vec<usize> {
        (0..self.n).filter(|&p| p != self.id).collect()
    }

    fn reset_timer(&mut self) {
        // xorshift64; `rng` is odd at construction, so never zero.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.timer = ELECTION + self.rng % ELECTION;
    }
}
