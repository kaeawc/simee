//! The Android emulator's network conditions as its console and command line set them:
//! `network speed <speed>` / `-netspeed`, `network delay <latency>` / `-netdelay`, and the
//! `network status` report. Names, values, parsing and the report follow stock
//! (external/qemu `android/emu/cmdline/{include,src}/android/network/constants.{h,c}` and
//! `do_network_*` in `android/android-emu/android/console.cpp`, emu-master-dev). The VMM's
//! network engine applies them; this module only says what was asked for.
//!
//! Packet loss is simee's own: stock's console has no command for it. `network loss
//! <percent>|<up>:<down>|none` sets it, and `network status` adds a line for each direction
//! while any is set (with none, the report is stock's byte for byte).

/// A `network speed` preset: its name, stock's display name, and upload/download in bits/s.
/// `0` means no limit.
pub struct SpeedPreset {
    pub name: &'static str,
    pub display_name: &'static str,
    pub upload_bps: f64,
    pub download_bps: f64,
}

/// A `network delay` preset: its name, stock's display name, and min/max latency in ms.
pub struct LatencyPreset {
    pub name: &'static str,
    pub display_name: &'static str,
    pub min_ms: f64,
    pub max_ms: f64,
}

/// Stock's `ANDROID_NETWORK_LIST_MODES`: name, display name, upload and download in kbit/s,
/// min and max latency in ms. Both tables are built from it, as stock's are.
const MODES: &[(&str, &str, f64, f64, f64, f64)] = &[
    ("gsm", "GSM/CD", 14.4, 14.4, 150.0, 550.0),
    ("hscsd", "HSCSD", 14.4, 57.6, 80.0, 400.0),
    ("gprs", "GPRS", 28.8, 57.6, 35.0, 200.0),
    ("umts", "UMTS/3G", 384.0, 384.0, 35.0, 200.0),
    ("edge", "EDGE/EGPRS", 473.6, 473.6, 80.0, 400.0),
    ("hsdpa", "HSDPA", 5760.0, 13980.0, 0.0, 0.0),
    ("lte", "LTE", 58000.0, 173000.0, 0.0, 0.0),
    ("evdo", "EVDO", 75000.0, 280000.0, 0.0, 0.0),
];

/// Every `network speed` name, in stock's order (the modes, then `5g` and `full`: no limit).
pub fn speeds() -> impl Iterator<Item = SpeedPreset> {
    MODES
        .iter()
        .map(|&(name, display_name, up, down, _, _)| SpeedPreset {
            name,
            display_name,
            upload_bps: up * 1000.0,
            download_bps: down * 1000.0,
        })
        .chain([("5g", "no limit"), ("full", "no limit")].map(|(name, display_name)| SpeedPreset {
            name,
            display_name,
            upload_bps: 0.0,
            download_bps: 0.0,
        }))
}

/// Every `network delay` name, in stock's order (the modes, then `5g` and `none`: no latency).
pub fn latencies() -> impl Iterator<Item = LatencyPreset> {
    MODES.iter().map(|&(name, display_name, _, _, min_ms, max_ms)| LatencyPreset { name, display_name, min_ms, max_ms }).chain(
        [("5g", "no latency"), ("none", "no latency")].map(|(name, display_name)| LatencyPreset {
            name,
            display_name,
            min_ms: 0.0,
            max_ms: 0.0,
        }),
    )
}

/// The longest prefix of `s` that C's `strtod` reads as a decimal number, and the rest.
/// Leading whitespace is skipped, as `strtod` does; hex and `inf`/`nan` forms aren't read.
fn strtod(s: &str) -> Option<(f64, &str)> {
    let t = s.trim_start();
    let b = t.as_bytes();
    let digits = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        i
    };
    let start = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let int_end = digits(start);
    let mut end = int_end;
    if b.get(end) == Some(&b'.') {
        end = digits(end + 1);
    }
    // A mantissa needs a digit: "." and "-" alone aren't numbers.
    if !t[start..end].bytes().any(|c| c.is_ascii_digit()) {
        return None;
    }
    if matches!(b.get(end), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(b.get(end + 1), Some(b'+' | b'-')));
        let exp_end = digits(end + 1 + sign);
        if exp_end > end + 1 + sign {
            end = exp_end;
        }
    }
    t[..end].parse().ok().map(|v| (v, &t[end..]))
}

/// `<a>` or `<a>:<b>` the way stock reads it: a name lookup failed, so a number (scaled by
/// `scale`) sets both values, and a number after `:` sets the second. Text after the numbers
/// is ignored, as `strtod` leaves it.
fn parse_pair(arg: &str, scale: f64) -> Option<(f64, f64)> {
    let (a, rest) = strtod(arg)?;
    let b = rest.strip_prefix(':').and_then(strtod).map_or(a, |(b, _)| b);
    Some((a * scale, b * scale))
}

/// What the guest's network is shaped to: bits/s each way (`0` = unlimited), the latency
/// range in ms, and the percent of packets lost each way (simee's `network loss`). The default
/// is stock's: `full`, `none` and no loss.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Conditions {
    pub upload_bps: f64,
    pub download_bps: f64,
    pub min_latency_ms: i32,
    pub max_latency_ms: i32,
    /// Percent (0 to 100) of the guest's packets lost on the way out and on the way in.
    pub upload_loss_pct: f64,
    pub download_loss_pct: f64,
}

/// One `network loss` percentage: a number from 0 to 100, with or without a `%`.
fn loss_pct(text: &str) -> Result<f64, String> {
    let text = text.trim();
    let t = text.strip_suffix('%').unwrap_or(text).trim_end();
    match t.parse::<f64>() {
        // `+ 0.0` makes -0 read as 0.
        Ok(v) if (0.0..=100.0).contains(&v) => Ok(v + 0.0),
        Ok(v) if v.is_finite() => Err(format!("`{text}` is out of range, each loss is 0 to 100 percent")),
        _ => Err(format!("`{text}` isn't a percentage")),
    }
}

impl Conditions {
    /// `network speed <speed>` / `-netspeed <speed>`: a preset name, `<num>` (kbit/s both
    /// ways) or `<up>:<down>` (kbit/s). Empty means `full`. `false` (nothing changed) when
    /// the argument is neither.
    pub fn set_speed(&mut self, speed: &str) -> bool {
        let speed = if speed.is_empty() { "full" } else { speed };
        let parsed = match speeds().find(|p| p.name == speed) {
            Some(p) => Some((p.upload_bps, p.download_bps)),
            None => parse_pair(speed, 1000.0),
        };
        let Some((up, down)) = parsed else { return false };
        (self.upload_bps, self.download_bps) = (up, down);
        true
    }

    /// `network delay <latency>` / `-netdelay <latency>`: a preset name, `<ms>` or
    /// `<min>:<max>` in ms. Empty means `none`. `false` (nothing changed) when the argument is
    /// neither. Stock stores the latencies as `int`s, truncating fractions and keeping a negative one.
    pub fn set_latency(&mut self, latency: &str) -> bool {
        let latency = if latency.is_empty() { "none" } else { latency };
        let parsed = match latencies().find(|p| p.name == latency) {
            Some(p) => Some((p.min_ms, p.max_ms)),
            None => parse_pair(latency, 1.0),
        };
        let Some((min, max)) = parsed else { return false };
        (self.min_latency_ms, self.max_latency_ms) = (min as i32, max as i32);
        true
    }

    /// simee's `network loss <loss>`: `<percent>` both ways or `<up>:<down>`, each 0 to 100
    /// with an optional `%`; `none` (or nothing) for no loss. Nothing changes on an error, which
    /// names the part it couldn't read.
    pub fn set_loss(&mut self, loss: &str) -> Result<(), String> {
        let loss = loss.trim();
        let (up, down) = match loss {
            "" | "none" => (0.0, 0.0),
            _ => match loss.split_once(':') {
                Some((up, down)) => (loss_pct(up)?, loss_pct(down)?),
                None => {
                    let both = loss_pct(loss)?;
                    (both, both)
                }
            },
        };
        (self.upload_loss_pct, self.download_loss_pct) = (up, down);
        Ok(())
    }

    /// Whether any packets are lost.
    pub fn is_lossy(&self) -> bool {
        self.upload_loss_pct > 0.0 || self.download_loss_pct > 0.0
    }

    /// Whether traffic is shaped at all.
    pub fn is_shaped(&self) -> bool {
        *self != Conditions::default()
    }

    /// `network status`'s report, stock's lines without the final `OK`, CRLF-separated; while
    /// packets are lost, then a line for each direction's loss.
    pub fn status(&self) -> String {
        let speed = |bps: f64| format!("{:>8} bits/s ({:.1} KB/s)", bps as i64, bps / 8192.0);
        let mut s = format!(
            "Current network status:\r\n  download speed:   {}\r\n  upload speed:     {}\r\n  minimum latency:  {} ms\r\n  maximum latency:  {} ms",
            speed(self.download_bps),
            speed(self.upload_bps),
            self.min_latency_ms,
            self.max_latency_ms
        );
        if self.is_lossy() {
            s.push_str(&format!("\r\n  download loss:    {} %\r\n  upload loss:      {} %", self.download_loss_pct, self.upload_loss_pct));
        }
        s
    }

    /// The conditions a `network status` report (as [`status`](Self::status) prints it, from
    /// simee's emulator or the stock one) says are set; `None` when a line is missing or isn't
    /// a number. Lines may end in CRLF or LF, and a trailing `OK` is ignored. A report without
    /// loss lines (stock's, or simee's with no loss) reads as no loss.
    pub fn from_status(report: &str) -> Option<Conditions> {
        let line = |label: &str| report.lines().map(str::trim).find_map(|l| l.strip_prefix(label));
        let value = |label: &str| -> Option<i64> { line(label)?.split_whitespace().next()?.parse().ok() };
        let loss = |label: &str| -> Option<f64> {
            match line(label) {
                None => Some(0.0),
                Some(l) => loss_pct(l.split_whitespace().next()?).ok(),
            }
        };
        Some(Conditions {
            download_bps: value("download speed:")? as f64,
            upload_bps: value("upload speed:")? as f64,
            min_latency_ms: i32::try_from(value("minimum latency:")?).ok()?,
            max_latency_ms: i32::try_from(value("maximum latency:")?).ok()?,
            upload_loss_pct: loss("upload loss:")?,
            download_loss_pct: loss("download loss:")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speed(arg: &str) -> Option<(f64, f64)> {
        let mut c = Conditions::default();
        c.set_speed(arg).then_some((c.upload_bps, c.download_bps))
    }

    fn latency(arg: &str) -> Option<(i32, i32)> {
        let mut c = Conditions::default();
        c.set_latency(arg).then_some((c.min_latency_ms, c.max_latency_ms))
    }

    /// Stock's constants_unittest.cpp cases, plus every preset.
    #[test]
    fn speeds_parse_as_stock() {
        assert_eq!(speed(""), Some((0.0, 0.0)));
        assert_eq!(speed("unknown-speed"), None);
        assert_eq!(speed("abcd12345"), None);
        assert_eq!(speed("1000"), Some((1e6, 1e6)));
        assert_eq!(speed("1000:20000"), Some((1e6, 2e7)));
        assert_eq!(speed("14.4"), Some((14400.0, 14400.0)));
        assert_eq!(speed("1000:"), Some((1e6, 1e6)), "nothing after ':' keeps the first value");
        assert_eq!(speed("1000kbps"), Some((1e6, 1e6)), "strtod stops at the unit");
        assert_eq!(speed("gsm"), Some((14400.0, 14400.0)));
        assert_eq!(speed("edge"), Some((473600.0, 473600.0)));
        assert_eq!(speed("umts"), Some((384000.0, 384000.0)));
        assert_eq!(speed("lte"), Some((58e6, 173e6)));
        assert_eq!(speed("full"), Some((0.0, 0.0)));
        assert_eq!(speed("5g"), Some((0.0, 0.0)));
        assert_eq!(speed("3g"), Some((3000.0, 3000.0)), "not a name: strtod reads the 3");
        assert_eq!(speed("GSM"), None, "names are case-sensitive");
        let names: Vec<_> = speeds().map(|p| p.name).collect();
        assert_eq!(names, ["gsm", "hscsd", "gprs", "umts", "edge", "hsdpa", "lte", "evdo", "5g", "full"]);
    }

    #[test]
    fn latencies_parse_as_stock() {
        assert_eq!(latency(""), Some((0, 0)));
        assert_eq!(latency("unknown-speed"), None);
        assert_eq!(latency("abcd12345"), None);
        assert_eq!(latency("1000"), Some((1000, 1000)));
        assert_eq!(latency("1000:20000"), Some((1000, 20000)));
        assert_eq!(latency("gsm"), Some((150, 550)));
        assert_eq!(latency("gprs"), Some((35, 200)));
        assert_eq!(latency("edge"), Some((80, 400)));
        assert_eq!(latency("umts"), Some((35, 200)));
        assert_eq!(latency("lte"), Some((0, 0)));
        assert_eq!(latency("none"), Some((0, 0)));
        assert_eq!(latency("12.9"), Some((12, 12)), "stored as int, truncated");
        let names: Vec<_> = latencies().map(|p| p.name).collect();
        assert_eq!(names, ["gsm", "hscsd", "gprs", "umts", "edge", "hsdpa", "lte", "evdo", "5g", "none"]);
    }

    #[test]
    fn a_rejected_argument_changes_nothing() {
        let mut c = Conditions::default();
        assert!(c.set_speed("edge") && c.set_latency("edge"));
        let before = c;
        assert!(!c.set_speed("warp"));
        assert!(!c.set_latency("."));
        assert_eq!(c, before);
        assert!(c.is_shaped());
        assert!(c.set_speed("full") && c.set_latency("none"));
        assert!(!c.is_shaped());
    }

    #[test]
    fn strtod_reads_c_prefixes() {
        assert_eq!(strtod("1e3x"), Some((1000.0, "x")));
        assert_eq!(strtod("1e"), Some((1.0, "e")), "an exponent needs digits");
        assert_eq!(strtod(".5:1"), Some((0.5, ":1")));
        assert_eq!(strtod(" -2"), Some((-2.0, "")));
        assert_eq!(strtod("-"), None);
        assert_eq!(strtod(":5"), None);
    }

    /// Byte for byte what stock's `%8d bits/s (%.1f KB/s)` and `%ld ms` print.
    #[test]
    fn status_reads_as_stock() {
        assert_eq!(
            Conditions::default().status(),
            "Current network status:\r\n  download speed:          0 bits/s (0.0 KB/s)\r\n  upload speed:            0 bits/s (0.0 KB/s)\r\n  minimum latency:  0 ms\r\n  maximum latency:  0 ms"
        );
        let mut c = Conditions::default();
        assert!(c.set_speed("hscsd") && c.set_latency("gsm"));
        assert_eq!(
            c.status(),
            "Current network status:\r\n  download speed:      57600 bits/s (7.0 KB/s)\r\n  upload speed:        14400 bits/s (1.8 KB/s)\r\n  minimum latency:  150 ms\r\n  maximum latency:  550 ms"
        );
        assert!(c.set_speed("lte"));
        assert!(c.status().contains("download speed:   173000000 bits/s (21118.2 KB/s)"), "wider than 8 digits isn't cut");
    }

    fn loss(arg: &str) -> Result<(f64, f64), String> {
        let mut c = Conditions::default();
        c.set_loss(arg).map(|()| (c.upload_loss_pct, c.download_loss_pct))
    }

    /// simee's `network loss`: one percent for both ways or `<up>:<down>`, each 0 to 100, an
    /// optional `%`, and `none` (or nothing) for no loss. Errors name the bad part.
    #[test]
    fn loss_parses_percentages_and_explains_what_it_refuses() {
        assert_eq!(loss("10"), Ok((10.0, 10.0)));
        assert_eq!(loss("5:20"), Ok((5.0, 20.0)));
        assert_eq!(loss("5%:20%"), Ok((5.0, 20.0)));
        assert_eq!(loss(" 12.5 "), Ok((12.5, 12.5)));
        assert_eq!(loss("0:100"), Ok((0.0, 100.0)));
        assert_eq!(loss("none"), Ok((0.0, 0.0)));
        assert_eq!(loss(""), Ok((0.0, 0.0)));
        assert_eq!(loss("-0"), Ok((0.0, 0.0)));
        let err = loss("lots").expect_err("a word");
        assert!(err.contains("`lots` isn't a percentage"), "{err}");
        let err = loss("10:101").expect_err("over 100");
        assert!(err.contains("`101`") && err.contains("0 to 100"), "{err}");
        let err = loss("-5").expect_err("negative");
        assert!(err.contains("0 to 100"), "{err}");
        assert!(loss("1:2:3").is_err(), "two values at most");
        assert!(loss("10:").is_err(), "a ':' needs a second value");
        assert!(loss("inf").is_err() && loss("nan").is_err());
        // A refused argument changes nothing.
        let mut c = Conditions::default();
        assert_eq!(c.set_loss("10:20"), Ok(()));
        assert!(c.is_shaped(), "loss alone is shaping");
        assert!(c.set_loss("abc").is_err());
        assert_eq!((c.upload_loss_pct, c.download_loss_pct), (10.0, 20.0));
    }

    /// No loss reads exactly as stock (the lines above); loss adds its two lines, which read
    /// back. Stock's report, with none, reads as no loss.
    #[test]
    fn loss_shows_in_status_and_reads_back() {
        let mut c = Conditions::default();
        assert!(!c.status().contains("loss"));
        assert_eq!(c.set_loss("2.5:30"), Ok(()));
        assert_eq!(
            c.status(),
            "Current network status:\r\n  download speed:          0 bits/s (0.0 KB/s)\r\n  upload speed:            0 bits/s (0.0 KB/s)\r\n  minimum latency:  0 ms\r\n  maximum latency:  0 ms\r\n  download loss:    30 %\r\n  upload loss:      2.5 %"
        );
        assert_eq!(Conditions::from_status(&c.status()), Some(c));
        assert_eq!(Conditions::from_status(&format!("{}\r\nOK", c.status()).replace("\r\n", "\n")), Some(c));
        assert_eq!(c.set_loss("none"), Ok(()));
        assert_eq!(Conditions::from_status(&c.status()), Some(Conditions::default()));
        assert_eq!(Conditions::from_status(&format!("{}\r\n  upload loss:      x %", c.status())), None, "a bad loss line");
    }

    /// What a client reads back from `network status` is what was set, from either line ending
    /// (the console's CRLF, or lines a client split and joined with LF), with or without `OK`.
    #[test]
    fn status_reads_back_as_the_conditions() {
        let mut c = Conditions::default();
        assert_eq!(Conditions::from_status(&c.status()), Some(c));
        assert!(c.set_speed("330:780") && c.set_latency("150:550"));
        assert_eq!(Conditions::from_status(&c.status()), Some(c));
        assert_eq!(Conditions::from_status(&c.status().replace("\r\n", "\n")), Some(c));
        assert_eq!(Conditions::from_status(&format!("{}\r\nOK", c.status())), Some(c));
        assert!(c.set_speed("lte"));
        assert_eq!(Conditions::from_status(&c.status()), Some(c), "wider than 8 digits");
        assert_eq!(Conditions::from_status("OK"), None);
        assert_eq!(Conditions::from_status("Current network status:\r\n  download speed:   x bits/s"), None);
    }
}
