//! What a client may do: which devices it may touch, which toolsets it may use, whether it may
//! change anything, and how boots are admitted. One [`Policy`] holds a default and the changes per
//! client, read from flags and from a JSON file (`simee mcp --policy`).
//!
//! ```json
//! {
//!   "default": {"read_only": false, "devices": ["ci-*", "@created"], "toolsets": ["core", "debug"]},
//!   "clients": {"review-bot": {"read_only": true, "devices": ["*"]}}
//! }
//! ```
//!
//! A client's rules are the default's with the client's own fields on top. A client is named by what
//! the transport vouches for, else by the `clientInfo.name` it declares, which is self-declared
//! and so no boundary between two clients that share a transport ([`crate::safety`]).
//! A misspelt key is an error: a policy that silently ignores a rule is worse than none.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use crate::tools::Toolsets;

/// In a `devices` list: the devices this client created, whatever their names.
pub const CREATED: &str = "@created";

/// How a boot heeds the host-health verdict (#275). The same names as `sim-api`'s `admission`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionMode {
    /// The agent may boot whatever it asks; the verdict is only reported.
    Ignore,
    /// A boot the host can't take now fails fast, with the verdict and every reason. An agent may
    /// ask for `wait` instead, never for `ignore`.
    Refuse,
}

impl AdmissionMode {
    pub fn parse(text: &str) -> Result<AdmissionMode, String> {
        match text {
            "ignore" => Ok(AdmissionMode::Ignore),
            "refuse" => Ok(AdmissionMode::Refuse),
            other => Err(format!("unknown admission `{other}` (choose ignore or refuse)")),
        }
    }
}

/// What one client may do.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientPolicy {
    /// Nothing that changes a device or the host runs (the catalog's effect is `Read`).
    pub read_only: bool,
    /// The toolsets, within what the server serves (`None`: all of them).
    pub toolsets: Option<Toolsets>,
    /// The devices by UDID or name pattern (`*` and `?`), and [`CREATED`] (`None`: any device).
    pub devices: Option<Vec<String>>,
    /// Tools that need confirmation beyond the built-in ones ([`crate::safety::CONFIRM`]).
    pub confirm_also: BTreeSet<String>,
    pub admission: AdmissionMode,
}

impl Default for ClientPolicy {
    fn default() -> ClientPolicy {
        ClientPolicy { read_only: false, toolsets: None, devices: None, confirm_also: BTreeSet::new(), admission: AdmissionMode::Refuse }
    }
}

impl ClientPolicy {
    /// Whether a device with this id and name is one the client may touch. `created` holds the
    /// ids of the devices it made.
    pub fn allows_device(&self, udid: &str, name: &str, created: &BTreeSet<String>) -> bool {
        let Some(patterns) = &self.devices else { return true };
        patterns.iter().any(|p| match p.as_str() {
            CREATED => created.contains(udid),
            p => glob(p, udid) || glob(p, name),
        })
    }

    /// Whether a device that doesn't exist yet may be made with this name: its name has to be one
    /// the client can touch afterwards, or the client touches what it made.
    pub fn allows_new_name(&self, name: &str) -> bool {
        match &self.devices {
            None => true,
            Some(patterns) => patterns.iter().any(|p| p == CREATED || glob(p, name)),
        }
    }

    /// The tools served to this client: the server's, narrowed by the client's toolsets.
    pub fn toolsets(&self, served: &Toolsets) -> Toolsets {
        match &self.toolsets {
            Some(own) => served.intersect(own),
            None => served.clone(),
        }
    }
}

/// The rules of every client.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Policy {
    pub default: ClientPolicy,
    clients: BTreeMap<String, Partial>,
}

/// The fields a policy entry sets; the rest come from the default.
#[derive(Clone, Debug, Default, PartialEq)]
struct Partial {
    read_only: Option<bool>,
    toolsets: Option<Toolsets>,
    devices: Option<Vec<String>>,
    confirm_also: Option<BTreeSet<String>>,
    admission: Option<AdmissionMode>,
}

impl Partial {
    fn apply(&self, onto: &mut ClientPolicy) {
        if let Some(v) = self.read_only {
            onto.read_only = v;
        }
        if let Some(v) = &self.toolsets {
            onto.toolsets = Some(v.clone());
        }
        if let Some(v) = &self.devices {
            onto.devices = Some(v.clone());
        }
        if let Some(v) = &self.confirm_also {
            onto.confirm_also = v.clone();
        }
        if let Some(v) = self.admission {
            onto.admission = v;
        }
    }
}

const KEYS: &[&str] = &["read_only", "toolsets", "devices", "confirm", "admission"];

impl Policy {
    /// `default` with `change` on top.
    pub fn with_default(mut self, change: impl FnOnce(&mut ClientPolicy)) -> Policy {
        change(&mut self.default);
        self
    }

    /// The clients the policy has rules of their own for.
    pub fn client_names(&self) -> impl Iterator<Item = &str> {
        self.clients.keys().map(String::as_str)
    }

    /// The rules for the client named `client`.
    pub fn for_client(&self, client: &str) -> ClientPolicy {
        let mut policy = self.default.clone();
        if let Some(own) = self.clients.get(client) {
            own.apply(&mut policy);
        }
        policy
    }

    /// Reads a policy file's text.
    pub fn parse(text: &str) -> Result<Policy, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let Value::Object(top) = value else { return Err("a policy is one JSON object".into()) };
        let mut policy = Policy::default();
        for (key, value) in &top {
            match key.as_str() {
                "default" => partial(value, "default")?.apply(&mut policy.default),
                "clients" => {
                    let Value::Object(clients) = value else { return Err("`clients` is an object of client names".into()) };
                    for (name, rules) in clients {
                        policy.clients.insert(name.clone(), partial(rules, &format!("clients.{name}"))?);
                    }
                }
                other => return Err(format!("unknown key `{other}` (choose from default, clients)")),
            }
        }
        Ok(policy)
    }

    /// Reads a policy file.
    pub fn from_file(path: &Path) -> Result<Policy, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Policy::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

fn partial(value: &Value, at: &str) -> Result<Partial, String> {
    let Value::Object(fields) = value else { return Err(format!("`{at}` is an object")) };
    let mut p = Partial::default();
    for (key, value) in fields {
        let here = format!("{at}.{key}");
        match key.as_str() {
            "read_only" => p.read_only = Some(value.as_bool().ok_or_else(|| format!("`{here}` is true or false"))?),
            "toolsets" => {
                let names = strings(value, &here)?;
                p.toolsets = Some(Toolsets::parse(&names.join(",")).map_err(|e| format!("`{here}`: {e}"))?);
            }
            "devices" => p.devices = Some(strings(value, &here)?),
            "confirm" => {
                let tools = strings(value, &here)?;
                // A misspelt tool would confirm nothing, and the operator would think it did.
                if let Some(unknown) = tools.iter().find(|t| !crate::tools::is_tool(t)) {
                    return Err(format!("`{here}`: `{unknown}` is not a tool (`tools/list` and docs/api.md name them)"));
                }
                p.confirm_also = Some(tools.into_iter().collect());
            }
            "admission" => {
                let text = value.as_str().ok_or_else(|| format!("`{here}` is ignore or refuse"))?;
                p.admission = Some(AdmissionMode::parse(text).map_err(|e| format!("`{here}`: {e}"))?);
            }
            _ => return Err(format!("unknown key `{here}` (choose from {})", KEYS.join(", "))),
        }
    }
    Ok(p)
}

fn strings(value: &Value, at: &str) -> Result<Vec<String>, String> {
    let list = value.as_array().ok_or_else(|| format!("`{at}` is a list of strings"))?;
    list.iter().map(|v| v.as_str().map(str::to_owned).ok_or_else(|| format!("`{at}` is a list of strings"))).collect()
}

/// Whether `text` matches `pattern`, where `*` is any run of characters and `?` any one.
pub fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests;
