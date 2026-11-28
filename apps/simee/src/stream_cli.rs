//! `simee stream ...`: start, inspect, keep alive and stop device streams (#266).
//!
//! A thin front over the `stream_start`, `stream_status`, `stream_stop` and `streams` methods. A
//! stream lives in the process that started it and ends (sending its WHIP DELETE) only if that
//! process stops it, so every command goes to a running `simee serve` of this build and none runs
//! in-process: a stream started here would die with this command, without its DELETE.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde_json::{Value, json};

use super::{items, number, text};

/// What the stream commands call: the method table of a running server.
pub trait Api {
    fn call(&mut self, method: &str, params: Value) -> Result<Value>;
}

/// `simee stream`'s commands.
#[derive(Subcommand, Debug)]
pub enum StreamCmd {
    /// Publish a device's screen over WHIP. Needs a running `simee serve`: the stream lives in
    /// the server and ends when its lease (renewed by `stream status` or `stream watch`) lapses.
    Start {
        device: String,
        /// The WHIP endpoint to publish to (e.g. http://127.0.0.1:8889/device-1/whip).
        #[arg(long)]
        whip: String,
        /// A file holding the WHIP bearer token (the whole file, trimmed). Preferred: the token
        /// is neither in the process arguments nor printed.
        #[arg(long, conflicts_with = "token")]
        token_file: Option<PathBuf>,
        /// The WHIP bearer token itself (also `SIMEE_STREAM_TOKEN`); visible in the process
        /// list, so prefer `--token-file` or the environment variable.
        #[arg(long, env = "SIMEE_STREAM_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Frames per second (default 30).
        #[arg(long)]
        fps: Option<u32>,
        /// The longest side of the encoded picture in pixels (default 1280; 0 is full size).
        #[arg(long)]
        max_size: Option<u32>,
        /// Target bitrate in bits per second (default 2500000).
        #[arg(long)]
        bitrate: Option<u32>,
        /// Let viewers drive the device through the stream's data channel.
        #[arg(long)]
        input: bool,
        #[arg(long)]
        json: bool,
    },
    /// A stream's state, stage timeline and stats. Renews its lease.
    Status {
        stream_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Stop a stream: its encode loop ends and the WHIP resource is deleted.
    Stop {
        stream_id: String,
        #[arg(long)]
        json: bool,
    },
    /// The server's streams (of one device, or all), without renewing any lease.
    List {
        device: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Keep a stream alive from this terminal: renew its lease every third of the lease, and stop
    /// the stream when you press Enter (Ctrl-C ends this command, and the server stops the stream
    /// when its lease lapses). Exits when the stream ends on its own.
    Watch { stream_id: String },
}

/// Runs one stream command. `wait` sleeps up to a duration and says whether `watch` should stop
/// (the user asked) before it passed.
pub fn run(command: StreamCmd, api: &mut dyn Api, wait: &mut dyn FnMut(Duration) -> bool, out: &mut dyn Write) -> Result<()> {
    let print_json = |out: &mut dyn Write, v: &Value| -> Result<()> { Ok(writeln!(out, "{}", serde_json::to_string_pretty(v)?)?) };
    match command {
        StreamCmd::Start { device, whip, token_file, token, fps, max_size, bitrate, input, json } => {
            let token = match token_file {
                Some(path) => Some(read_token(&path)?),
                None => token.filter(|t| !t.is_empty()),
            };
            let params = start_params(&device, &whip, token.as_deref(), fps, max_size, bitrate, input);
            let r = api.call("stream_start", params)?;
            if json {
                return print_json(out, &r);
            }
            writeln!(out, "stream_id: {}", text(&r["stream_id"]))?;
            writeln!(out, "whip_resource: {}", text(&r["whip_resource"]))?;
            let ttl = number(&r["lease_ttl_s"]);
            writeln!(out, "lease: {ttl}s, renewed by `simee stream status|watch {}`", text(&r["stream_id"]))?;
        }
        StreamCmd::Status { stream_id, json } => {
            let r = api.call("stream_status", json!({"stream_id": stream_id}))?;
            if json {
                return print_json(out, &r);
            }
            write!(out, "{}", status_text(&r))?;
        }
        StreamCmd::Stop { stream_id, json } => {
            let r = api.call("stream_stop", json!({"stream_id": stream_id}))?;
            if json {
                return print_json(out, &r);
            }
            write!(out, "{}", stopped_text(&r))?;
        }
        StreamCmd::List { device, json } => {
            let r = api.call("streams", json!({"device": device}))?;
            if json {
                return print_json(out, &r);
            }
            if items(&r).is_empty() {
                writeln!(out, "no streams")?;
            }
            for s in items(&r) {
                writeln!(out, "{}", summary(s))?;
            }
        }
        StreamCmd::Watch { stream_id } => watch(&stream_id, api, wait, out)?,
    }
    Ok(())
}

/// `stream_start`'s parameters; settings left unset keep the server's defaults.
fn start_params(
    device: &str,
    whip: &str,
    token: Option<&str>,
    fps: Option<u32>,
    max_size: Option<u32>,
    bitrate: Option<u32>,
    input: bool,
) -> Value {
    let mut p = json!({"device": device, "whip_url": whip});
    for (key, value) in [("fps", fps), ("max_size", max_size), ("bitrate", bitrate)] {
        if let Some(value) = value {
            p[key] = json!(value);
        }
    }
    if let Some(token) = token {
        p["token"] = json!(token);
    }
    if input {
        p["input"] = json!(true);
    }
    p
}

/// The token in `path`, trimmed; an empty file is an error. The path (never the token) appears in
/// errors.
fn read_token(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("--token-file {}: can't read it", path.display()))?;
    let token = raw.trim();
    anyhow::ensure!(!token.is_empty(), "--token-file {} is empty", path.display());
    Ok(token.to_owned())
}

/// Renews the stream's lease (by asking for its status) every third of the lease until `wait`
/// says to stop, then stops it. A stream that ended on its own is stopped too (which forgets it)
/// and reported as an error, so a script sees the stream is gone.
fn watch(id: &str, api: &mut dyn Api, wait: &mut dyn FnMut(Duration) -> bool, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "watching {id}: press Enter to stop it (Ctrl-C leaves it to lapse with its lease)")?;
    loop {
        let status = api.call("stream_status", json!({"stream_id": id}))?;
        writeln!(out, "{}", summary(&status))?;
        out.flush()?;
        if status["state"] == "ended" {
            let _ = api.call("stream_stop", json!({"stream_id": id}));
            anyhow::bail!("stream {id} ended: {}", status["reason"].as_str().unwrap_or("no reason given"));
        }
        if wait(renew_every(&status)) {
            let r = api.call("stream_stop", json!({"stream_id": id}))?;
            write!(out, "{}", stopped_text(&r))?;
            return Ok(());
        }
    }
}

/// The `wait` of a terminal: sleeps up to the duration and says whether a line (Enter) came on
/// stdin. A closed stdin (a background job) never asks to stop: the stream stays alive.
pub fn enter_to_stop() -> impl FnMut(Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new().name("simee-stream-watch-stdin".into()).spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            let _ = tx.send(());
        }
    });
    move |d| match rx.recv_timeout(d) {
        Ok(()) => true,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            std::thread::sleep(d);
            false
        }
    }
}

/// A third of the lease, at least a second, so a renewal lands well before it lapses.
fn renew_every(status: &Value) -> Duration {
    let ttl = status["lease_ttl_s"].as_f64().filter(|t| *t > 0.0).unwrap_or(30.0);
    Duration::from_secs_f64((ttl / 3.0).max(1.0))
}

/// One line: `ID state device (name, platform) -> whip_url`.
fn summary(s: &Value) -> String {
    format!(
        "{} {} {} ({}, {}) -> {}",
        text(&s["stream_id"]),
        text(&s["state"]),
        text(&s["device"]),
        text(&s["name"]),
        text(&s["platform"]),
        text(&s["whip_url"])
    )
}

/// A stream's status as lines: the summary, the lease, the stage timeline and the stats.
fn status_text(s: &Value) -> String {
    let mut out = format!("{}\n", summary(s));
    if let Some(r) = s["whip_resource"].as_str() {
        out.push_str(&format!("whip_resource: {r}\n"));
    }
    if let Some(reason) = s["reason"].as_str() {
        out.push_str(&format!("reason: {reason}\n"));
    }
    out.push_str(&format!("lease: {}s left of {}s\n", number(&s["lease_expires_in_s"]), number(&s["lease_ttl_s"])));
    let stages: Vec<String> = items(&s["stages"]).iter().map(|st| format!("{} {} ms", text(&st["stage"]), number(&st["ms"]))).collect();
    out.push_str(&format!("stages: {}\n", stages.join(", ")));
    let st = &s["stats"];
    let rate = |v: &Value, unit: &str| v.as_f64().map_or_else(|| format!("- {unit}"), |n| format!("{n} {unit}"));
    out.push_str(&format!(
        "stats: {}, {}, encoded {}, sent {}, keyframes {}, pli {}, fir {}\n",
        rate(&st["fps"], "fps"),
        rate(&st["bitrate_bps"], "bit/s"),
        number(&st["encoded"]),
        number(&st["sent"]),
        number(&st["keyframes"]),
        number(&st["pli"]),
        number(&st["fir"])
    ));
    if s["input"] == true {
        out.push_str(&format!("input: {} events, {} errors\n", number(&st["input_events"]), number(&st["input_errors"])));
    }
    out
}

/// What `stream_stop` returns: the stream, and whether its WHIP resource was deleted.
fn stopped_text(r: &Value) -> String {
    let mut out = format!("stopped {}\n", text(&r["stream_id"]));
    match (r["deleted"].as_bool(), r["delete_error"].as_str()) {
        (_, Some(e)) => out.push_str(&format!("WHIP DELETE failed: {e}\n")),
        (Some(true), _) => out.push_str("WHIP resource deleted\n"),
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Harness {
        #[command(subcommand)]
        command: StreamCmd,
    }

    fn parse(args: &[&str]) -> Result<StreamCmd, clap::Error> {
        Harness::try_parse_from(std::iter::once("stream").chain(args.iter().copied())).map(|h| h.command)
    }

    /// Records calls and answers each from a script (a status per `stream_status`).
    #[derive(Default)]
    struct Fake {
        calls: Vec<(String, Value)>,
        statuses: VecDeque<Value>,
    }

    impl Api for Fake {
        fn call(&mut self, method: &str, params: Value) -> Result<Value> {
            self.calls.push((method.to_owned(), params));
            match method {
                "stream_status" => Ok(self.statuses.pop_front().unwrap_or_else(|| running(30))),
                "stream_stop" => Ok(json!({"stream_id": "s-1", "deleted": true})),
                "stream_start" => Ok(json!({"stream_id": "s-1", "whip_resource": "http://h/r", "lease_ttl_s": 30, "device": "d"})),
                _ => Ok(json!([])),
            }
        }
    }

    fn running(ttl: u64) -> Value {
        json!({"stream_id": "s-1", "state": "running", "device": "d", "name": "iPhone", "platform": "ios", "whip_url": "http://h/w",
               "lease_ttl_s": ttl, "lease_expires_in_s": 28.5, "stages": [{"stage": "requested", "ms": 0}, {"stage": "whip_connected", "ms": 340}],
               "stats": {"fps": 30.0, "bitrate_bps": null, "encoded": 10, "sent": 9, "keyframes": 1, "pli": 0, "fir": 0}})
    }

    fn run_with(command: StreamCmd, api: &mut Fake, stops: &mut Vec<bool>) -> (Result<()>, String, Vec<Duration>) {
        let mut out = Vec::new();
        let mut waits = Vec::new();
        let result = run(
            command,
            api,
            &mut |d| {
                waits.push(d);
                if stops.is_empty() { true } else { stops.remove(0) }
            },
            &mut out,
        );
        (result, String::from_utf8(out).expect("utf8"), waits)
    }

    #[test]
    fn start_parses_every_option() {
        let c = parse(&[
            "start",
            "booted",
            "--whip",
            "http://127.0.0.1:8889/d/whip",
            "--fps",
            "15",
            "--max-size",
            "0",
            "--bitrate",
            "1000000",
            "--input",
        ])
        .expect("parses");
        let StreamCmd::Start { device, whip, fps, max_size, bitrate, input, token_file, .. } = c else { panic!("not start") };
        assert_eq!((device.as_str(), whip.as_str()), ("booted", "http://127.0.0.1:8889/d/whip"));
        assert_eq!((fps, max_size, bitrate, input, token_file), (Some(15), Some(0), Some(1_000_000), true, None));
        assert!(parse(&["start", "booted"]).is_err(), "--whip is required");
        assert!(parse(&["start", "d", "--whip", "u", "--token", "a", "--token-file", "f"]).is_err(), "one token source");
        assert!(matches!(parse(&["watch", "s-1"]), Ok(StreamCmd::Watch { .. })));
        assert!(matches!(parse(&["list"]), Ok(StreamCmd::List { device: None, .. })));
        assert!(matches!(parse(&["status", "s-1", "--json"]), Ok(StreamCmd::Status { json: true, .. })));
    }

    #[test]
    fn start_sends_only_what_was_set_and_reads_the_token_from_its_file() {
        let dir = std::env::temp_dir().join(format!("simee-stream-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("token");
        std::fs::write(&file, "  s3cret-token\n").expect("write");
        let c =
            parse(&["start", "d", "--whip", "http://h/w", "--token-file", file.to_str().expect("utf8"), "--fps", "10"]).expect("parses");
        let mut api = Fake::default();
        let (r, out, _) = run_with(c, &mut api, &mut vec![]);
        r.expect("start");
        assert_eq!(api.calls[0].0, "stream_start");
        assert_eq!(api.calls[0].1, json!({"device": "d", "whip_url": "http://h/w", "token": "s3cret-token", "fps": 10}));
        assert!(out.contains("stream_id: s-1") && out.contains("whip_resource: http://h/r") && out.contains("lease: 30s"), "{out}");
        assert!(!out.contains("s3cret"), "the token is never printed: {out}");
        // An unreadable or empty token file refuses before any call, naming the path only.
        std::fs::write(&file, " \n").expect("write");
        let c = parse(&["start", "d", "--whip", "u", "--token-file", file.to_str().expect("utf8")]).expect("parses");
        let mut api = Fake::default();
        let (r, ..) = run_with(c, &mut api, &mut vec![]);
        assert!(r.expect_err("empty").to_string().contains("is empty") && api.calls.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn status_prints_the_timeline_and_stats() {
        let mut api = Fake::default();
        let (r, out, _) = run_with(parse(&["status", "s-1"]).expect("parses"), &mut api, &mut vec![]);
        r.expect("status");
        assert_eq!(api.calls, vec![("stream_status".to_owned(), json!({"stream_id": "s-1"}))]);
        assert!(out.contains("s-1 running d (iPhone, ios) -> http://h/w"), "{out}");
        assert!(out.contains("lease: 28.5s left of 30s") && out.contains("stages: requested 0 ms, whip_connected 340 ms"), "{out}");
        assert!(out.contains("stats: 30 fps, - bit/s, encoded 10, sent 9, keyframes 1, pli 0, fir 0"), "{out}");
    }

    #[test]
    fn watch_renews_every_third_of_the_lease_then_stops_the_stream() {
        let mut api = Fake { statuses: VecDeque::from([running(30), running(30), running(30)]), ..Fake::default() };
        let (r, out, waits) = run_with(StreamCmd::Watch { stream_id: "s-1".into() }, &mut api, &mut vec![false, false, true]);
        r.expect("watch");
        let methods: Vec<&str> = api.calls.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(methods, ["stream_status", "stream_status", "stream_status", "stream_stop"]);
        assert_eq!(waits, vec![Duration::from_secs(10); 3]);
        assert!(out.contains("stopped s-1") && out.contains("WHIP resource deleted"), "{out}");
    }

    #[test]
    fn watch_follows_the_servers_lease_and_ends_with_a_stream_that_ended() {
        let mut ended = running(6);
        ended["state"] = json!("ended");
        ended["reason"] = json!("the peer disconnected");
        let mut api = Fake { statuses: VecDeque::from([running(6), ended]), ..Fake::default() };
        let (r, _, waits) = run_with(StreamCmd::Watch { stream_id: "s-1".into() }, &mut api, &mut vec![false]);
        assert_eq!(waits, vec![Duration::from_secs(2)]);
        let e = r.expect_err("ended").to_string();
        assert!(e.contains("the peer disconnected"), "{e}");
        assert_eq!(api.calls.last().map(|(m, _)| m.as_str()), Some("stream_stop"), "an ended stream is forgotten");
    }

    #[test]
    fn watch_gives_up_when_the_stream_is_gone() {
        struct Gone;
        impl Api for Gone {
            fn call(&mut self, _: &str, _: Value) -> Result<Value> {
                anyhow::bail!("no stream `s-1`")
            }
        }
        let mut out = Vec::new();
        let r = run(StreamCmd::Watch { stream_id: "s-1".into() }, &mut Gone, &mut |_| false, &mut out);
        assert!(r.expect_err("gone").to_string().contains("no stream"));
    }
}
