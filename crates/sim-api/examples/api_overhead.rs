//! Per-call overhead of `simee serve` (one kept connection, and a new connection per call) and of
//! `simee mcp` (stdio), for methods that need no booted device. Each binary named gets its own
//! server on a private socket and its own MCP child; rounds interleave the binaries, and each cell
//! is the median over rounds of the mean time per call in a round. The first reply of each method
//! is kept per binary, so binaries whose replies differ are reported (`identity` names its build
//! and executable, so those are masked first).
//!
//!     cargo build --release -p simee
//!     cargo run --release -p simee-api --example api_overhead -- \
//!         [--rounds 5] [--socket-dir DIR] before=/path/to/old/simee after=target/release/simee
use sim_core::net::UnixStream;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// The methods timed: a no-op, the method list, a pure table, and a host listing (no boot).
const METHODS: &[&str] = &["identity", "help", "form_factors", "devices"];
/// A round's budget per cell: stop at this many calls or this long, whichever comes first.
const MAX_CALLS: usize = 2000;
const BUDGET: Duration = Duration::from_millis(200);
const MIN_CALLS: usize = 5;

struct Target {
    label: String,
    /// What `identity` names that differs between builds.
    identity: Vec<String>,
    socket: PathBuf,
    server: Child,
    mcp: Mcp,
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(exe: &Path) -> Mcp {
        let mut child = Command::new(exe)
            .arg("mcp")
            .env("SIMEE_MCP_TOOLSETS", "all")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("simee mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Mcp { child, stdin, stdout, next_id: 1 }
    }

    /// One `tools/call` in the current revision; the response line, with its id.
    fn call(&mut self, tool: &str, line: &mut Vec<u8>) {
        let id = self.next_id;
        self.next_id += 1;
        let request = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":\"{tool}\",\"arguments\":{{}},\
             \"_meta\":{{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\"}}}}}}\n"
        );
        self.stdin.write_all(request.as_bytes()).expect("mcp write");
        line.clear();
        self.stdout.read_until(b'\n', line).expect("mcp read");
        assert!(!line.is_empty(), "simee mcp closed its stdout");
        let reply = String::from_utf8_lossy(line);
        assert!(!reply.contains("\"error\":") && !reply.contains("\"isError\":true"), "simee mcp refused `{tool}`: {reply}");
    }
}

/// One request on a kept connection: the response line.
fn exchange(stream: &mut BufReader<UnixStream>, request: &[u8], line: &mut Vec<u8>) {
    stream.get_mut().write_all(request).expect("write");
    line.clear();
    stream.read_until(b'\n', line).expect("read");
    assert!(!line.is_empty(), "the server closed the connection");
}

fn request(method: &str) -> Vec<u8> {
    format!("{{\"id\":1,\"method\":\"{method}\"}}\n").into_bytes()
}

/// Mean microseconds per call of `f` over one round's budget.
fn round(mut f: impl FnMut()) -> f64 {
    let start = Instant::now();
    let mut n = 0;
    while n < MAX_CALLS && (n < MIN_CALLS || start.elapsed() < BUDGET) {
        f();
        n += 1;
    }
    start.elapsed().as_secs_f64() * 1e6 / n as f64
}

fn wait_for_socket(path: &Path) {
    let start = Instant::now();
    while UnixStream::connect(path).is_err() {
        assert!(start.elapsed() < Duration::from_secs(10), "{} never listened", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `line` with the server's own build and executable masked: `identity` names them, and they
/// differ between any two builds by design.
fn masked(line: &[u8], identity: &[String]) -> Vec<u8> {
    let mut text = String::from_utf8_lossy(line).into_owned();
    for value in identity {
        text = text.replace(value.as_str(), "<masked>");
    }
    text.into_bytes()
}

/// The `build` and `exe` a server's `identity` gives.
fn identity_of(socket: &Path) -> Vec<String> {
    let mut stream = BufReader::new(UnixStream::connect(socket).expect("connect"));
    let mut line = Vec::new();
    exchange(&mut stream, &request("identity"), &mut line);
    let reply: serde_json::Value = serde_json::from_slice(&line).expect("identity");
    ["build", "exe"].iter().filter_map(|k| reply["result"][k].as_str().map(str::to_owned)).collect()
}

fn main() {
    let mut rounds = 5;
    let mut socket_dir = std::env::temp_dir();
    let mut specs = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--rounds" => rounds = args.next().and_then(|n| n.parse().ok()).expect("--rounds N"),
            "--socket-dir" => socket_dir = args.next().map(PathBuf::from).expect("--socket-dir DIR"),
            spec => {
                let (label, exe) = spec.split_once('=').expect("label=path/to/simee");
                specs.push((label.to_owned(), PathBuf::from(exe)));
            }
        }
    }
    assert!(!specs.is_empty(), "name at least one label=path/to/simee");
    let mut targets: Vec<Target> = specs
        .into_iter()
        .map(|(label, exe)| {
            let socket = socket_dir.join(format!("ovh-{label}-{}.sock", std::process::id()));
            std::fs::remove_file(&socket).ok();
            let server = Command::new(&exe)
                .arg("serve")
                .arg("--socket")
                .arg(&socket)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("simee serve");
            let mcp = Mcp::start(&exe);
            eprintln!("{label}: serve pid {} on {}, mcp pid {}", server.id(), socket.display(), mcp.child.id());
            wait_for_socket(&socket);
            let identity = identity_of(&socket);
            Target { label, identity, socket, server, mcp }
        })
        .collect();

    let transports = ["kept", "per-call", "mcp"];
    // cells[target][transport][method] = per-round means
    let mut cells = vec![vec![vec![Vec::new(); METHODS.len()]; transports.len()]; targets.len()];
    let mut first: Vec<Vec<Vec<u8>>> = vec![Vec::new(); targets.len()];
    let mut first_mcp: Vec<Vec<Vec<u8>>> = vec![Vec::new(); targets.len()];
    let mut line = Vec::new();
    let mut kept: Vec<_> = targets.iter().map(|t| BufReader::new(UnixStream::connect(&t.socket).expect("connect"))).collect();
    // A warm-up, whose first replies are kept for comparing.
    for (t, target) in targets.iter_mut().enumerate() {
        for method in METHODS {
            exchange(&mut kept[t], &request(method), &mut line);
            first[t].push(masked(&line, &target.identity));
            target.mcp.call(method, &mut line);
            first_mcp[t].push(masked(&line, &target.identity));
        }
    }
    // Each cell's rounds alternate between the binaries right next to each other, in turn first,
    // so the host's load drifting during the run falls on both alike.
    for r in 0..rounds {
        for (m, method) in METHODS.iter().enumerate() {
            let req = request(method);
            for (i, _) in transports.iter().enumerate() {
                let mut order: Vec<usize> = (0..targets.len()).collect();
                if r % 2 == 1 {
                    order.reverse();
                }
                for t in order {
                    let target = &mut targets[t];
                    let us = match i {
                        0 => round(|| exchange(&mut kept[t], &req, &mut line)),
                        1 => round(|| {
                            let mut fresh = BufReader::new(UnixStream::connect(&target.socket).expect("connect"));
                            exchange(&mut fresh, &req, &mut line);
                        }),
                        _ => round(|| target.mcp.call(method, &mut line)),
                    };
                    cells[t][i][m].push(us);
                }
            }
        }
    }
    drop(kept);

    println!("µs per call, median of {rounds} rounds (each the mean of up to {MAX_CALLS} calls or {} ms)", BUDGET.as_millis());
    print!("{:<10} {:<13}", "transport", "method");
    for target in &targets {
        print!(" {:>12}", target.label);
    }
    println!();
    for (i, transport) in transports.iter().enumerate() {
        for (m, method) in METHODS.iter().enumerate() {
            print!("{transport:<10} {method:<13}");
            for cell in &mut cells {
                let v = &mut cell[i][m];
                v.sort_by(f64::total_cmp);
                print!(" {:>12.1}", v[v.len() / 2]);
            }
            println!();
        }
    }
    for t in 1..targets.len() {
        for (m, method) in METHODS.iter().enumerate() {
            for (kind, replies) in [("serve", &first), ("mcp", &first_mcp)] {
                let same = replies[0][m] == replies[t][m];
                println!(
                    "{kind} {method} reply, {} vs {}: {}",
                    targets[0].label,
                    targets[t].label,
                    if same {
                        "identical bytes".to_owned()
                    } else {
                        format!("DIFFERENT ({} vs {} bytes)", replies[0][m].len(), replies[t][m].len())
                    }
                );
            }
        }
    }
}

/// Stops this target's two children however the run ends, a panic included.
impl Drop for Target {
    fn drop(&mut self) {
        drop(self.mcp.child.kill());
        drop(self.mcp.child.wait());
        drop(self.server.kill());
        drop(self.server.wait());
        std::fs::remove_file(&self.socket).ok();
        eprintln!("{}: stopped pids {} and {}", self.label, self.server.id(), self.mcp.child.id());
    }
}
