//! `simee mcp install <client>` and `simee mcp config <client>`: the stdio entry that starts
//! `simee mcp`, written into (or printed for) each MCP client's own config file.
//!
//! The merge touches one entry. Everything else in the file keeps its keys, their order and their
//! values; a file that isn't plain JSON (comments, trailing commas) is refused rather than
//! rewritten. Paths come from [`Host`], so tests name their own home directory and operating system.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Subcommand)]
pub enum McpAction {
    /// Add simee to an MCP client's config file, keeping every other server in it. Refuses to
    /// replace an existing simee entry without `--force`.
    Install {
        #[command(flatten)]
        entry: EntryArgs,
        /// Print the file that would be written and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Replace an existing entry of the same name.
        #[arg(long)]
        force: bool,
    },
    /// Print the config snippet for an MCP client and the file it belongs in.
    Config {
        #[command(flatten)]
        entry: EntryArgs,
    },
}

#[derive(Args)]
pub struct EntryArgs {
    /// The MCP client.
    client: Client,
    /// `user`: the client's per-user file. `project`: the file in the current directory.
    #[arg(long, value_enum, default_value = "user")]
    scope: Scope,
    /// The server's name in the client's config.
    #[arg(long, default_value = "simee")]
    name: String,
    /// Toolsets the server serves (`SIMEE_MCP_TOOLSETS`), e.g. `core,debug`.
    #[arg(long, value_name = "LIST")]
    toolsets: Option<String>,
    /// More environment for the server, `KEY=VALUE` (repeatable).
    #[arg(long, value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// The simee binary the client starts (default: this one, as an absolute path).
    #[arg(long, value_name = "PATH")]
    command: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum Client {
    ClaudeCode,
    ClaudeDesktop,
    Cursor,
    Vscode,
    Windsurf,
    Codex,
    Gemini,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum Scope {
    User,
    Project,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Macos,
    Linux,
    Windows,
}

/// What config paths depend on: the operating system and the directories it names.
pub struct Host {
    pub os: Os,
    pub home: PathBuf,
    /// `%APPDATA%` (Windows).
    pub appdata: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME` (Linux).
    pub xdg_config_home: Option<PathBuf>,
    pub cwd: PathBuf,
}

impl Host {
    pub fn current() -> Result<Host> {
        let os = if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        };
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = var(if os == Os::Windows { "USERPROFILE" } else { "HOME" }).context("no home directory: set HOME")?;
        Ok(Host { os, home, appdata: var("APPDATA"), xdg_config_home: var("XDG_CONFIG_HOME"), cwd: std::env::current_dir()? })
    }

    /// The per-user application config directory (`Code`, `Claude` live under it).
    fn app_config(&self) -> Result<PathBuf> {
        Ok(match self.os {
            Os::Macos => self.home.join("Library/Application Support"),
            Os::Linux => self.xdg_config_home.clone().unwrap_or_else(|| self.home.join(".config")),
            Os::Windows => self.appdata.clone().context("no %APPDATA%")?,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Json { servers_key: &'static str, typed: bool },
    Toml,
}

impl Client {
    fn label(self) -> &'static str {
        match self {
            Client::ClaudeCode => "Claude Code",
            Client::ClaudeDesktop => "Claude Desktop",
            Client::Cursor => "Cursor",
            Client::Vscode => "VS Code",
            Client::Windsurf => "Windsurf",
            Client::Codex => "Codex",
            Client::Gemini => "Gemini CLI",
        }
    }

    fn format(self) -> Format {
        match self {
            Client::Vscode => Format::Json { servers_key: "servers", typed: true },
            Client::ClaudeCode | Client::Cursor => Format::Json { servers_key: "mcpServers", typed: true },
            Client::ClaudeDesktop | Client::Windsurf | Client::Gemini => Format::Json { servers_key: "mcpServers", typed: false },
            Client::Codex => Format::Toml,
        }
    }

    /// The file the client reads its servers from.
    pub fn config_path(self, scope: Scope, host: &Host) -> Result<PathBuf> {
        let unsupported = |why: &str| bail!("{} has no {why}", self.label());
        Ok(match (self, scope) {
            (Client::ClaudeCode, Scope::User) => host.home.join(".claude.json"),
            (Client::ClaudeCode, Scope::Project) => host.cwd.join(".mcp.json"),
            (Client::ClaudeDesktop, Scope::User) => host.app_config()?.join("Claude/claude_desktop_config.json"),
            (Client::Cursor, Scope::User) => host.home.join(".cursor/mcp.json"),
            (Client::Cursor, Scope::Project) => host.cwd.join(".cursor/mcp.json"),
            (Client::Vscode, Scope::User) => host.app_config()?.join("Code/User/mcp.json"),
            (Client::Vscode, Scope::Project) => host.cwd.join(".vscode/mcp.json"),
            (Client::Windsurf, Scope::User) => host.home.join(".codeium/windsurf/mcp_config.json"),
            (Client::Codex, Scope::User) => host.home.join(".codex/config.toml"),
            (Client::Codex, Scope::Project) => host.cwd.join(".codex/config.toml"),
            (Client::Gemini, Scope::User) => host.home.join(".gemini/settings.json"),
            (Client::Gemini, Scope::Project) => host.cwd.join(".gemini/settings.json"),
            (Client::ClaudeDesktop, Scope::Project) => return unsupported("project config file (use --scope user)"),
            (Client::Windsurf, Scope::Project) => return unsupported("project config file (use --scope user)"),
        })
    }
}

/// The server to start: `command args` with `env`.
#[derive(Debug, PartialEq)]
pub struct Entry {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Entry {
    fn from_args(a: &EntryArgs) -> Result<Entry> {
        let command = match &a.command {
            Some(p) => p.clone(),
            None => std::env::current_exe().context("can't find this binary: pass --command")?,
        };
        let command = std::path::absolute(&command).unwrap_or(command);
        let mut env = BTreeMap::new();
        if let Some(list) = &a.toolsets {
            sim_mcp::Toolsets::parse(list).map_err(anyhow::Error::msg).context("--toolsets")?;
            env.insert("SIMEE_MCP_TOOLSETS".to_string(), list.clone());
        }
        for pair in &a.env {
            let (k, v) =
                pair.split_once('=').filter(|(k, _)| !k.is_empty()).with_context(|| format!("--env {pair:?}: expected KEY=VALUE"))?;
            env.insert(k.to_string(), v.to_string());
        }
        Ok(Entry { command: command.to_string_lossy().into_owned(), args: vec!["mcp".to_string()], env })
    }

    fn json(&self, typed: bool) -> J {
        let mut o = Vec::new();
        if typed {
            o.push(("type".to_string(), J::Scalar(Value::from("stdio"))));
        }
        o.push(("command".to_string(), J::Scalar(Value::from(self.command.as_str()))));
        o.push(("args".to_string(), J::Arr(self.args.iter().map(|a| J::Scalar(Value::from(a.as_str()))).collect())));
        if !self.env.is_empty() {
            o.push(("env".to_string(), J::Obj(self.env.iter().map(|(k, v)| (k.clone(), J::Scalar(Value::from(v.as_str())))).collect())));
        }
        J::Obj(o)
    }

    fn toml(&self) -> toml_edit::Table {
        let mut t = toml_edit::Table::new();
        t["command"] = toml_edit::value(self.command.as_str());
        t["args"] = toml_edit::value(self.args.iter().map(String::as_str).collect::<toml_edit::Array>());
        if !self.env.is_empty() {
            let mut env = toml_edit::Table::new();
            for (k, v) in &self.env {
                env[k.as_str()] = toml_edit::value(v.as_str());
            }
            t["env"] = toml_edit::Item::Table(env);
        }
        t
    }
}

/// A JSON value that keeps its objects' key order, so a merge doesn't reshuffle the user's file.
#[derive(Clone, Debug, PartialEq)]
enum J {
    Obj(Vec<(String, J)>),
    Arr(Vec<J>),
    Scalar(Value),
}

impl<'de> Deserialize<'de> for J {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<J, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = J;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<J, A::Error> {
                let mut o = Vec::new();
                while let Some(kv) = m.next_entry::<String, J>()? {
                    o.push(kv);
                }
                Ok(J::Obj(o))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<J, A::Error> {
                let mut a = Vec::new();
                while let Some(v) = s.next_element::<J>()? {
                    a.push(v);
                }
                Ok(J::Arr(a))
            }
            fn visit_bool<E>(self, v: bool) -> Result<J, E> {
                Ok(J::Scalar(Value::Bool(v)))
            }
            fn visit_i64<E>(self, v: i64) -> Result<J, E> {
                Ok(J::Scalar(v.into()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<J, E> {
                Ok(J::Scalar(v.into()))
            }
            fn visit_f64<E>(self, v: f64) -> Result<J, E> {
                Ok(J::Scalar(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)))
            }
            fn visit_str<E>(self, v: &str) -> Result<J, E> {
                Ok(J::Scalar(Value::String(v.to_string())))
            }
            fn visit_unit<E>(self) -> Result<J, E> {
                Ok(J::Scalar(Value::Null))
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for J {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            J::Obj(o) => {
                let mut m = s.serialize_map(Some(o.len()))?;
                for (k, v) in o {
                    m.serialize_entry(k, v)?;
                }
                m.end()
            }
            J::Arr(a) => {
                let mut q = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    q.serialize_element(v)?;
                }
                q.end()
            }
            J::Scalar(v) => v.serialize(s),
        }
    }
}

impl J {
    fn pretty(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("a JSON tree serializes");
        s.push('\n');
        s
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut J> {
        match self {
            J::Obj(o) => o.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// What a merge did to the file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// Nothing to write: the entry is already there, the same.
    Unchanged,
    Created,
    Added,
    Replaced,
}

/// `existing` (None: no file) with the entry in. An entry of the same name that differs is an
/// error unless `force`.
pub fn merge(client: Client, existing: Option<&str>, name: &str, entry: &Entry, force: bool) -> Result<(String, Change)> {
    let blank = existing.is_none_or(|t| t.trim().is_empty());
    match client.format() {
        Format::Json { servers_key, typed } => {
            let mut doc = if blank {
                J::Obj(Vec::new())
            } else {
                serde_json::from_str::<J>(existing.unwrap_or_default()).context(
                    "the file isn't plain JSON (comments and trailing commas aren't read); add the snippet from `simee mcp config` by hand",
                )?
            };
            let J::Obj(top) = &mut doc else { bail!("the file's top level isn't a JSON object") };
            if !top.iter().any(|(k, _)| k == servers_key) {
                top.push((servers_key.to_string(), J::Obj(Vec::new())));
            }
            let Some(J::Obj(servers)) = doc.get_mut(servers_key) else { bail!("`{servers_key}` in the file isn't a JSON object") };
            let new = entry.json(typed);
            let change = match servers.iter_mut().find(|(k, _)| k == name) {
                Some((_, old)) if *old == new => return Ok((existing.unwrap_or_default().to_string(), Change::Unchanged)),
                Some(_) if !force => bail!("`{name}` is already in `{servers_key}` with different settings; pass --force to replace it"),
                Some((_, old)) => {
                    *old = new;
                    Change::Replaced
                }
                None => {
                    servers.push((name.to_string(), new));
                    if blank { Change::Created } else { Change::Added }
                }
            };
            Ok((doc.pretty(), change))
        }
        Format::Toml => {
            let mut doc: toml_edit::DocumentMut =
                if blank { Default::default() } else { existing.unwrap_or_default().parse().context("the file isn't valid TOML")? };
            let servers = doc
                .entry("mcp_servers")
                .or_insert_with(|| {
                    let mut t = toml_edit::Table::new();
                    t.set_implicit(true);
                    toml_edit::Item::Table(t)
                })
                .as_table_mut()
                .context("`mcp_servers` in the file isn't a table")?;
            let new = entry.toml();
            let change = match servers.get(name) {
                Some(old) if old.as_table().is_some_and(|t| t.to_string() == new.to_string()) => {
                    return Ok((existing.unwrap_or_default().to_string(), Change::Unchanged));
                }
                Some(_) if !force => bail!("`{name}` is already in `mcp_servers` with different settings; pass --force to replace it"),
                Some(_) => Change::Replaced,
                None if blank => Change::Created,
                None => Change::Added,
            };
            servers.insert(name, toml_edit::Item::Table(new));
            Ok((doc.to_string(), change))
        }
    }
}

/// The snippet `simee mcp config` prints: the whole file for JSON, the table for TOML.
pub fn snippet(client: Client, name: &str, entry: &Entry) -> String {
    match client.format() {
        Format::Json { servers_key, typed } => {
            J::Obj(vec![(servers_key.to_string(), J::Obj(vec![(name.to_string(), entry.json(typed))]))]).pretty()
        }
        Format::Toml => {
            let mut doc = toml_edit::DocumentMut::new();
            let mut servers = toml_edit::Table::new();
            servers.set_implicit(true);
            servers.insert(name, toml_edit::Item::Table(entry.toml()));
            doc.insert("mcp_servers", toml_edit::Item::Table(servers));
            doc.to_string()
        }
    }
}

/// Replaces `path` with `text` in one rename, so a reader never sees half a file. An existing
/// file's permissions carry over (and a symlink is written through); a new file is private,
/// since its env can hold secrets.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().context("config path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = target.file_name().context("config path has no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.simee-{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        match std::fs::metadata(&target) {
            Ok(meta) => f.set_permissions(meta.permissions())?,
            #[cfg(unix)]
            Err(_) => {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            #[cfg(not(unix))]
            Err(_) => {}
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", target.display()))
}

fn read_existing(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn run(action: &McpAction, host: &Host, out: &mut dyn Write) -> Result<()> {
    match action {
        McpAction::Config { entry: a } => {
            let entry = Entry::from_args(a)?;
            let path = a.client.config_path(a.scope, host)?;
            writeln!(out, "# {}: {}", a.client.label(), path.display())?;
            write!(out, "{}", snippet(a.client, &a.name, &entry))?;
        }
        McpAction::Install { entry: a, dry_run, force } => {
            let entry = Entry::from_args(a)?;
            let path = a.client.config_path(a.scope, host)?;
            let existing = read_existing(&path)?;
            let (text, change) =
                merge(a.client, existing.as_deref(), &a.name, &entry, *force).with_context(|| path.display().to_string())?;
            let verb = match change {
                Change::Unchanged => "already has",
                Change::Created => "create",
                Change::Added => "add `simee` to",
                Change::Replaced => "replace `simee` in",
            };
            if *dry_run {
                let mut shown = format!("would {verb} {}", path.display());
                if change != Change::Unchanged {
                    write!(shown, ":\n{text}")?;
                }
                writeln!(out, "{}", shown.trim_end())?;
                return Ok(());
            }
            if change != Change::Unchanged {
                write_atomic(&path, &text)?;
            }
            let done = match change {
                Change::Unchanged => "already set up in",
                Change::Created => "created",
                Change::Added => "added to",
                Change::Replaced => "replaced in",
            };
            writeln!(out, "{}: simee {done} {}. Restart {} to load it.", a.client.label(), path.display(), a.client.label())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn host(os: Os) -> Host {
        Host {
            os,
            home: PathBuf::from("/home/u"),
            appdata: Some(PathBuf::from("/appdata")),
            xdg_config_home: None,
            cwd: PathBuf::from("/work"),
        }
    }

    fn entry() -> Entry {
        Entry { command: "/bin/simee".into(), args: vec!["mcp".into()], env: BTreeMap::new() }
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Dir {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!("simee-mcp-install-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&p).expect("ok");
            Dir(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn host_in(dir: &Dir) -> Host {
        Host { os: Os::Linux, home: dir.0.join("home"), appdata: None, xdg_config_home: None, cwd: dir.0.join("cwd") }
    }

    fn args(client: &str, extra: &[&str]) -> McpAction {
        use clap::Parser;
        #[derive(Parser)]
        struct P {
            #[command(subcommand)]
            a: McpAction,
        }
        let cmd: Vec<&str> = ["p", "install", client, "--command", "/bin/simee"].into_iter().chain(extra.iter().copied()).collect();
        P::try_parse_from(cmd).unwrap_or_else(|e| panic!("{e}")).a
    }

    #[test]
    fn paths_per_client_and_os() {
        let p = |c: Client, s, os| c.config_path(s, &host(os)).map(|p| p.to_string_lossy().replace('\\', "/"));
        assert_eq!(p(Client::ClaudeCode, Scope::User, Os::Linux).expect("ok"), "/home/u/.claude.json");
        assert_eq!(p(Client::ClaudeCode, Scope::Project, Os::Linux).expect("ok"), "/work/.mcp.json");
        assert_eq!(
            p(Client::ClaudeDesktop, Scope::User, Os::Macos).expect("ok"),
            "/home/u/Library/Application Support/Claude/claude_desktop_config.json"
        );
        assert_eq!(p(Client::ClaudeDesktop, Scope::User, Os::Linux).expect("ok"), "/home/u/.config/Claude/claude_desktop_config.json");
        assert_eq!(p(Client::ClaudeDesktop, Scope::User, Os::Windows).expect("ok"), "/appdata/Claude/claude_desktop_config.json");
        assert_eq!(p(Client::Cursor, Scope::User, Os::Macos).expect("ok"), "/home/u/.cursor/mcp.json");
        assert_eq!(p(Client::Cursor, Scope::Project, Os::Macos).expect("ok"), "/work/.cursor/mcp.json");
        assert_eq!(p(Client::Vscode, Scope::User, Os::Macos).expect("ok"), "/home/u/Library/Application Support/Code/User/mcp.json");
        assert_eq!(p(Client::Vscode, Scope::User, Os::Linux).expect("ok"), "/home/u/.config/Code/User/mcp.json");
        assert_eq!(p(Client::Vscode, Scope::User, Os::Windows).expect("ok"), "/appdata/Code/User/mcp.json");
        assert_eq!(p(Client::Vscode, Scope::Project, Os::Linux).expect("ok"), "/work/.vscode/mcp.json");
        assert_eq!(p(Client::Windsurf, Scope::User, Os::Macos).expect("ok"), "/home/u/.codeium/windsurf/mcp_config.json");
        assert_eq!(p(Client::Codex, Scope::User, Os::Linux).expect("ok"), "/home/u/.codex/config.toml");
        assert_eq!(p(Client::Codex, Scope::Project, Os::Linux).expect("ok"), "/work/.codex/config.toml");
        assert_eq!(p(Client::Gemini, Scope::User, Os::Linux).expect("ok"), "/home/u/.gemini/settings.json");
        assert_eq!(p(Client::Gemini, Scope::Project, Os::Linux).expect("ok"), "/work/.gemini/settings.json");
    }

    #[test]
    fn xdg_config_home_moves_linux_app_config_and_windows_needs_appdata() {
        let mut h = host(Os::Linux);
        h.xdg_config_home = Some(PathBuf::from("/xdg"));
        assert_eq!(Client::Vscode.config_path(Scope::User, &h).expect("ok"), PathBuf::from("/xdg/Code/User/mcp.json"));
        let mut w = host(Os::Windows);
        w.appdata = None;
        assert!(Client::Vscode.config_path(Scope::User, &w).expect_err("must fail").to_string().contains("APPDATA"));
    }

    #[test]
    fn clients_without_a_project_file_say_so() {
        for c in [Client::ClaudeDesktop, Client::Windsurf] {
            let e = c.config_path(Scope::Project, &host(Os::Linux)).expect_err("must fail").to_string();
            assert!(e.contains("--scope user"), "{e}");
        }
    }

    #[test]
    fn json_merge_keeps_other_servers_and_key_order() {
        let existing = r#"{"zeta": 1, "mcpServers": {"other": {"command": "x", "args": ["a"]}, "alpha": {"url": "http://h"}}, "alpha": [1, 2.5, null, true]}"#;
        let (text, change) = merge(Client::ClaudeDesktop, Some(existing), "simee", &entry(), false).expect("ok");
        assert_eq!(change, Change::Added);
        let v: Value = serde_json::from_str(&text).expect("ok");
        assert_eq!(v["zeta"], 1);
        assert_eq!(v["alpha"], serde_json::json!([1, 2.5, null, true]));
        assert_eq!(v["mcpServers"]["other"], serde_json::json!({"command": "x", "args": ["a"]}));
        assert_eq!(v["mcpServers"]["alpha"], serde_json::json!({"url": "http://h"}));
        assert_eq!(v["mcpServers"]["simee"], serde_json::json!({"command": "/bin/simee", "args": ["mcp"]}));
        let order = |s: &str| ["zeta", "mcpServers", "\"alpha\": ["].map(|k| s.find(k).expect("ok"));
        assert!(order(&text).windows(2).all(|w| w[0] < w[1]), "top-level order kept:\n{text}");
        assert!(text.find("\"other\"").expect("ok") < text.find("\"simee\"").expect("ok"), "new entry goes last");
    }

    #[test]
    fn each_json_client_uses_its_key_and_shape() {
        let cases = [
            (Client::ClaudeCode, "mcpServers", true),
            (Client::ClaudeDesktop, "mcpServers", false),
            (Client::Cursor, "mcpServers", true),
            (Client::Vscode, "servers", true),
            (Client::Windsurf, "mcpServers", false),
            (Client::Gemini, "mcpServers", false),
        ];
        for (client, key, typed) in cases {
            let (text, change) = merge(client, None, "simee", &entry(), false).expect("ok");
            assert_eq!(change, Change::Created);
            let v: Value = serde_json::from_str(&text).expect("ok");
            let e = &v[key]["simee"];
            assert_eq!(e["command"], "/bin/simee", "{client:?}");
            assert_eq!(e["args"], serde_json::json!(["mcp"]));
            assert_eq!(e.get("type").is_some(), typed, "{client:?}");
            assert!(text.ends_with("}\n"));
            assert_eq!(snippet(client, "simee", &entry()), text, "the snippet is the file a new install writes");
        }
    }

    #[test]
    fn env_and_toolsets_are_written() {
        let a = args("cursor", &["--toolsets", "core,debug", "--env", "A=b=c"]);
        let McpAction::Install { entry: a, .. } = a else { panic!() };
        let e = Entry::from_args(&a).expect("ok");
        assert_eq!(e.env["SIMEE_MCP_TOOLSETS"], "core,debug");
        assert_eq!(e.env["A"], "b=c");
        let (text, _) = merge(Client::Cursor, None, "simee", &e, false).expect("ok");
        let v: Value = serde_json::from_str(&text).expect("ok");
        assert_eq!(v["mcpServers"]["simee"]["env"], serde_json::json!({"A": "b=c", "SIMEE_MCP_TOOLSETS": "core,debug"}));
    }

    #[test]
    fn bad_toolsets_and_env_are_refused() {
        for bad in [&["--toolsets", "nonsense"][..], &["--env", "NOEQUALS"], &["--env", "=v"]] {
            let McpAction::Install { entry: a, .. } = args("cursor", bad) else { panic!() };
            assert!(Entry::from_args(&a).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn existing_entry_needs_force_but_an_identical_one_is_a_no_op() {
        let existing = r#"{"mcpServers": {"simee": {"command": "/old/simee", "args": ["mcp"]}, "other": {}}}"#;
        let e = merge(Client::ClaudeDesktop, Some(existing), "simee", &entry(), false).expect_err("must fail").to_string();
        assert!(e.contains("--force"), "{e}");
        let (text, change) = merge(Client::ClaudeDesktop, Some(existing), "simee", &entry(), true).expect("ok");
        assert_eq!(change, Change::Replaced);
        let v: Value = serde_json::from_str(&text).expect("ok");
        assert_eq!(v["mcpServers"]["simee"]["command"], "/bin/simee");
        assert!(v["mcpServers"]["other"].is_object());
        let (same, change) = merge(Client::ClaudeDesktop, Some(&text), "simee", &entry(), false).expect("ok");
        assert_eq!((same, change), (text, Change::Unchanged));
    }

    #[test]
    fn json_that_isnt_plain_or_isnt_an_object_is_refused_untouched() {
        for bad in ["{\"mcpServers\": {} // comment\n}", "[]", "{\"mcpServers\": []}", "{\"mcpServers\": {},}"] {
            assert!(merge(Client::Cursor, Some(bad), "simee", &entry(), true).is_err(), "{bad}");
        }
        // An empty file is a new one.
        assert_eq!(merge(Client::Cursor, Some("  \n"), "simee", &entry(), false).expect("ok").1, Change::Created);
    }

    #[test]
    fn toml_merge_keeps_comments_and_other_servers() {
        let existing = "# my config\nmodel = \"x\"\n\n[mcp_servers.other]\ncommand = \"npx\" # keep\nargs = [\"-y\", \"p\"]\n\n[mcp_servers.other.env]\nK = \"v\"\n\n[tui]\nfoo = 1\n";
        let mut e = entry();
        e.env.insert("SIMEE_MCP_TOOLSETS".into(), "all".into());
        let (text, change) = merge(Client::Codex, Some(existing), "simee", &e, false).expect("ok");
        assert_eq!(change, Change::Added);
        for kept in ["# my config", "model = \"x\"", "command = \"npx\" # keep", "[mcp_servers.other.env]", "[tui]"] {
            assert!(text.contains(kept), "{kept} lost:\n{text}");
        }
        let v: toml_edit::DocumentMut = text.parse().expect("ok");
        assert_eq!(v["mcp_servers"]["simee"]["command"].as_str(), Some("/bin/simee"));
        assert_eq!(v["mcp_servers"]["simee"]["args"].as_array().expect("ok").len(), 1);
        assert_eq!(v["mcp_servers"]["simee"]["env"]["SIMEE_MCP_TOOLSETS"].as_str(), Some("all"));
        assert_eq!(v["mcp_servers"]["other"]["env"]["K"].as_str(), Some("v"));
    }

    #[test]
    fn toml_force_and_new_file() {
        let (text, change) = merge(Client::Codex, None, "simee", &entry(), false).expect("ok");
        assert_eq!(change, Change::Created);
        assert!(text.contains("[mcp_servers.simee]"), "{text}");
        assert_eq!(snippet(Client::Codex, "simee", &entry()), text);
        let mut other = entry();
        other.command = "/elsewhere".into();
        assert!(merge(Client::Codex, Some(&text), "simee", &other, false).expect_err("must fail").to_string().contains("--force"));
        assert_eq!(merge(Client::Codex, Some(&text), "simee", &entry(), false).expect("ok").1, Change::Unchanged);
        let (forced, change) = merge(Client::Codex, Some(&text), "simee", &other, true).expect("ok");
        assert_eq!(change, Change::Replaced);
        assert!(forced.contains("/elsewhere") && !forced.contains("/bin/simee"), "{forced}");
        assert!(merge(Client::Codex, Some("[mcp_servers"), "simee", &entry(), true).is_err());
    }

    #[test]
    fn install_writes_and_dry_run_does_not() {
        let dir = Dir::new();
        let host = host_in(&dir);
        let path = Client::Cursor.config_path(Scope::User, &host).expect("ok");
        let mut out = Vec::new();
        run(&args("cursor", &["--dry-run"]), &host, &mut out).expect("ok");
        let shown = String::from_utf8(out).expect("ok");
        assert!(shown.starts_with("would create") && shown.contains("\"/bin/simee\""), "{shown}");
        assert!(!path.exists() && !path.parent().expect("ok").exists(), "dry run wrote nothing");

        run(&args("cursor", &[]), &host, &mut Vec::new()).expect("ok");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("ok")).expect("ok");
        assert_eq!(v["mcpServers"]["simee"]["command"], "/bin/simee");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("ok")).expect("ok").collect();
        assert_eq!(leftovers.len(), 1, "no temp file left");

        // A dry run over an existing file leaves it byte for byte.
        let before = std::fs::read(&path).expect("ok");
        run(&args("cursor", &["--dry-run", "--force", "--toolsets", "all"]), &host, &mut Vec::new()).expect("ok");
        assert_eq!(std::fs::read(&path).expect("ok"), before);
    }

    #[test]
    fn install_refuses_then_forces_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new();
        let host = host_in(&dir);
        let path = Client::ClaudeCode.config_path(Scope::User, &host).expect("ok");
        std::fs::create_dir_all(&host.home).expect("ok");
        std::fs::write(&path, r#"{"numStartups": 7, "mcpServers": {"simee": {"command": "/old"}, "keep": {"command": "k"}}}"#).expect("ok");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("ok");

        let before = std::fs::read(&path).expect("ok");
        let e = run(&args("claude-code", &[]), &host, &mut Vec::new()).expect_err("must fail");
        assert!(format!("{e:#}").contains("--force"), "{e:#}");
        assert_eq!(std::fs::read(&path).expect("ok"), before, "a refusal leaves the file alone");

        run(&args("claude-code", &["--force"]), &host, &mut Vec::new()).expect("ok");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("ok")).expect("ok");
        assert_eq!(v["numStartups"], 7);
        assert_eq!(v["mcpServers"]["simee"]["command"], "/bin/simee");
        assert_eq!(v["mcpServers"]["keep"]["command"], "k");
        assert_eq!(std::fs::metadata(&path).expect("ok").permissions().mode() & 0o777, 0o640);
    }

    #[test]
    fn a_new_file_is_private_and_a_symlink_is_written_through() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new();
        let fresh = dir.0.join("sub/new.json");
        write_atomic(&fresh, "{}\n").expect("ok");
        assert_eq!(std::fs::metadata(&fresh).expect("ok").permissions().mode() & 0o777, 0o600);

        let real = dir.0.join("dotfiles.json");
        let link = dir.0.join("link.json");
        std::fs::write(&real, "{}").expect("ok");
        std::os::unix::fs::symlink(&real, &link).expect("ok");
        write_atomic(&link, "{\"a\":1}\n").expect("ok");
        assert!(std::fs::symlink_metadata(&link).expect("ok").file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).expect("ok"), "{\"a\":1}\n");
    }

    #[test]
    fn config_prints_the_path_and_the_snippet() {
        let dir = Dir::new();
        let host = host_in(&dir);
        let McpAction::Install { entry: a, .. } = args("vscode", &["--scope", "project"]) else { panic!() };
        let mut out = Vec::new();
        run(&McpAction::Config { entry: a }, &host, &mut out).expect("ok");
        let s = String::from_utf8(out).expect("ok");
        assert!(s.starts_with("# VS Code: ") && s.contains(".vscode/mcp.json"), "{s}");
        assert!(s.contains("\"servers\"") && s.contains("\"type\": \"stdio\""), "{s}");
        assert!(!dir.0.join("cwd").exists(), "config writes nothing");
    }
}
