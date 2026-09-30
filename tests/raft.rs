//! Deterministic single-process simulation of a cluster built on the pure Raft core, with
//! message loss, duplication and reordering, crashes and partitions. Every run is a function
//! of its seed; safety properties are checked after every step of every node.

use gatekv::raft::{Body, ELECTION, Entry, Msg, Raft, Role};
use std::collections::BTreeMap;

/// Seeds per scenario: 200 by default, `GATEKV_SEEDS=3000 cargo test --release --test raft`
/// for a longer soak.
fn seeds() -> u64 {
    std::env::var("GATEKV_SEEDS").map_or(200, |s| s.parse().expect("GATEKV_SEEDS: a number"))
}

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

#[derive(Clone, Default)]
struct Disk {
    term: u64,
    vote: Option<usize>,
    log: Vec<Entry>,
}

struct Sim {
    n: usize,
    nodes: Vec<Raft>,
    disks: Vec<Disk>,
    up: Vec<bool>,
    /// Partition: nodes only hear from nodes on the same side.
    side: Vec<bool>,
    /// In flight: delivery time, sender, receiver, message.
    net: Vec<(u64, usize, usize, Msg)>,
    now: u64,
    rng: Rng,
    loss: u64,
    dup: u64,
    delay: u64,
    /// Longest committed prefix seen on any node.
    committed: Vec<Entry>,
    leaders: BTreeMap<u64, usize>,
    cmds: u64,
}

impl Sim {
    fn new(n: usize, seed: u64) -> Sim {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let nodes = (0..n).map(|i| Raft::new(i, n, rng.next())).collect();
        Sim {
            n,
            nodes,
            disks: vec![Disk::default(); n],
            up: vec![true; n],
            side: vec![false; n],
            net: Vec::new(),
            now: 0,
            rng,
            loss: 0,
            dup: 0,
            delay: 2,
            committed: Vec::new(),
            leaders: BTreeMap::new(),
            cmds: 0,
        }
    }

    /// What the real driver does after each batch: persist, then send. Then check safety.
    fn flush(&mut self, i: usize) {
        let ready = self.nodes[i].ready();
        let (node, disk) = (&self.nodes[i], &mut self.disks[i]);
        if ready.save_hard {
            (disk.term, disk.vote) = (node.term, node.vote);
        }
        if let Some(from) = ready.save_from {
            assert!((1..=node.log.len()).contains(&from));
            disk.log.truncate(from - 1);
            disk.log.extend_from_slice(&node.log[from - 1..]);
        }
        // Everything the node relies on must have been flagged for persistence.
        assert_eq!((disk.term, disk.vote), (node.term, node.vote));
        assert_eq!(disk.log.len(), node.log.len());
        for (to, m) in ready.msgs {
            if self.rng.chance(self.loss) {
                continue;
            }
            let copies = if self.rng.chance(self.dup) { 2 } else { 1 };
            for _ in 0..copies {
                let at = self.now + 1 + self.rng.below(self.delay);
                self.net.push((at, i, to, m.clone()));
            }
        }
        self.check(i);
    }

    fn check(&mut self, i: usize) {
        let node = &self.nodes[i];
        assert!(node.commit <= node.log.len());
        if node.role == Role::Leader {
            if let Some(&first) = self.leaders.get(&node.term) {
                assert_eq!(first, i, "two leaders in term {}", node.term);
            } else {
                // Leader Completeness, checked at election: nothing committed so far is missing.
                self.leaders.insert(node.term, i);
                assert!(
                    node.log.starts_with(&self.committed),
                    "leader {i} lacks entries"
                );
            }
        }
        // State Machine Safety: committed entries agree everywhere and never change.
        let n = node.commit.min(self.committed.len());
        assert_eq!(
            node.log[..n],
            self.committed[..n],
            "node {i} commits a different entry"
        );
        if node.commit > self.committed.len() {
            let known = self.committed.len();
            self.committed
                .extend_from_slice(&node.log[known..node.commit]);
        }
    }

    /// Log Matching, on every pair of nodes, and memory against disk.
    fn check_logs(&self) {
        for (a, disk) in self.nodes.iter().zip(&self.disks) {
            assert_eq!(a.log, disk.log);
            for b in &self.nodes {
                let common = a.log.len().min(b.log.len());
                if let Some(k) = (0..common).rev().find(|&k| a.log[k].term == b.log[k].term) {
                    assert_eq!(a.log[..=k], b.log[..=k], "Log Matching violated");
                }
            }
        }
    }

    fn step(&mut self) {
        self.now += 1;
        for i in 0..self.n {
            if self.up[i] {
                self.nodes[i].tick();
                self.flush(i);
            }
        }
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.net)
            .into_iter()
            .partition(|m| m.0 <= self.now);
        self.net = later;
        for (_, from, to, m) in due {
            if self.up[to] && self.side[from] == self.side[to] {
                self.nodes[to].step(from, m);
                self.flush(to);
            }
        }
        if self.now.is_multiple_of(8) {
            self.check_logs();
        }
    }

    fn run(&mut self, ticks: u64) {
        for _ in 0..ticks {
            self.step();
        }
    }

    fn propose(&mut self, i: usize) -> Option<Vec<u8>> {
        self.cmds += 1;
        let cmd = self.cmds.to_le_bytes().to_vec();
        if !self.up[i] || self.nodes[i].propose(cmd.clone()).is_none() {
            return None;
        }
        self.flush(i);
        Some(cmd)
    }

    fn leader(&self) -> Option<usize> {
        let leaders = (0..self.n).filter(|&i| self.up[i] && self.nodes[i].role == Role::Leader);
        leaders.max_by_key(|&i| self.nodes[i].term)
    }

    fn crash(&mut self, i: usize) {
        self.up[i] = false;
    }

    fn restart(&mut self, i: usize) {
        let mut node = Raft::new(i, self.n, self.rng.next());
        let disk = self.disks[i].clone();
        (node.term, node.vote, node.log) = (disk.term, disk.vote, disk.log);
        self.nodes[i] = node;
        self.up[i] = true;
    }

    fn heal(&mut self) {
        self.side = vec![false; self.n];
        (self.loss, self.dup) = (0, 0);
        for i in 0..self.n {
            if !self.up[i] {
                self.restart(i);
            }
        }
    }

    /// Once the cluster is healthy, one leader must commit a new entry everywhere.
    fn assert_converges(&mut self, seed: u64) {
        self.run(40 * ELECTION);
        let l = self
            .leader()
            .unwrap_or_else(|| panic!("seed {seed}: no leader"));
        let cmd = self.propose(l).expect("the leader accepts a proposal");
        self.run(10 * ELECTION);
        let leader = &self.nodes[l];
        assert_eq!(
            leader.role,
            Role::Leader,
            "seed {seed}: leadership changed while healthy"
        );
        assert!(leader.log.iter().any(|e| e.cmd == cmd));
        for (i, node) in self.nodes.iter().enumerate() {
            assert_eq!(node.log, leader.log, "seed {seed}: node {i} diverges");
            assert_eq!(node.commit, node.log.len(), "seed {seed}: node {i} lags");
        }
        assert_eq!(self.committed, leader.log);
        self.check_logs();
    }
}

#[test]
fn elects_a_single_leader_and_replicates() {
    for seed in 0..seeds() {
        let mut sim = Sim::new(3, seed);
        sim.run(40 * ELECTION);
        let l = sim.leader().expect("a leader");
        for _ in 0..10 {
            sim.propose(l);
            sim.run(1);
        }
        sim.assert_converges(seed);
        let commands = sim.committed.iter().filter(|e| !e.cmd.is_empty()).count();
        assert_eq!(
            commands, 11,
            "seed {seed}: ten commands and the final one, each once"
        );
    }
}

#[test]
fn survives_a_leader_crash() {
    for seed in 0..seeds() {
        let mut sim = Sim::new(3, seed);
        sim.run(40 * ELECTION);
        let old = sim.leader().expect("a leader");
        for _ in 0..5 {
            sim.propose(old);
        }
        sim.run(5 * ELECTION);
        let (committed, term) = (sim.committed.clone(), sim.nodes[old].term);
        assert!(committed.len() >= 6, "seed {seed}");
        sim.crash(old);
        sim.run(40 * ELECTION);
        let new = sim.leader().expect("a new leader");
        assert!(new != old && sim.nodes[new].term > term);
        assert!(sim.nodes[new].log.starts_with(&committed));
        sim.restart(old);
        sim.assert_converges(seed);
        assert!(sim.committed.starts_with(&committed));
    }
}

#[test]
fn partitioned_leader_loses_only_uncommitted_entries() {
    for seed in 0..seeds() {
        let mut sim = Sim::new(5, seed);
        sim.run(40 * ELECTION);
        let old = sim.leader().expect("a leader");
        sim.propose(old);
        sim.run(5 * ELECTION);
        let before = sim.committed.clone();
        // The old leader and one follower end up in the minority.
        sim.side[old] = true;
        sim.side[(old + 1) % 5] = true;
        let orphans: Vec<Vec<u8>> = (0..5).filter_map(|_| sim.propose(old)).collect();
        assert_eq!(orphans.len(), 5);
        sim.run(40 * ELECTION);
        let new = sim.leader().expect("a majority leader");
        assert!(!sim.side[new]);
        for _ in 0..5 {
            sim.propose(new);
        }
        sim.run(10 * ELECTION);
        assert_eq!(
            sim.nodes[old].commit,
            before.len(),
            "seed {seed}: minority committed"
        );
        sim.heal();
        sim.assert_converges(seed);
        assert!(sim.committed.starts_with(&before));
        assert!(sim.committed.iter().all(|e| !orphans.contains(&e.cmd)));
        assert!(sim.committed.len() >= before.len() + 6);
    }
}

#[test]
fn safety_under_random_faults() {
    for seed in 0..seeds() {
        let n = if seed % 2 == 0 { 3 } else { 5 };
        let mut sim = Sim::new(n, seed);
        (sim.loss, sim.dup, sim.delay) = (10, 10, 2 * ELECTION / 3);
        for _ in 0..3000 {
            let i = sim.rng.below(n as u64) as usize;
            match sim.rng.below(200) {
                0 => sim.crash(i),
                1 | 2 if !sim.up[i] => sim.restart(i),
                3 => sim.side[i] = !sim.side[i],
                4 => sim.side = vec![false; n],
                _ => {}
            }
            if sim.rng.chance(10) {
                sim.propose(i);
            }
            sim.step();
        }
        sim.heal();
        sim.delay = 2;
        sim.assert_converges(seed);
    }
}

#[test]
fn arbitrary_messages_never_panic() {
    let mut rng = Rng(42);
    for seed in 0..50 {
        let mut node = Raft::new(0, 3, seed);
        for _ in 0..5000 {
            let mut index = || match rng.below(10) {
                0 => usize::MAX - rng.below(2) as usize,
                _ => rng.below(12) as usize,
            };
            let (a, b) = (index(), index());
            let term = rng.below(6);
            let entries = (0..rng.below(4))
                .map(|_| Entry {
                    term: rng.below(6),
                    cmd: vec![1],
                })
                .collect();
            let body = match rng.below(4) {
                0 => Body::Vote(a, term),
                1 => Body::VoteResp(rng.chance(50)),
                2 => Body::Append(a, term, entries, b),
                _ => Body::AppendResp(rng.chance(50), a),
            };
            node.step(
                rng.below(4) as usize,
                Msg {
                    term: rng.below(6),
                    body,
                },
            );
            if rng.chance(20) {
                node.tick();
            }
            if rng.chance(5) {
                node.propose(vec![2]);
            }
            let ready = node.ready();
            assert!(
                ready
                    .save_from
                    .is_none_or(|f| (1..=node.log.len()).contains(&f))
            );
            assert!(node.commit <= node.log.len());
        }
    }
}

fn node(id: usize, term: u64, terms: &[u64]) -> Raft {
    let mut r = Raft::new(id, 3, 1);
    (r.term, r.log) = (term, log(terms));
    r
}

fn log(terms: &[u64]) -> Vec<Entry> {
    let entry = |(i, &term)| Entry {
        term,
        cmd: vec![i as u8],
    };
    terms.iter().enumerate().map(entry).collect()
}

fn msg(term: u64, body: Body) -> Msg {
    Msg { term, body }
}

#[test]
fn delayed_append_never_truncates_matching_entries() {
    let mut f = node(1, 1, &[]);
    f.step(0, msg(1, Body::Append(0, 0, log(&[1, 1, 1]), 0)));
    f.step(0, msg(1, Body::Append(0, 0, log(&[1]), 0)));
    assert_eq!(f.log, log(&[1, 1, 1]));
    let replies: Vec<Body> = f.ready().msgs.into_iter().map(|(_, m)| m.body).collect();
    assert_eq!(
        replies,
        [Body::AppendResp(true, 3), Body::AppendResp(true, 1)]
    );
}

#[test]
fn follower_commit_stops_at_the_last_entry_it_was_sent() {
    // The last two entries are leftovers of term 2 that the term-3 leader may not have.
    let mut f = node(1, 2, &[1, 1, 2, 2]);
    f.step(0, msg(3, Body::Append(1, 1, log(&[1, 1])[1..].to_vec(), 4)));
    assert_eq!(f.commit, 2);
}

#[test]
fn leader_commits_by_counting_only_entries_of_its_term() {
    // Raft figure 8: an older entry stored on a majority is not committed yet.
    let mut l = node(0, 3, &[1, 2]);
    while l.role != Role::Candidate {
        l.tick();
    }
    l.step(1, msg(4, Body::VoteResp(true)));
    assert_eq!((l.role, l.log.len()), (Role::Leader, 3));
    l.step(1, msg(4, Body::AppendResp(true, 2)));
    assert_eq!(l.commit, 0);
    l.step(1, msg(4, Body::AppendResp(true, 3)));
    assert_eq!(l.commit, 3);
}

#[test]
fn stale_messages_change_nothing_and_learn_the_newer_term() {
    let mut f = node(1, 5, &[1, 5]);
    f.step(0, msg(3, Body::Append(0, 0, log(&[3]), 1)));
    f.step(2, msg(3, Body::Vote(9, 9)));
    assert_eq!((f.term, f.vote, f.commit, f.leader), (5, None, 0, None));
    assert_eq!(f.log, log(&[1, 5]));
    let replies: Vec<Msg> = f.ready().msgs.into_iter().map(|(_, m)| m).collect();
    assert_eq!(
        replies,
        [
            msg(5, Body::AppendResp(false, 0)),
            msg(5, Body::VoteResp(false))
        ]
    );
}

#[test]
fn committed_entries_survive_a_faulty_leader() {
    let mut f = node(1, 1, &[]);
    f.step(0, msg(1, Body::Append(0, 0, log(&[1, 1]), 2)));
    f.step(0, msg(1, Body::Append(0, 0, log(&[7]), 2)));
    assert_eq!((f.commit, f.log.clone()), (2, log(&[1, 1])));
}

#[test]
fn a_deposed_leader_waits_a_full_election_timeout() {
    let mut l = node(0, 1, &[]);
    while l.role != Role::Candidate {
        l.tick();
    }
    l.step(1, msg(l.term, Body::VoteResp(true)));
    l.step(2, msg(l.term + 1, Body::AppendResp(false, 0)));
    for _ in 1..ELECTION {
        l.tick();
        assert_eq!(l.role, Role::Follower);
    }
}
