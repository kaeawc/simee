//! `simee mcp`'s safety settings (docs/mcp-safety.md): the flags that become a [`Policy`] and an
//! audit log, and `simee mcp-audit`, which reads that log back.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Args;
use sim_mcp::audit::{self, AuditLog};
use sim_mcp::policy::{AdmissionMode, CREATED, Policy};
use sim_mcp::{Safety, audit::Query};

/// What a client may do, and the record of what it did. Destructive tools always ask for
/// confirmation, and there is no flag that turns that off.
#[derive(Args, Debug, Default, PartialEq)]
pub struct McpSafety {
    /// Let clients look and nothing else: only tools that change nothing run, and only those are
    /// listed.
    #[arg(long)]
    read_only: bool,
    /// A device a client may touch, by UDID or name (`*` matches any run of characters, `?` one),
    /// or `@created` for the devices it made itself. Repeatable; with none, any device. Devices
    /// outside the list are left out of what a client lists and refused when named.
    #[arg(long = "allow-device", value_name = "PATTERN")]
    allow_device: Vec<String>,
    /// Let a client touch only the devices it created (the same as `--allow-device @created`).
    #[arg(long)]
    created_devices_only: bool,
    /// A JSON file with the rules for each client: `{"default": {...}, "clients": {"name":
    /// {"read_only", "devices", "toolsets", "confirm", "admission"}}}`. The flags here replace its
    /// `default` fields.
    #[arg(long, value_name = "FILE")]
    policy: Option<PathBuf>,
    /// What a boot does when the host is too loaded for another device: `refuse` fails it with the
    /// verdict (an agent may ask to `wait` instead), `ignore` boots anyway and reports the verdict.
    #[arg(long, value_name = "MODE", value_parser = ["refuse", "ignore"])]
    admission: Option<String>,
    /// Where to write the audit log of every tool call (default: `mcp-audit.log` in simee's state
    /// directory).
    #[arg(long, value_name = "PATH", conflicts_with = "no_audit_log")]
    audit_log: Option<PathBuf>,
    /// Keep no audit log.
    #[arg(long)]
    no_audit_log: bool,
    /// The most a log file holds before it is rotated (three older files are kept).
    #[arg(long, value_name = "BYTES", conflicts_with = "no_audit_log")]
    audit_max_bytes: Option<u64>,
}

impl McpSafety {
    /// The policy: the file's, with the flags over its default.
    fn policy(&self) -> Result<Policy> {
        let mut policy = match &self.policy {
            Some(path) => Policy::from_file(path).map_err(anyhow::Error::msg)?,
            None => Policy::default(),
        };
        let mut devices = self.allow_device.clone();
        if self.created_devices_only && !devices.iter().any(|d| d == CREATED) {
            devices.push(CREATED.to_owned());
        }
        let admission = self.admission.as_deref().map(AdmissionMode::parse).transpose().map_err(anyhow::Error::msg)?;
        policy = policy.with_default(|rules| {
            if self.read_only {
                rules.read_only = true;
            }
            if !devices.is_empty() {
                rules.devices = Some(devices);
            }
            if let Some(mode) = admission {
                rules.admission = mode;
            }
        });
        Ok(policy)
    }

    fn audit(&self) -> Result<Option<AuditLog>> {
        if self.no_audit_log {
            return Ok(None);
        }
        let path = match &self.audit_log {
            Some(path) => path.clone(),
            None => audit::default_path().context("the audit log has no place (--no-audit-log keeps none)")?,
        };
        let mut config = audit::Config::new(path.clone());
        if let Some(max) = self.audit_max_bytes {
            config.max_bytes = max.max(1);
        }
        let log = AuditLog::open(config).with_context(|| format!("cannot open the audit log {}", path.display()))?;
        Ok(Some(log))
    }

    /// Makes these settings the process's, for every server it runs, and says what they are on
    /// stderr (stdout is the protocol).
    pub fn install(&self) -> Result<()> {
        let safety = Safety::new(self.policy()?, self.audit()?);
        match safety.audit_path() {
            Some(path) => eprintln!("simee mcp: audit log {}", path.display()),
            None => eprintln!("simee mcp: no audit log (--no-audit-log)"),
        }
        Safety::install(safety).map_err(anyhow::Error::msg)
    }
}

/// `simee mcp-audit`: what the audit log says, newest last.
#[derive(Args, Debug, PartialEq)]
pub struct McpAudit {
    /// The log to read (default: the one `simee mcp` writes).
    #[arg(long, value_name = "PATH")]
    log: Option<PathBuf>,
    /// How many of the most recent matching entries to show.
    #[arg(long, default_value_t = Query::DEFAULT_LIMIT)]
    limit: usize,
    /// Only calls of this tool.
    #[arg(long)]
    tool: Option<String>,
    /// Only this client's calls (the name it gave, or its transport's).
    #[arg(long)]
    client: Option<String>,
    /// Only calls that ended this way: `ok`, `error`, `refused` or `input_required`.
    #[arg(long, value_parser = ["ok", "error", "refused", "input_required"])]
    status: Option<String>,
    /// One JSON object per line instead of text.
    #[arg(long)]
    json: bool,
}

impl McpAudit {
    pub fn run(&self, out: &mut impl Write) -> Result<()> {
        let path = match &self.log {
            Some(path) => path.clone(),
            None => audit::default_path()?,
        };
        let query = Query { limit: Some(self.limit), tool: self.tool.clone(), client: self.client.clone(), status: self.status.clone() };
        for entry in audit::query(&path, &query).with_context(|| format!("cannot read the audit log {}", path.display()))? {
            if self.json {
                writeln!(out, "{entry}")?;
            } else {
                writeln!(out, "{}", audit::describe(&entry))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use serde_json::Value;

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        safety: McpSafety,
    }

    fn parse(args: &[&str]) -> Result<McpSafety, clap::Error> {
        Cli::try_parse_from(std::iter::once("simee-mcp").chain(args.iter().copied())).map(|c| c.safety)
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("simee-mcp-safety-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn with_no_flag_nothing_is_limited_and_boots_are_admitted() {
        let policy = parse(&[]).expect("parse").policy().expect("policy");
        let rules = policy.for_client("any");
        assert!(!rules.read_only && rules.devices.is_none() && rules.toolsets.is_none());
        assert_eq!(rules.admission, AdmissionMode::Refuse);
    }

    #[test]
    fn the_flags_make_the_default_rules() {
        let flags = parse(&["--read-only", "--allow-device", "ci-*", "--allow-device", "UDID-1", "--admission", "ignore"]).expect("parse");
        let rules = flags.policy().expect("policy").for_client("any");
        assert!(rules.read_only);
        assert_eq!(rules.devices, Some(vec!["ci-*".to_owned(), "UDID-1".to_owned()]));
        assert_eq!(rules.admission, AdmissionMode::Ignore);
        let created = parse(&["--created-devices-only"]).expect("parse").policy().expect("policy").for_client("any");
        assert_eq!(created.devices, Some(vec![CREATED.to_owned()]));
        let both = parse(&["--allow-device", "ci-*", "--created-devices-only"]).expect("parse").policy().expect("policy").for_client("any");
        assert_eq!(both.devices, Some(vec!["ci-*".to_owned(), CREATED.to_owned()]), "its own devices and the pattern's");
        assert!(parse(&["--admission", "wait"]).is_err(), "an agent may wait; the server's setting is refuse or ignore");
    }

    #[test]
    fn a_policy_file_sets_each_client_and_the_flags_replace_its_default() {
        let dir = scratch("file");
        let file = dir.join("policy.json");
        std::fs::write(&file, r#"{"default": {"devices": ["ci-*"]}, "clients": {"bot": {"read_only": true}}}"#).expect("write");
        let path = file.to_str().expect("utf-8");
        let from_file = parse(&["--policy", path]).expect("parse").policy().expect("policy");
        assert_eq!(from_file.for_client("x").devices, Some(vec!["ci-*".to_owned()]));
        assert!(from_file.for_client("bot").read_only && !from_file.for_client("x").read_only);
        let over = parse(&["--policy", path, "--allow-device", "lab-*"]).expect("parse").policy().expect("policy");
        assert_eq!(over.for_client("bot").devices, Some(vec!["lab-*".to_owned()]), "the flag replaces the file's default");
        assert!(over.for_client("bot").read_only, "and leaves the client's own rules");
        std::fs::write(&file, r#"{"default": {"read-only": true}}"#).expect("write");
        assert!(parse(&["--policy", path]).expect("parse").policy().expect_err("typo").to_string().contains("unknown key"));
        assert!(parse(&["--policy", "/nonexistent/p.json"]).expect("parse").policy().is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_audit_log_goes_where_it_is_told_or_nowhere() {
        let dir = scratch("log");
        let path = dir.join("a").join("audit.log");
        let log =
            parse(&["--audit-log", path.to_str().expect("utf-8"), "--audit-max-bytes", "4096"]).expect("parse").audit().expect("open");
        assert_eq!(log.expect("a log").path(), path);
        assert!(path.is_file());
        assert!(parse(&["--no-audit-log"]).expect("parse").audit().expect("none").is_none());
        assert!(parse(&["--no-audit-log", "--audit-log", "/tmp/x"]).is_err());
        assert!(parse(&["--no-audit-log", "--audit-max-bytes", "9"]).is_err());
        // A log that can't be opened is an error at the start, not a silent gap.
        let blocked = dir.join("file");
        std::fs::write(&blocked, "").expect("write");
        assert!(parse(&["--audit-log", blocked.join("under").to_str().expect("utf-8")]).expect("parse").audit().is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mcp_audit_prints_the_log_as_text_or_json_and_filters() {
        let dir = scratch("read");
        let path = dir.join("audit.log");
        let log = parse(&["--audit-log", path.to_str().expect("utf-8")]).expect("parse").audit().expect("open").expect("a log");
        for (tool, status) in [("tap", audit::status::OK), ("erase", audit::status::REFUSED), ("tap", audit::status::ERROR)] {
            log.record(&audit::Entry {
                client: "agent".into(),
                tool: tool.into(),
                args: serde_json::json!({"device": "d", "password": "hunter2"}),
                status,
                duration_ms: 5,
                detail: None,
                admission: None,
            });
        }
        let read = |extra: &[&str], json: bool| {
            let mut args = vec!["--log", path.to_str().expect("utf-8")];
            args.extend_from_slice(extra);
            if json {
                args.push("--json");
            }
            #[derive(Parser)]
            struct Audit {
                #[command(flatten)]
                audit: McpAudit,
            }
            let parsed = Audit::try_parse_from(std::iter::once("simee-mcp-audit").chain(args)).expect("parse").audit;
            let mut out = Vec::new();
            parsed.run(&mut out).expect("run");
            String::from_utf8(out).expect("utf-8")
        };
        let text = read(&["--tool", "tap"], false);
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains("agent tap ok 5 ms") && text.contains("agent tap error 5 ms") && !text.contains("hunter2"), "{text}");
        let json = read(&["--status", "refused"], true);
        let entry: Value = serde_json::from_str(json.trim()).expect("one JSON line");
        assert_eq!((entry["status"].as_str(), entry["tool"].as_str()), (Some("refused"), Some("erase")));
        assert_eq!(entry["args"]["password"], "[redacted]");
        assert_eq!(read(&["--limit", "1"], false).lines().count(), 1);
        assert_eq!(read(&["--client", "nobody"], false), "");
        let _ = std::fs::remove_dir_all(dir);
    }
}
