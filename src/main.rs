use gatekv::raft::{Msg, Raft, Role};
use gatekv::{Error, Result, kv::Kv, resp, store::Store, wire};
use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::{Duration, Instant};
use std::{env, thread};

const N: usize = 3;
const TICK: Duration = Duration::from_millis(50);
const USAGE: Error = Error::Invalid("usage: gatekv <id: 0-2> <dir> <addr0> <addr1> <addr2>");
const STOPPED: Error = Error::Invalid("node loop stopped");

enum Event {
    Peer(usize, Msg),
    Client(Vec<u8>, Sender<Vec<u8>>),
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let [id, dir, addrs @ ..] = args.as_slice() else {
        return Err(USAGE);
    };
    let id: u8 = id.parse().map_err(|_| USAGE)?;
    let addrs = addrs.iter().map(|a| a.parse().map_err(|_| USAGE));
    let addrs = addrs.collect::<Result<Vec<SocketAddr>>>()?;
    let me = usize::from(id);
    if me >= N || addrs.len() != N {
        return Err(USAGE);
    }
    if !is_x86_feature_detected!("sse4.2") {
        return Err(Error::Invalid("this CPU lacks SSE4.2"));
    }
    let (store, term, vote, log) = Store::open(Path::new(dir))?;
    let mut raft = Raft::new(me, N, RandomState::new().hash_one(id));
    (raft.term, raft.vote, raft.log) = (term, vote, log);
    let listener = TcpListener::bind(addrs[me])?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || accept(listener, tx));
    let mut peers = Vec::new();
    for (p, &addr) in addrs.iter().enumerate() {
        let (ptx, prx) = mpsc::sync_channel(256);
        if p != me {
            thread::spawn(move || dial(addr, id, prx));
        }
        peers.push(ptx);
    }
    run(raft, store, rx, peers, &addrs)
}

fn accept(listener: TcpListener, tx: Sender<Event>) {
    for stream in listener.incoming().flatten() {
        let tx = tx.clone();
        thread::spawn(move || serve(stream, &tx));
    }
}

fn serve(s: TcpStream, tx: &Sender<Event>) -> Result<()> {
    s.set_nodelay(true)?;
    let mut r = BufReader::new(s.try_clone()?);
    // Peers open with their id byte; RESP clients start with '*'.
    match r.fill_buf()?.first().copied() {
        Some(b'*') => client(r, s, tx),
        Some(p) if usize::from(p) < N => {
            r.consume(1);
            loop {
                let m = wire::read(&mut r)?;
                tx.send(Event::Peer(p.into(), m)).map_err(|_| STOPPED)?;
            }
        }
        _ => Ok(()),
    }
}

fn client(mut r: BufReader<TcpStream>, mut w: TcpStream, tx: &Sender<Event>) -> Result<()> {
    let (rtx, rrx) = mpsc::channel();
    while let Some(args) = resp::read(&mut r)? {
        let reply = if let Some(cmd) = resp::command(&args) {
            tx.send(Event::Client(cmd, rtx.clone()))
                .map_err(|_| STOPPED)?;
            rrx.recv().map_err(|_| STOPPED)?
        } else if matches!(args.as_slice(), [p] if p.eq_ignore_ascii_case(b"PING")) {
            b"+PONG\r\n".to_vec()
        } else {
            b"-ERR unknown command or wrong number of arguments\r\n".to_vec()
        };
        w.write_all(&reply)?;
    }
    Ok(())
}

fn dial(addr: SocketAddr, id: u8, rx: Receiver<Msg>) {
    let mut conn = None;
    for m in rx {
        if conn.is_none() {
            conn = connect(addr, id).ok();
        }
        // A failed write loses the message and the connection; Raft retransmits.
        if let Some(s) = &mut conn
            && s.write_all(&wire::encode(&m)).is_err()
        {
            conn = None;
        }
    }
}

fn connect(addr: SocketAddr, id: u8) -> io::Result<TcpStream> {
    let mut s = TcpStream::connect_timeout(&addr, TICK)?;
    s.set_nodelay(true)?;
    s.set_write_timeout(Some(10 * TICK))?;
    s.write_all(&[id])?;
    Ok(s)
}

fn run(
    mut raft: Raft,
    mut store: Store,
    rx: Receiver<Event>,
    peers: Vec<SyncSender<Msg>>,
    addrs: &[SocketAddr],
) -> Result<()> {
    let (mut kv, mut applied, mut waiting) = (Kv::default(), 0, HashMap::new());
    let mut deadline = Instant::now() + TICK;
    loop {
        let first = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        // Whatever is already queued joins the batch, so that one fsync covers it all.
        for event in first.into_iter().chain(rx.try_iter().take(1024)) {
            match event {
                Event::Peer(from, m) => raft.step(from, m),
                Event::Client(cmd, reply) => match raft.propose(cmd) {
                    Some(i) => _ = waiting.insert(i, reply),
                    None => _ = reply.send(redirect(raft.leader, addrs)),
                },
            }
        }
        if Instant::now() >= deadline {
            raft.tick();
            deadline = Instant::now() + TICK;
        }
        let ready = raft.ready();
        if ready.save_hard {
            store.save_meta(raft.term, raft.vote)?;
        }
        if let Some(from) = ready.save_from {
            store.save_log(from, &raft.log[from - 1..])?;
        }
        for (to, m) in ready.msgs {
            // A full queue means a slow or dead peer; dropping is safe, Raft retransmits.
            _ = peers[to].try_send(m);
        }
        while applied < raft.commit {
            let out = kv.apply(&raft.log[applied].cmd);
            applied += 1;
            if let Some(reply) = waiting.remove(&applied) {
                _ = reply.send(out);
            }
        }
        // Only a leader has waiting clients, and it never overwrites its own entries. Leadership
        // cannot be lost and regained within one batch, so a client whose entry may be lost is
        // always redirected here, before its index can be reused.
        if raft.role != Role::Leader {
            for (_, reply) in waiting.drain() {
                _ = reply.send(redirect(raft.leader, addrs));
            }
        }
    }
}

fn redirect(leader: Option<usize>, addrs: &[SocketAddr]) -> Vec<u8> {
    match leader {
        Some(l) => format!("-NOTLEADER {}\r\n", addrs[l]).into_bytes(),
        None => b"-NOTLEADER\r\n".to_vec(),
    }
}
