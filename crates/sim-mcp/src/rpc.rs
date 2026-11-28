//! JSON-RPC messages and the protocol's fixed names: versions, `_meta` keys, error codes, and what
//! a request says about its client.

use serde_json::{Value, json};

/// The current revision: stateless, every request carries its version and capabilities.
pub const MODERN: &str = "2026-07-28";
/// Revisions that open with an `initialize` handshake, newest first.
pub const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

pub const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CLIENT_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
pub const META_SUBSCRIPTION: &str = "io.modelcontextprotocol/subscriptionId";

/// The Tasks extension: long operations as pollable handles.
pub const EXT_TASKS: &str = "io.modelcontextprotocol/tasks";
/// The MCP Apps extension: `ui://` resources a host renders next to a conversation.
pub const EXT_UI: &str = "io.modelcontextprotocol/ui";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// Reserved for the specification: the request needs an extension the client didn't declare.
pub const MISSING_CAPABILITY: i64 = -32021;
pub const UNSUPPORTED_VERSION: i64 = -32022;
/// What revisions before 2026-07-28 call "resource not found" (the current one uses -32602).
pub const RESOURCE_NOT_FOUND_LEGACY: i64 = -32002;

/// A JSON-RPC error.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError { code, message: message.into(), data: None }
    }

    pub fn invalid_params(message: impl Into<String>) -> RpcError {
        RpcError::new(INVALID_PARAMS, message)
    }

    pub fn with_data(mut self, data: Value) -> RpcError {
        self.data = Some(data);
        self
    }

    pub fn to_json(&self) -> Value {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        error
    }
}

/// One incoming message.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
}

/// Reads one line. Responses (this server sends no requests) and anything else that isn't a
/// request or notification are errors; so are batches, which no supported revision has.
pub fn parse(line: &str) -> Result<Incoming, RpcError> {
    let value: Value = serde_json::from_str(line).map_err(|e| RpcError::new(PARSE_ERROR, format!("parse error: {e}")))?;
    let Value::Object(mut msg) = value else {
        return Err(RpcError::new(INVALID_REQUEST, "a message is one JSON object (batches are not supported)"));
    };
    if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(RpcError::new(INVALID_REQUEST, "`jsonrpc` must be \"2.0\""));
    }
    let Some(Value::String(method)) = msg.remove("method") else {
        return Err(RpcError::new(INVALID_REQUEST, "a request has a string `method`"));
    };
    let params = msg.remove("params").unwrap_or(Value::Null);
    match msg.remove("id") {
        Some(id @ (Value::String(_) | Value::Number(_))) => Ok(Incoming::Request { id, method, params }),
        Some(_) => Err(RpcError::new(INVALID_REQUEST, "`id` is a string or a number")),
        None => Ok(Incoming::Notification { method, params }),
    }
}

/// Reads a client's response (to a request this server sent): its id, and the result or the error.
/// `None` for anything else.
pub fn parse_response(line: &str) -> Option<(Value, Result<Value, Value>)> {
    let Value::Object(mut msg) = serde_json::from_str(line).ok()? else { return None };
    if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || msg.contains_key("method") {
        return None;
    }
    let id = msg.remove("id").filter(|id| id.is_string() || id.is_number())?;
    match (msg.remove("result"), msg.remove("error")) {
        (Some(result), None) => Some((id, Ok(result))),
        (None, Some(error)) => Some((id, Err(error))),
        _ => None,
    }
}

/// Which protocol generation a request speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Era {
    /// Version and capabilities travel in each request's `_meta`.
    Modern,
    /// After an `initialize` handshake.
    Legacy,
}

/// What a request says about itself: its era, the capabilities of its client and the token to
/// report progress under.
#[derive(Clone, Debug)]
pub struct Ctx {
    pub era: Era,
    /// The revision: [`MODERN`], or the one `initialize` agreed on.
    pub version: &'static str,
    pub caps: Value,
    pub progress_token: Option<Value>,
}

impl Ctx {
    /// Whether the client declared the extension `name` (`io.modelcontextprotocol/tasks`).
    pub fn has_extension(&self, name: &str) -> bool {
        self.caps.get("extensions").is_some_and(|e| e.get(name).is_some())
    }

    pub fn modern(&self) -> bool {
        self.era == Era::Modern
    }

    /// Whether the revision has tool output schemas (2025-06-18 on; 2025-03-26 has none).
    pub fn output_schemas(&self) -> bool {
        self.version != "2025-03-26"
    }
}

/// The `_meta` of a request's params.
pub fn meta(params: &Value) -> &Value {
    params.get("_meta").unwrap_or(&Value::Null)
}

/// The version a modern request asked for, if it carries one.
pub fn requested_version(params: &Value) -> Option<&str> {
    meta(params).get(META_VERSION).and_then(Value::as_str)
}

/// An error as the client's revision spells it: "resource not found" is -32602 for the current
/// revision and -32002 for the ones before it; everything else is the same in both.
pub fn for_era(mut error: RpcError, era: Era) -> RpcError {
    if era == Era::Legacy && error.code == INVALID_PARAMS && error.message == "Resource not found" {
        error.code = RESOURCE_NOT_FOUND_LEGACY;
    }
    error
}

/// The error for a version this server doesn't speak.
pub fn unsupported_version(requested: &str) -> RpcError {
    let supported: Vec<&str> = std::iter::once(MODERN).chain(LEGACY.iter().copied()).collect();
    RpcError::new(UNSUPPORTED_VERSION, "Unsupported protocol version").with_data(json!({"supported": supported, "requested": requested}))
}

/// The error for a request from a client that didn't declare the extension it needs.
pub fn missing_extension(name: &str) -> RpcError {
    RpcError::new(MISSING_CAPABILITY, "Missing required client capability")
        .with_data(json!({"requiredCapabilities": {"extensions": {name: {}}}}))
}

/// A request's key in the table of requests in flight.
pub fn id_key(id: &Value) -> String {
    id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_notifications_and_junk() {
        let req = parse(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"a":1}}"#).expect("request");
        assert_eq!(req, Incoming::Request { id: json!(3), method: "tools/list".into(), params: json!({"a": 1}) });
        let note = parse(r#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#).expect("notification");
        assert_eq!(note, Incoming::Notification { method: "notifications/cancelled".into(), params: Value::Null });
        for (line, code) in [
            ("{", PARSE_ERROR),
            ("[]", INVALID_REQUEST),
            ("3", INVALID_REQUEST),
            (r#"{"id":1,"method":"x"}"#, INVALID_REQUEST),
            (r#"{"jsonrpc":"2.0","id":1}"#, INVALID_REQUEST),
            (r#"{"jsonrpc":"2.0","id":true,"method":"x"}"#, INVALID_REQUEST),
            (r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, INVALID_REQUEST),
        ] {
            assert_eq!(parse(line).expect_err(line).code, code, "{line}");
        }
    }

    #[test]
    fn what_a_request_says_about_its_client() {
        let params = json!({"_meta": {META_VERSION: MODERN, META_CLIENT_CAPS: {"extensions": {EXT_TASKS: {}}}}});
        assert_eq!(requested_version(&params), Some(MODERN));
        assert_eq!(requested_version(&json!({})), None);
        let ctx = Ctx { era: Era::Modern, version: MODERN, caps: meta(&params)[META_CLIENT_CAPS].clone(), progress_token: None };
        assert!(ctx.has_extension(EXT_TASKS) && !ctx.has_extension(EXT_UI) && ctx.modern());
    }

    #[test]
    fn errors_carry_the_spec_shapes() {
        let e = unsupported_version("1900-01-01").to_json();
        assert_eq!(e["code"], -32022);
        assert_eq!(e["data"]["supported"], json!(["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"]));
        assert_eq!(e["data"]["requested"], "1900-01-01");
        let e = missing_extension(EXT_TASKS).to_json();
        assert_eq!((e["code"].as_i64(), &e["data"]["requiredCapabilities"]["extensions"][EXT_TASKS]), (Some(-32021), &json!({})));
        let not_found = RpcError::invalid_params("Resource not found");
        assert_eq!(for_era(not_found.clone(), Era::Modern).code, INVALID_PARAMS);
        assert_eq!(for_era(not_found, Era::Legacy).code, RESOURCE_NOT_FOUND_LEGACY);
        let other = RpcError::invalid_params("`uri` is required");
        assert_eq!(for_era(other, Era::Legacy).code, INVALID_PARAMS, "only not-found changes");
        assert_eq!(id_key(&json!("1")), "\"1\"");
        assert_ne!(id_key(&json!(1)), id_key(&json!("1")));
    }
}
