//! The device view: an MCP App (a `ui://` resource a host renders in a sandboxed frame) that shows
//! a device's screen and sends taps, swipes, text and buttons back through the same tools an agent
//! calls. Its tool is [`crate::tools::VIEW_TOOL`].

use serde_json::{Value, json};

/// The view's resource URI, named by the view tool's `_meta.ui.resourceUri`.
pub const URI: &str = "ui://simee/device";
/// The content type MCP Apps hosts render.
pub const MIME: &str = "text/html;profile=mcp-app";

/// The page. It speaks the Apps postMessage protocol to its host and needs nothing from the network.
const HTML: &str = include_str!("app.html");

/// The entry in `resources/list`.
pub fn listing() -> Value {
    json!({"uri": URI, "name": "device-view", "title": "Device view", "mimeType": MIME,
        "description": "A device's live screen with tap, swipe, type and button controls: open it with the `device_view` tool."})
}

/// The `resources/read` contents. No external origins: the page loads nothing, and the frames it
/// shows arrive as `data:` URLs through the host.
pub fn contents() -> Value {
    json!({"uri": URI, "mimeType": MIME, "text": HTML, "_meta": {"ui": {"csp": {}, "prefersBorder": true}}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_speaks_the_apps_protocol_and_the_tools_it_calls_exist() {
        let page = contents()["text"].as_str().expect("page").to_owned();
        for method in [
            "ui/initialize",
            "ui/notifications/initialized",
            "ui/notifications/tool-input",
            "ui/notifications/tool-result",
            "ui/resource-teardown",
            "tools/call",
        ] {
            assert!(page.contains(method), "{method}");
        }
        // Every tool the page calls is a tool this server has.
        for tool in ["screenshot", "tap", "swipe", "type", "button"] {
            assert!(page.contains(&format!("'{tool}'")) && crate::tools::exists(tool), "{tool}");
            assert!(crate::tools::Toolsets::default().contains(tool), "{tool} is in the default toolset, beside the view");
        }
        assert!(!page.contains("http://") && !page.contains("https://"), "no external origins");
        assert_eq!(contents()["mimeType"], MIME);
        assert!(URI.starts_with("ui://"));
        assert_eq!(listing()["uri"], URI);
    }
}
