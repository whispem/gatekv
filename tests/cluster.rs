//! Three real processes on localhost, driven over RESP: writes, a follower's redirect, the
//! leader killed, reads from the new leader, then a restart from disk and a second failover.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant};

struct Cluster {
    base: PathBuf,
    addrs: Vec<String>,
    nodes: Vec<Option<Child>>,
}

impl Cluster {
    fn start() -> Cluster {
        // Let the OS pick three free ports.
        let ports: Vec<TcpListener> = (0..3)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let addrs = ports
            .iter()
            .map(|l| l.local_addr().unwrap().to_string())
            .collect();
        drop(ports);
        let base = std::env::temp_dir().join(format!("gatekv-cluster-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut c = Cluster {
            base,
            addrs,
            nodes: (0..3).map(|_| None).collect(),
        };
        for i in 0..3 {
            c.spawn(i);
        }
        c
    }

    fn spawn(&mut self, i: usize) {
        let child = Command::new(env!("CARGO_BIN_EXE_gatekv"))
            .arg(i.to_string())
            .arg(self.base.join(i.to_string()))
            .args(&self.addrs)
            .spawn()
            .unwrap();
        self.nodes[i] = Some(child);
    }

    fn kill(&mut self, i: usize) {
        if let Some(mut child) = self.nodes[i].take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Sends a command to live nodes in turn, following redirects, until one of them answers
    /// with something other than a redirect. Returns that node and its reply.
    fn call(&self, args: &[&str]) -> (usize, String) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut i = 0;
        while Instant::now() < deadline {
            if self.nodes[i].is_some() {
                match request(&self.addrs[i], args) {
                    Some(reply) if reply.starts_with("-NOTLEADER") => {
                        let hint = reply.split(' ').nth(1);
                        let leader = self.addrs.iter().position(|a| Some(a.as_str()) == hint);
                        if let Some(l) = leader.filter(|&l| self.nodes[l].is_some()) {
                            i = l;
                            continue;
                        }
                    }
                    Some(reply) => return (i, reply),
                    None => {}
                }
            }
            i = (i + 1) % 3;
            sleep(Duration::from_millis(50));
        }
        panic!("no leader answered {args:?}");
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for i in 0..3 {
            self.kill(i);
        }
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// One RESP round trip; `None` if the node cannot be reached or does not answer.
fn request(addr: &str, args: &[&str]) -> Option<String> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut req = format!("*{}\r\n", args.len());
    for a in args {
        req += &format!("${}\r\n{a}\r\n", a.len());
    }
    stream.write_all(req.as_bytes()).ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let line = line.trim_end();
    match line.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) {
        Some(n) => {
            let mut bulk = vec![0; n + 2];
            reader.read_exact(&mut bulk).ok()?;
            Some(String::from_utf8_lossy(&bulk[..n]).into_owned())
        }
        None if line.is_empty() => None,
        None => Some(line.to_string()),
    }
}

fn assert_all_readable(c: &Cluster, keys: usize) {
    for k in 0..keys {
        assert_eq!(c.call(&["GET", &format!("key{k}")]).1, format!("value{k}"));
    }
}

#[test]
fn committed_writes_survive_leader_crashes_and_restarts() {
    let mut c = Cluster::start();
    for k in 0..20 {
        assert_eq!(
            c.call(&["SET", &format!("key{k}"), &format!("value{k}")]).1,
            "+OK"
        );
    }
    assert_eq!(c.call(&["DEL", "key19"]).1, ":1");
    assert_eq!(c.call(&["GET", "key19"]).1, "$-1");
    assert_eq!(request(&c.addrs[0], &["PING"]).unwrap(), "+PONG");

    let (leader, _) = c.call(&["GET", "key0"]);
    let follower = (leader + 1) % 3;
    let redirect = request(&c.addrs[follower], &["GET", "key0"]).unwrap();
    assert_eq!(redirect, format!("-NOTLEADER {}", c.addrs[leader]));

    c.kill(leader);
    assert_all_readable(&c, 19);
    let (second, reply) = c.call(&["SET", "after", "failover"]);
    assert_eq!(reply, "+OK");
    assert_ne!(second, leader);

    // The first leader comes back from its disk; then the second one goes down, so the
    // restarted node is needed for every quorum from here on.
    c.spawn(leader);
    sleep(Duration::from_secs(2));
    c.kill(second);
    assert_all_readable(&c, 19);
    assert_eq!(c.call(&["GET", "after"]).1, "failover");
    assert_eq!(c.call(&["GET", "key19"]).1, "$-1");
}
