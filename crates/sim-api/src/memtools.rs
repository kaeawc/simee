//! `mem_leaks`, `mem_heap`, `mem_vmmap` and `mem_history`: the host's `leaks`, `heap`,
//! `vmmap --summary` and `malloc_history` on one app of a simulator. A simulator app is a host
//! process (a child of its device's `launchd_sim`), so the host's own tools read it; this runs one
//! with a deadline and parses its report. The parsers are pure and tested against reports captured
//! from an iOS 26.5 simulator app (`tests/fixtures/memtools/`).
//!
//! What the tools need: the host's binaries carry `com.apple.system-task-ports.read`, and for a
//! simulator target they hand the work to the runtime's copy (the report's `Analysis Tool`). On
//! macOS 26.6 with SIP disabled and Developer Mode on, an app ad-hoc signed without
//! `get-task-allow` and the device's SpringBoard were both read as the user, without `sudo`; a
//! process of another user (root) is refused ("cannot examine process … appropriate privileges"),
//! which [`finish`] turns into an error that says so. A SIP-enabled host is not measured: a notice
//! the tool prints while still answering comes back in `warnings`.

use sim_core::hostfs::CommandExt;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

/// How long a tool may run when `timeout_s` isn't given. `leaks` took 2.3 s on a small test app
/// and 4.9 s on a device's SpringBoard (docs/api.md), and grows with the heap.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// One of the memory tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Leaks,
    Heap,
    Vmmap,
    History,
}

impl Tool {
    /// The tool behind an API method.
    pub fn for_method(method: &str) -> Option<Tool> {
        Some(match method {
            "mem_leaks" => Tool::Leaks,
            "mem_heap" => Tool::Heap,
            "mem_vmmap" => Tool::Vmmap,
            "mem_history" => Tool::History,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Tool::Leaks => "leaks",
            Tool::Heap => "heap",
            Tool::Vmmap => "vmmap",
            Tool::History => "malloc_history",
        }
    }

    /// The host binary.
    pub fn program(self) -> String {
        format!("/usr/bin/{}", self.name())
    }

    /// The arguments for `pid` (`address` is `mem_history`'s, checked by [`address`]). `leaks`
    /// prints the classic list (one entry and its stack per leak) without content descriptions.
    pub fn args(self, pid: i32, address: Option<&str>) -> Vec<String> {
        let pid = pid.to_string();
        match self {
            Tool::Leaks => vec!["--list".into(), "--noContent".into(), pid],
            Tool::Heap => vec![pid],
            Tool::Vmmap => vec!["--summary".into(), pid],
            Tool::History => vec![pid, address.unwrap_or_default().to_owned()],
        }
    }
}

/// `mem_history`'s address: hex with a `0x` prefix, lower-cased.
pub fn address(s: &str) -> Result<String, String> {
    let digits = s.trim().strip_prefix("0x").or_else(|| s.trim().strip_prefix("0X")).unwrap_or("");
    if digits.is_empty() || digits.len() > 16 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("address must be hex with a 0x prefix (from mem_leaks or mem_heap), not `{s}`"));
    }
    Ok(format!("0x{}", digits.to_ascii_lowercase()))
}

/// What a finished tool printed.
#[derive(Debug, Default)]
pub struct Output {
    /// The exit code (`None`: killed by a signal).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

/// SIGKILLs the process group `pgid` (the host tools run on macOS only; elsewhere the caller's
/// handle kill is all there is).
fn kill_group(pgid: u32) {
    #[cfg(target_os = "macos")]
    if let Some(pgid) = i32::try_from(pgid).ok().filter(|p| *p > 1) {
        let _ = sim_coresim::procs::signal(-pgid, 9);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = pgid;
}

/// Runs `program` and waits up to `deadline` for it. The tool runs in a process group of its own
/// (its pid is the pgid), because `leaks` and friends respawn the simulator runtime's copy as a
/// child. Past the deadline that group, which this created and nothing else, is killed, the
/// child reaped, and the call fails.
pub fn run(program: &str, args: &[String], deadline: Duration) -> Result<Output, String> {
    let mut child = Command::new(program)
        .args(args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let (out, err) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < deadline => std::thread::sleep(Duration::from_millis(10)),
            waited => {
                // The child is not yet reaped, so its pid is still the pgid this created; a negative
                // pid signals the whole group (kill(2)).
                kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                // The group's death closes the pipes; the readers end.
                let _ = (out.join(), err.join());
                return Err(match waited {
                    Err(e) => format!("{program}: {e}"),
                    _ => format!("{program} didn't finish within {} s and was stopped (give a longer timeout_s)", deadline.as_secs_f64()),
                });
            }
        }
    };
    let text = |h: JoinHandle<Vec<u8>>| String::from_utf8_lossy(&h.join().unwrap_or_default()).into_owned();
    Ok(Output { code: status.code(), stdout: text(out), stderr: text(err) })
}

/// The tool's result for `pid`: its report parsed, `warnings` (what it printed to stderr while
/// answering), and with `raw` its report as printed. A refusal, a failure or a report with
/// nothing to parse is an error naming the tool and why.
pub fn finish(tool: Tool, pid: i32, out: &Output, raw: bool) -> Result<Value, String> {
    let name = tool.name();
    let said = |text: &str| text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect::<Vec<_>>();
    let mut lines = said(&out.stderr);
    lines.extend(said(&out.stdout).into_iter().filter(|l| l.contains("cannot examine") || l.contains("no stack logs")));
    if let Some(line) = lines.iter().find(|l| l.contains("cannot examine process")) {
        return Err(format!(
            "{name} was refused pid {pid}: {}. The host's memory tools read a process of your own user (a simulator app runs as you); \
             another user's process needs root",
            strip_tool_prefix(line)
        ));
    }
    if tool == Tool::History && lines.iter().any(|l| l.contains("has no stack logs")) {
        return Err(format!(
            "pid {pid} has no allocation stacks: launch the app with `malloc_stack_logging: true` and `restart: true` (the default; a running app keeps its old environment), then ask again"
        ));
    }
    // leaks exits 1 when it found leaks.
    let ok = match tool {
        Tool::Leaks => matches!(out.code, Some(0 | 1)),
        _ => out.code == Some(0),
    };
    if !ok {
        let why = lines.first().map_or_else(|| "no output".to_owned(), |l| strip_tool_prefix(l).to_owned());
        let code = out.code.map_or_else(|| "a signal".to_owned(), |c| format!("exit {c}"));
        return Err(format!("{name} failed on pid {pid} ({code}): {why}"));
    }
    let mut result = match tool {
        Tool::Leaks => parse_leaks(&out.stdout),
        Tool::Heap => parse_heap(&out.stdout),
        Tool::Vmmap => parse_vmmap(&out.stdout),
        Tool::History => Ok(parse_history(&out.stdout)),
    }
    .map_err(|e| format!("{name} on pid {pid}: {e}"))?;
    result.insert("pid".into(), pid.into());
    let warnings: Vec<String> = said(&out.stderr).iter().map(|l| strip_tool_prefix(l).to_owned()).collect();
    if !warnings.is_empty() {
        result.insert("warnings".into(), json!(warnings));
    }
    if raw {
        result.insert("raw".into(), out.stdout.clone().into());
    }
    Ok(Value::Object(result))
}

/// `leaks[6581]: leaks cannot …` → `leaks cannot …`.
fn strip_tool_prefix(line: &str) -> &str {
    match line.split_once("]: ") {
        Some((tool, rest)) if !tool.contains(' ') => rest,
        _ => line,
    }
}

/// Whether the app with `bundle_id` was started under the launchd label `label` (its
/// `XPC_SERVICE_NAME`): `UIKitApplication:<bundle id>[<n>]…`.
pub fn is_app_label(label: &str, bundle_id: &str) -> bool {
    label.strip_prefix("UIKitApplication:").and_then(|rest| rest.strip_prefix(bundle_id)).is_some_and(|rest| rest.starts_with('['))
}

/// A size as the tools print it, in bytes: `4096`, `1394K`, `15.5M`, `4.5G`, `28KB` (powers of
/// 1024).
pub fn bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let s = s.strip_suffix('B').filter(|rest| rest.ends_with(['K', 'M', 'G', 'T'])).unwrap_or(s);
    let (num, unit) = match s.char_indices().last()? {
        (i, 'K') => (&s[..i], 1u64 << 10),
        (i, 'M') => (&s[..i], 1 << 20),
        (i, 'G') => (&s[..i], 1 << 30),
        (i, 'T') => (&s[..i], 1 << 40),
        _ => (s, 1),
    };
    if num.is_empty() || !num.starts_with(|c: char| c.is_ascii_digit()) || !num.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let n: f64 = num.parse().ok()?;
    Some((n * unit as f64).round() as u64)
}

/// Splits at every run of two or more spaces (the tools' column gaps; a name has single spaces).
fn columns(s: &str) -> Vec<&str> {
    s.split("  ").map(str::trim).filter(|c| !c.is_empty()).collect()
}

/// The leading `n` whitespace-separated words of `s`, and the rest of the line after them.
fn words(s: &str, n: usize) -> Option<(Vec<&str>, &str)> {
    let mut rest = s.trim_start();
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        out.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    Some((out, rest))
}

/// The kinds of allocation `leaks` and `heap` name in their type column.
fn is_language(s: &str) -> bool {
    matches!(s, "C" | "C++" | "ObjC" | "Swift" | "CFType")
}

/// A type column group, `<name>  <language>  <binary>`, any of them missing: `heap` leaves the
/// language out of some rows, and `leaks` names nothing for a plain malloc without stacks.
fn named(parts: &[&str], out: &mut Map<String, Value>, name_key: &str) {
    let (name, language, binary) = match parts {
        [] => return,
        [name] => (name.to_string(), None, None),
        [name, other] if is_language(other) => (name.to_string(), Some(*other), None),
        [name, binary] => (name.to_string(), None, Some(*binary)),
        [names @ .., language, binary] if is_language(language) => (names.join("  "), Some(*language), Some(*binary)),
        [names @ .., binary] => (names.join("  "), None, Some(*binary)),
    };
    out.insert(name_key.into(), name.into());
    if let Some(l) = language {
        out.insert("language".into(), l.into());
    }
    if let Some(b) = binary {
        out.insert("binary".into(), b.into());
    }
}

/// The report's header (to its `----` line): the process, its bundle id and path, and its
/// physical footprint now and at its peak.
fn header(text: &str) -> Map<String, Value> {
    let mut out = Map::new();
    for line in text.lines() {
        if line == "----" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key {
            "Process" => {
                let name = value.rsplit_once(" [").map_or(value, |(name, _)| name);
                out.insert("process".into(), name.into());
            }
            "Identifier" => {
                out.insert("identifier".into(), value.into());
            }
            "Path" => {
                out.insert("path".into(), value.into());
            }
            "Physical footprint" => {
                out.insert("physical_footprint_bytes".into(), json!(bytes(value)));
            }
            "Physical footprint (peak)" => {
                out.insert("physical_footprint_peak_bytes".into(), json!(bytes(value)));
            }
            _ => {}
        }
    }
    out
}

/// A backtrace as the tools print it on one line, outermost frame first:
/// `0x1026644e4 (dyld) start | … | 0x1023c8d24 (dev.simee.leaky) leak_malloc_blocks  main.m:14`.
/// Returned innermost first (frame 0 made the allocation), each `{address, image, symbol,
/// source?}`.
pub fn frames(stack: &str) -> Vec<Value> {
    let mut out: Vec<Value> = stack
        .split(" | ")
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(|frame| {
            let (address, rest) = frame.split_once(' ').unwrap_or((frame, ""));
            let rest = rest.trim();
            let (image, symbol) = match rest.strip_prefix('(').and_then(|r| r.split_once(") ")) {
                Some((image, symbol)) => (Some(image), symbol.trim()),
                None => (None, rest),
            };
            let (symbol, source) = match symbol.rsplit_once("  ") {
                Some((s, src)) if src.contains(':') && !src.contains(' ') => (s.trim(), Some(src)),
                _ => (symbol, None),
            };
            let mut f = json!({"address": address, "symbol": symbol});
            if let Some(image) = image {
                f["image"] = image.into();
            }
            if let Some(source) = source {
                f["source"] = source.into();
            }
            f
        })
        .collect();
    out.reverse();
    out
}

/// One `Leak:` line of `leaks --list`:
/// `0x1034a3540  size=64  zone: DefaultMallocZone_0x…   NSConcreteMutableData  ObjC  Foundation`.
fn leak_line(line: &str) -> Option<(String, u64, String, Map<String, Value>)> {
    let parts = columns(line);
    let address = (*parts.first()?).to_owned();
    let size = parts.get(1)?.strip_prefix("size=")?.parse().ok()?;
    let zone = parts.get(2)?.strip_prefix("zone:").map_or("", str::trim).to_owned();
    let mut kind = Map::new();
    named(parts.get(3..).unwrap_or_default(), &mut kind, "type");
    Some((address, size, zone, kind))
}

/// One leak (or a group of them): what leaked (`type`, `language`, `binary`), its allocation
/// stack, and its instances.
type Leak = (Map<String, Value>, Vec<Value>, Vec<Value>);

/// `leaks --list --noContent`: the totals, and the leaks grouped by what leaked and where it was
/// allocated (`instances` each `{address, size, zone}`), most bytes first. `stacks` says whether
/// the process logged allocation stacks; without them every leak's `stack` is empty.
pub fn parse_leaks(text: &str) -> Result<Map<String, Value>, String> {
    let mut out = header(text);
    let mut count = None;
    let mut entries: Vec<Leak> = Vec::new();
    for line in text.lines() {
        if line.starts_with("Binary Images:") {
            break;
        }
        if let Some(tail) = line.strip_prefix("Process ").and_then(|r| r.split_once(": ")).map(|(_, t)| t) {
            match tail.split_whitespace().collect::<Vec<_>>().as_slice() {
                [n, "nodes", "malloced", "for", kb, "KB"] => {
                    out.insert("nodes".into(), json!(n.parse::<u64>().ok()));
                    out.insert("malloced_bytes".into(), json!(kb.parse::<u64>().ok().map(|k| k * 1024)));
                }
                [n, "leaks" | "leak", "for", b, "total", "leaked", "bytes."] => {
                    count = n.parse::<u64>().ok();
                    out.insert("leaked_bytes".into(), json!(b.parse::<u64>().ok()));
                }
                _ => {}
            }
        } else if let Some(rest) = line.strip_prefix("Leak: ") {
            let (address, size, zone, kind) = leak_line(rest).ok_or_else(|| format!("unreadable leak line `{line}`"))?;
            entries.push((kind, Vec::new(), vec![json!({"address": address, "size": size, "zone": zone})]));
        } else if let Some(stack) = line.trim_start().strip_prefix("Call stack: ")
            && let Some(last) = entries.last_mut()
        {
            last.1 = frames(stack);
        }
    }
    let count = count.ok_or("no leak summary in its report")?;
    let stacks = entries.iter().any(|(_, stack, _)| !stack.is_empty());
    let mut groups: Vec<Leak> = Vec::new();
    for (kind, stack, instances) in entries {
        match groups.iter_mut().find(|(k, s, _)| *k == kind && *s == stack) {
            Some(group) => group.2.extend(instances),
            None => groups.push((kind, stack, instances)),
        }
    }
    let size_of = |instances: &[Value]| instances.iter().filter_map(|i| i["size"].as_u64()).sum::<u64>();
    groups.sort_by_key(|(_, _, instances)| std::cmp::Reverse(size_of(instances)));
    let leaks: Vec<Value> = groups
        .into_iter()
        .map(|(mut kind, stack, instances)| {
            kind.insert("count".into(), instances.len().into());
            kind.insert("bytes".into(), size_of(&instances).into());
            kind.insert("instances".into(), instances.into());
            kind.insert("stack".into(), stack.into());
            Value::Object(kind)
        })
        .collect();
    out.insert("leak_count".into(), count.into());
    out.insert("stacks".into(), stacks.into());
    if count > 0 && !stacks {
        out.insert("note".into(), "no allocation stacks: launch the app with malloc_stack_logging: true to get them".into());
    }
    out.insert("leaks".into(), leaks.into());
    Ok(out)
}

/// `heap`: the zone count, the totals, and every class (or allocation site) with its count,
/// bytes and average size, in `heap`'s order (most instances first).
pub fn parse_heap(text: &str) -> Result<Map<String, Value>, String> {
    let mut out = header(text);
    let mut classes = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if in_table {
            if trimmed.is_empty() {
                in_table = false;
                continue;
            }
            let (nums, rest) = words(trimmed, 3).ok_or_else(|| format!("unreadable class row `{trimmed}`"))?;
            let (Ok(count), Ok(total), Ok(avg)) = (nums[0].parse::<u64>(), nums[1].parse::<u64>(), nums[2].parse::<f64>()) else {
                return Err(format!("unreadable class row `{trimmed}`"));
            };
            let mut row = Map::new();
            named(&columns(rest), &mut row, "class");
            row.insert("count".into(), count.into());
            row.insert("bytes".into(), total.into());
            row.insert("avg_bytes".into(), json!(avg));
            classes.push(Value::Object(row));
        } else if trimmed.starts_with("=====") && classes.is_empty() {
            in_table = true;
        } else if let Some(n) = line.strip_prefix("Process ").and_then(|r| r.split_once(": ")).and_then(|(_, t)| t.strip_suffix(" zones")) {
            out.insert("zones".into(), json!(n.trim().parse::<u64>().ok()));
        } else if let Some(rest) = line.strip_prefix("All zones: ").filter(|r| r.ends_with(" bytes)")) {
            // `13031 nodes (1870800 bytes)`
            let mut it = rest.split_whitespace();
            out.insert("nodes".into(), json!(it.next().and_then(|n| n.parse::<u64>().ok())));
            out.insert("bytes".into(), json!(it.nth(1).and_then(|b| b.trim_start_matches('(').parse::<u64>().ok())));
        }
    }
    if classes.is_empty() {
        return Err("no class table in its report".into());
    }
    out.insert("classes".into(), classes.into());
    Ok(out)
}

/// What one column of a table holds.
#[derive(Clone, Copy)]
enum Col {
    Size,
    Count,
    Pct,
}

fn cell(kind: Col, s: &str) -> Option<Value> {
    match kind {
        Col::Size => bytes(s).map(Value::from),
        Col::Count => s.parse::<u64>().ok().map(Value::from),
        Col::Pct => s.strip_suffix('%')?.parse::<f64>().ok().map(Value::from),
    }
}

/// One row of a `vmmap` table: a name (which may have spaces), then `cols`' cells, then an
/// optional note (`see MALLOC ZONE table below`).
fn table_row(line: &str, cols: &[(String, Col)], name_key: &str) -> Option<Map<String, Value>> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = (1..words.len())
        .find(|&i| words.len() >= i + cols.len() && cols.iter().zip(&words[i..]).all(|((_, kind), w)| cell(*kind, w).is_some()))?;
    let mut row = Map::new();
    row.insert(name_key.into(), words[..at].join(" ").into());
    for ((key, kind), w) in cols.iter().zip(&words[at..]) {
        row.insert(key.clone(), cell(*kind, w)?);
    }
    let note = words[at + cols.len()..].join(" ");
    if !note.is_empty() {
        row.insert("note".into(), note.into());
    }
    Some(row)
}

/// `Total=1.3G resident=1.2G(99%) swapped_out_or_unallocated=12.0M(1%)` → `{total_bytes, …}`.
fn key_sizes(s: &str) -> Map<String, Value> {
    s.split_whitespace()
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let v = v.split_once('(').map_or(v, |(size, _)| size);
            Some((format!("{}_bytes", k.to_ascii_lowercase()), json!(bytes(v))))
        })
        .collect()
}

/// The rows of the table whose header line starts with `title`, to its closing `=====` line,
/// and its `TOTAL` row after it.
fn table(lines: &[&str], title: &str, cols: &[(String, Col)], name_key: &str) -> Result<(Vec<Value>, Option<Value>), String> {
    let start = lines.iter().position(|l| l.starts_with(title)).ok_or_else(|| format!("no `{title}` table in its report"))?;
    let mut rows = Vec::new();
    let mut ruled = 0;
    for line in &lines[start + 1..] {
        if line.starts_with("=====") {
            ruled += 1;
            continue;
        }
        if ruled == 1 {
            rows.push(Value::Object(table_row(line, cols, name_key).ok_or_else(|| format!("unreadable `{title}` row `{line}`"))?));
        } else if ruled == 2 {
            return Ok((
                rows,
                table_row(line, cols, name_key).map(|mut t| {
                    t.remove(name_key);
                    Value::Object(t)
                }),
            ));
        }
    }
    Ok((rows, None))
}

/// `vmmap --summary`: the read-only and writable totals, every region type with its virtual,
/// resident, dirty, swapped, volatile, non-volatile and empty bytes and region count (the
/// columns as the header names them), and each malloc zone with its allocations and
/// fragmentation; each table with its `total`.
pub fn parse_vmmap(text: &str) -> Result<Map<String, Value>, String> {
    let mut out = header(text);
    let lines: Vec<&str> = text.lines().collect();
    for line in &lines {
        if let Some(rest) = line.strip_prefix("ReadOnly portion of Libraries:") {
            out.insert("readonly_libraries".into(), Value::Object(key_sizes(rest)));
        } else if let Some(rest) = line.strip_prefix("Writable regions:") {
            out.insert("writable_regions".into(), Value::Object(key_sizes(rest)));
        }
    }
    // The region table's columns, from the header line above `REGION TYPE`:
    // `VIRTUAL RESIDENT DIRTY SWAPPED VOLATILE NONVOL EMPTY REGION` (the last is the region count).
    let at = lines.iter().position(|l| l.starts_with("REGION TYPE")).ok_or("no `REGION TYPE` table in its report")?;
    let names: Vec<String> = lines[..at].last().map(|l| l.split_whitespace().map(str::to_ascii_lowercase).collect()).unwrap_or_default();
    if names.len() < 2 {
        return Err("no column names above its `REGION TYPE` table".into());
    }
    let last = names.len() - 1;
    let cols: Vec<(String, Col)> = names
        .iter()
        .enumerate()
        .map(|(i, n)| if i == last { (format!("{n}_count"), Col::Count) } else { (format!("{n}_bytes"), Col::Size) })
        .collect();
    let (regions, total) = table(&lines, "REGION TYPE", &cols, "region")?;
    out.insert("regions".into(), regions.into());
    out.insert("total".into(), total.unwrap_or(Value::Null));
    let zone_cols: Vec<(String, Col)> = [
        ("virtual_bytes", Col::Size),
        ("resident_bytes", Col::Size),
        ("dirty_bytes", Col::Size),
        ("swapped_bytes", Col::Size),
        ("allocation_count", Col::Count),
        ("allocated_bytes", Col::Size),
        ("frag_bytes", Col::Size),
        ("frag_pct", Col::Pct),
        ("region_count", Col::Count),
    ]
    .into_iter()
    .map(|(k, c)| (k.to_owned(), c))
    .collect();
    if lines.iter().any(|l| l.starts_with("MALLOC ZONE")) {
        let (zones, total) = table(&lines, "MALLOC ZONE", &zone_cols, "zone")?;
        out.insert("malloc_zones".into(), zones.into());
        out.insert("malloc_total".into(), total.unwrap_or(Value::Null));
    }
    Ok(out)
}

/// `malloc_history <pid> <address>`: each event the process's stack log holds for the address,
/// `{event, start, end, size, stack}` (`event` is `alloc`, or `free` under full stack logging).
/// Under `MallocStackLogging=1` (lite mode) only live allocations are logged, so an address the
/// log doesn't hold has no events.
pub fn parse_history(text: &str) -> Map<String, Value> {
    let mut out = header(text);
    let mut events = Vec::new();
    for line in text.lines() {
        // `ALLOC 0x103788000-0x1037893ff [size=5120]:  <stack>`
        let Some((head, stack)) = line.split_once("]:") else { continue };
        let Some((event_range, size)) = head.split_once(" [size=") else { continue };
        let Some((event, range)) = event_range.split_once(' ') else { continue };
        let (start, end) = range.trim().split_once('-').unwrap_or((range.trim(), ""));
        if !event.chars().all(|c| c.is_ascii_uppercase()) || !start.starts_with("0x") {
            continue;
        }
        events.push(json!({
            "event": event.to_ascii_lowercase(),
            "start": start,
            "end": end,
            "size": size.parse::<u64>().ok(),
            "stack": frames(stack),
        }));
    }
    out.insert("events".into(), events.into());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEAKS_STACKS: &str = include_str!("../tests/fixtures/memtools/leaks-stacks.txt");
    const LEAKS_PLAIN: &str = include_str!("../tests/fixtures/memtools/leaks-plain.txt");
    const HEAP: &str = include_str!("../tests/fixtures/memtools/heap.txt");
    const VMMAP: &str = include_str!("../tests/fixtures/memtools/vmmap-summary.txt");
    const HISTORY: &str = include_str!("../tests/fixtures/memtools/malloc-history.txt");

    #[test]
    fn sizes_read_as_the_tools_print_them() {
        assert_eq!(bytes("4096"), Some(4096));
        assert_eq!(bytes("0K"), Some(0));
        assert_eq!(bytes("1394K"), Some(1394 * 1024));
        assert_eq!(bytes("15.5M"), Some((15.5 * 1048576.0) as u64));
        assert_eq!(bytes("4.5G"), Some((4.5 * 1073741824.0) as u64));
        assert_eq!(bytes("28KB"), Some(28 * 1024));
        for bad in ["", "K", "inf", "1e5", "-1K", "TOTAL", "__DATA", "(empty)"] {
            assert_eq!(bytes(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_leak_report_with_stacks_groups_leaks_by_type_and_allocation_stack() {
        // leaks --list --noContent on the test app launched with MallocStackLogging=1, trimmed
        // to 7 of its 20 entries.
        let r = parse_leaks(LEAKS_STACKS).expect("parse");
        assert_eq!(r["process"], "Leaky");
        assert_eq!(r["identifier"], "dev.simee.leaky");
        assert_eq!(r["physical_footprint_bytes"], json!(bytes("15.5M")));
        assert_eq!((r["nodes"].clone(), r["malloced_bytes"].clone()), (json!(13031), json!(1827 * 1024)));
        assert_eq!((r["leak_count"].clone(), r["leaked_bytes"].clone()), (json!(20), json!(85152)));
        assert_eq!(r["stacks"], true);
        assert!(r.get("note").is_none());
        let leaks = r["leaks"].as_array().expect("leaks");
        // The three malloc leaks share a stack; the cycle's two LeakyNodes were allocated on
        // different lines, so each is its own group.
        let malloc = &leaks[0];
        assert_eq!(malloc["type"], "malloc in leak_malloc_blocks");
        assert_eq!((malloc["language"].clone(), malloc["binary"].clone()), (json!("C"), json!("Leaky")));
        assert_eq!((malloc["count"].clone(), malloc["bytes"].clone()), (json!(3), json!(3 * 5120)));
        assert_eq!(
            malloc["instances"][0],
            json!({"address": "0x103788000", "size": 5120, "zone": "MallocStackLoggingLiteZone_0x1029f8000"})
        );
        // Innermost frame first: malloc, then the app's leaking function with its source line.
        let stack = malloc["stack"].as_array().expect("stack");
        assert_eq!(stack[0]["symbol"], "_malloc_zone_malloc_instrumented_or_legacy");
        assert_eq!(stack[0]["image"], "libsystem_malloc.dylib");
        assert_eq!(
            stack[1],
            json!({"address": "0x1023c8d24", "image": "dev.simee.leaky", "symbol": "leak_malloc_blocks", "source": "main.m:14"})
        );
        assert_eq!(stack.last().expect("outermost")["symbol"], "start");
        let nodes: Vec<&Value> = leaks.iter().filter(|l| l["type"] == "LeakyNode").collect();
        assert_eq!(nodes.len(), 2);
        let lines: Vec<&Value> = nodes
            .iter()
            .map(|n| {
                assert_eq!((n["count"].clone(), n["language"].clone()), (json!(1), json!("ObjC")));
                let frame = n["stack"].as_array().expect("stack").iter().find(|f| f["symbol"] == "leak_retain_cycle").expect("app frame");
                &frame["source"]
            })
            .collect();
        assert_ne!(lines[0], lines[1]);
        let storage = leaks.iter().find(|l| l["type"] == "NSConcreteMutableData (Bytes Storage)").expect("bytes storage");
        assert_eq!((storage["language"].clone(), storage["binary"].clone()), (json!("C"), json!("Foundation")));
        assert_eq!(leaks.iter().map(|l| l["count"].as_u64().expect("count")).sum::<u64>(), 7);
    }

    #[test]
    fn a_leak_report_without_stacks_says_how_to_get_them() {
        let r = parse_leaks(LEAKS_PLAIN).expect("parse");
        assert_eq!(r["leak_count"], 20);
        assert_eq!(r["stacks"], false);
        assert!(r["note"].as_str().expect("note").contains("malloc_stack_logging"));
        let leaks = r["leaks"].as_array().expect("leaks");
        // The 16 plain mallocs have no type and no stack: one group.
        let plain = &leaks[0];
        assert!(plain.get("type").is_none());
        assert_eq!((plain["count"].clone(), plain["bytes"].clone()), (json!(16), json!(16 * 4096)));
        assert_eq!(leaks.iter().map(|l| l["count"].as_u64().expect("count")).sum::<u64>(), 20);
    }

    #[test]
    fn a_report_without_a_summary_is_an_error() {
        assert!(parse_leaks("Process:  Leaky [1]\n----\n").is_err());
        assert!(parse_heap("Process:  Leaky [1]\n----\n").is_err());
        assert!(parse_vmmap("Process:  Leaky [1]\n----\n").is_err());
        assert!(parse_leaks("Process 1: 0 leaks for 0 total leaked bytes.\n").expect("no leaks")["leaks"] == json!([]));
    }

    #[test]
    fn heap_lists_every_class_with_counts_and_bytes() {
        let r = parse_heap(HEAP).expect("parse");
        assert_eq!((r["zones"].clone(), r["nodes"].clone(), r["bytes"].clone()), (json!(4), json!(13031), json!(1870800)));
        let classes = r["classes"].as_array().expect("classes");
        assert_eq!(
            classes[0],
            json!({"class": "Class.data (class_rw_t)", "language": "C", "binary": "libobjc.A.dylib", "count": 2276, "bytes": 105440, "avg_bytes": 46.3})
        );
        assert_eq!(classes[1]["class"], "CFString");
        let find = |name: &str| classes.iter().find(|c| c["class"] == name).unwrap_or_else(|| panic!("{name}"));
        // No language column: a zone's non-objects, and a Swift lock.
        assert_eq!(find("non-object in zone QuartzCore_0x103264000")["binary"], "<unknown>");
        assert!(find("non-object in zone QuartzCore_0x103264000").get("language").is_none());
        assert_eq!(find("_UISwiftTraits.UnfairLock._lock (malloc)")["binary"], "UIKitCore");
        // A C++ signature with single spaces in it.
        let cxx = find("malloc_with_options_outlined in cache_t::insert(objc_selector*, void (*)(), objc_object*)");
        assert_eq!((cxx["language"].clone(), cxx["count"].clone()), (json!("C++"), json!(277)));
        assert!(classes.iter().all(|c| c["count"].as_u64().is_some() && c["bytes"].as_u64().is_some()));
    }

    #[test]
    fn vmmap_summary_parses_regions_zones_and_totals() {
        let r = parse_vmmap(VMMAP).expect("parse");
        assert_eq!(r["physical_footprint_peak_bytes"], json!(bytes("16.0M")));
        assert_eq!(r["writable_regions"]["total_bytes"], json!(bytes("31.6M")));
        assert_eq!(r["writable_regions"]["written_bytes"], json!(1394 * 1024));
        assert_eq!(r["readonly_libraries"]["swapped_out_or_unallocated_bytes"], json!(bytes("12.0M")));
        let regions = r["regions"].as_array().expect("regions");
        assert_eq!(regions.len(), 27);
        let find = |name: &str| regions.iter().find(|x| x["region"] == name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(
            find("MALLOC_SMALL"),
            &json!({"region": "MALLOC_SMALL", "virtual_bytes": 4416 * 1024, "resident_bytes": 4032 * 1024, "dirty_bytes": 2368 * 1024,
                "swapped_bytes": 0, "volatile_bytes": 0, "nonvol_bytes": 0, "empty_bytes": 0, "region_count": 26,
                "note": "see MALLOC ZONE table below"})
        );
        assert_eq!(find("unused but dirty shlib __DATA")["region_count"], 357);
        assert_eq!(find("__FONT_DATA")["virtual_bytes"], 2352);
        assert_eq!(find("MALLOC_SMALL (empty)")["region_count"], 15);
        assert_eq!(r["total"]["dirty_bytes"], json!(bytes("15.3M")));
        assert_eq!(r["total"]["region_count"], 1847);
        let zones = r["malloc_zones"].as_array().expect("zones");
        assert_eq!(zones.len(), 4);
        assert_eq!(zones[0]["zone"], "DefaultMallocZone_0x102934000");
        assert_eq!((zones[0]["allocation_count"].clone(), zones[0]["frag_pct"].clone()), (json!(285), json!(96.0)));
        assert_eq!(r["malloc_total"]["allocation_count"], 13031);
        assert_eq!(r["malloc_total"]["allocated_bytes"], 1827 * 1024);
    }

    #[test]
    fn malloc_history_gives_each_event_with_its_stack() {
        let r = parse_history(HISTORY);
        let events = r["events"].as_array().expect("events");
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(
            (e["event"].clone(), e["start"].clone(), e["end"].clone(), e["size"].clone()),
            (json!("alloc"), json!("0x103788000"), json!("0x1037893ff"), json!(5120))
        );
        assert_eq!(e["stack"][1]["symbol"], "leak_malloc_blocks");
        assert!(parse_history("malloc_history Report Version:  2.0\n")["events"] == json!([]));
    }

    #[test]
    fn addresses_are_hex_with_a_prefix() {
        assert_eq!(address("0x103788000").as_deref(), Ok("0x103788000"));
        assert_eq!(address(" 0X1037A8000 ").as_deref(), Ok("0x1037a8000"));
        for bad in ["103788000", "0x", "0xzz", "-1", "0x12345678901234567", "0x1 0x2"] {
            assert!(address(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_app_is_found_by_its_launchd_label() {
        assert!(is_app_label("UIKitApplication:dev.simee.leaky[21cb][rb-legacy]", "dev.simee.leaky"));
        assert!(!is_app_label("UIKitApplication:dev.simee.leaky.extra[21cb]", "dev.simee.leaky"));
        assert!(!is_app_label("com.apple.SpringBoard", "com.apple.SpringBoard"));
        assert!(!is_app_label("UIKitApplication:dev.simee.leaky", "dev.simee.leaky"));
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output { code: Some(code), stdout: stdout.into(), stderr: stderr.into() }
    }

    #[test]
    fn refusals_and_failures_are_errors_that_say_why() {
        // Captured from `leaks 1` and `malloc_history 1 0x10` as a user (pid 1 is root's launchd).
        let refused = "leaks[6581]: leaks cannot examine process 1 (launchd) because you do not have appropriate privileges to examine it; \
                       try running with `sudo`.\nleaks[6581]: [fatal] mach port for process 0 not valid\n";
        let e = finish(Tool::Leaks, 1, &output(255, "", refused), false).expect_err("refused");
        assert!(e.starts_with("leaks was refused pid 1: leaks cannot examine process 1 (launchd)"), "{e}");
        assert!(e.contains("another user's process needs root"), "{e}");
        // Captured from malloc_history on the app launched without MallocStackLogging (exit 249).
        let none =
            "The target process has no stack logs.  Run it with the MallocStackLogging environment variable set to get stack logs.\n";
        let e = finish(Tool::History, 7, &output(249, "", none), false).expect_err("no logs");
        assert!(e.contains("malloc_stack_logging: true"), "{e}");
        let e = finish(Tool::Heap, 7, &output(3, "", "heap[9]: something broke\n"), false).expect_err("failed");
        assert_eq!(e, "heap failed on pid 7 (exit 3): something broke");
        let e = finish(Tool::Vmmap, 7, &Output { code: None, ..Output::default() }, false).expect_err("signalled");
        assert_eq!(e, "vmmap failed on pid 7 (a signal): no output");
    }

    #[test]
    fn a_finished_tool_answers_its_parse_with_pid_warnings_and_raw_on_request() {
        // leaks exits 1 when it found leaks: that's an answer, not a failure.
        let r = finish(Tool::Leaks, 2572, &output(1, LEAKS_PLAIN, ""), false).expect("leaks");
        assert_eq!((r["pid"].clone(), r["leak_count"].clone()), (json!(2572), json!(20)));
        assert!(r.get("raw").is_none() && r.get("warnings").is_none());
        let r = finish(Tool::History, 2572, &output(0, "malloc_history Report Version:  2.0\n", "no stack logs found for 0x10\n"), true)
            .expect("history");
        assert_eq!(r["events"], json!([]));
        assert_eq!(r["warnings"], json!(["no stack logs found for 0x10"]));
        assert_eq!(r["raw"], "malloc_history Report Version:  2.0\n");
        assert!(finish(Tool::Vmmap, 1, &output(0, VMMAP, ""), false).expect("vmmap")["regions"].is_array());
        assert!(finish(Tool::Heap, 1, &output(0, HEAP, ""), false).expect("heap")["classes"].is_array());
    }

    #[test]
    fn the_runner_returns_output_and_kills_its_child_at_the_deadline() {
        let out = run("/bin/sh", &["-c".into(), "echo out; echo err >&2; exit 3".into()], Duration::from_secs(10)).expect("run");
        assert_eq!((out.code, out.stdout.as_str(), out.stderr.as_str()), (Some(3), "out\n", "err\n"));
        let start = Instant::now();
        let e = run("/bin/sleep", &["30".into()], Duration::from_millis(200)).expect_err("deadline");
        assert!(e.contains("didn't finish within 0.2 s"), "{e}");
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
        assert!(run("/nonexistent/tool", &[], Duration::from_secs(1)).is_err());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn the_deadline_kills_the_tools_children_too() {
        let dir = std::env::temp_dir().join(format!("memtools-grandchild-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let pidfile = dir.join("pid");
        let script = format!("/bin/sleep 30 & echo $! > {}; wait", pidfile.display());
        let start = Instant::now();
        let e = run("/bin/sh", &["-c".into(), script], Duration::from_millis(500)).expect_err("deadline");
        assert!(e.contains("didn't finish"), "{e}");
        assert!(start.elapsed() < Duration::from_secs(5), "returned only when the grandchild ended: {:?}", start.elapsed());
        let pid: i32 = std::fs::read_to_string(&pidfile).expect("pid").trim().parse().expect("number");
        std::thread::sleep(Duration::from_millis(200));
        assert!(sim_coresim::procs::signal(pid, 0).is_err(), "grandchild {pid} survived");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn each_method_runs_its_tool_on_the_pid() {
        assert_eq!(Tool::for_method("mem_leaks"), Some(Tool::Leaks));
        assert_eq!(Tool::for_method("mem_history"), Some(Tool::History));
        assert_eq!(Tool::for_method("processes"), None);
        assert_eq!(Tool::Leaks.program(), "/usr/bin/leaks");
        assert_eq!(Tool::Leaks.args(42, None), ["--list", "--noContent", "42"]);
        assert_eq!(Tool::Vmmap.args(42, None), ["--summary", "42"]);
        assert_eq!(Tool::Heap.args(42, None), ["42"]);
        assert_eq!(Tool::History.args(42, Some("0x10")), ["42", "0x10"]);
        assert_eq!(Tool::History.program(), "/usr/bin/malloc_history");
    }
}
