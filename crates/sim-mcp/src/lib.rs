//! simee's MCP server: an agent can do everything simee can, over stdio (`simee mcp`) or MCP's
//! Streamable HTTP transport (`simee mcp --http`, [`http`]).
//!
//! - **Tools**: one per method in `sim-api`'s table, generated from it ([`tools`]), plus
//!   `device_view`, in toolsets a client picks (`SIMEE_MCP_TOOLSETS`; `core` by default).
//! - **Resources**: devices, runtimes and form factors, each device's info, screen, screenshot,
//!   apps and (iOS) logs and processes, and an app's preferences ([`resources`]), with change
//!   notifications for any of them, throttled by kind ([`watch`]).
//! - **Tasks**: boot, create, install and the other long methods as pollable, cancellable tasks
//!   ([`tasks`]).
//! - **Prompts**: common workflows (boot and install, reproduce a bug, audit contrast, find
//!   slowness, test across form factors) naming the tools to call and their toolsets ([`prompts`]).
//! - **App**: a live, clickable view of a device, as an MCP App ([`app`]).
//! - **Safety**: destructive tools need the user's confirmation ([`safety`]), a policy says which
//!   devices and toolsets a client may use and whether it may change anything ([`policy`]), boots go
//!   through the host's admission, and every call is audited ([`audit`]): docs/mcp-safety.md.
//!
//! It speaks the current revision (2026-07-28: stateless, `server/discover`, per-request `_meta`)
//! and answers `initialize` for clients of the revisions before it ([`rpc`], [`server`]). The
//! protocol layer is hand-written, over `serde_json`: see docs/mcp.md for why. Both transports feed
//! the same [`Server`].

pub mod app;
pub mod audit;
pub mod backend;
pub mod http;
pub mod policy;
pub mod prompts;
pub mod resources;
pub mod rpc;
pub mod safety;
pub mod server;
pub mod tasks;
pub mod tools;
pub mod watch;

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

pub use backend::{ApiBackend, Backend};
pub use policy::Policy;
pub use safety::Safety;
pub use server::Server;
pub use tools::Toolsets;

/// A sink that writes each line to stdout, whole, under the stream's lock.
pub fn stdout_sink() -> tasks::Sink {
    Arc::new(|line: String| {
        let mut out = std::io::stdout().lock();
        // A closed stdout means the client is gone; its stdin closing ends the server.
        let _ = out.write_all(line.as_bytes()).and_then(|()| out.write_all(b"\n")).and_then(|()| out.flush());
    })
}

/// How long to wait for requests in flight once the client closes its end.
const DRAIN: Duration = Duration::from_secs(10);

/// Serves one client on stdin and stdout until stdin closes, with the toolsets
/// `SIMEE_MCP_TOOLSETS` names (default `core`) and the subscription pace `SIMEE_MCP_SCREEN_FPS`
/// sets. Nothing else may be written to stdout.
pub fn serve_stdio(backend: Arc<dyn Backend>) -> std::io::Result<()> {
    let invalid = |e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e);
    let toolsets = Toolsets::from_env().map_err(invalid)?;
    let timing = server::Timing::from_env().map_err(invalid)?;
    serve_with(backend, std::io::stdin().lock(), stdout_sink(), toolsets, timing)
}

/// Serves the lines of `input` to completion, answering through `sink`.
pub fn serve(backend: Arc<dyn Backend>, input: impl BufRead, sink: tasks::Sink, toolsets: Toolsets) -> std::io::Result<()> {
    serve_with(backend, input, sink, toolsets, server::Timing::default())
}

/// [`serve`] with `timing`. The server, and with it every subscription, ends when `input` does.
pub fn serve_with(
    backend: Arc<dyn Backend>,
    mut input: impl BufRead,
    sink: tasks::Sink,
    toolsets: Toolsets,
    timing: server::Timing,
) -> std::io::Result<()> {
    let server = Server::with_timing(backend, sink, timing, toolsets);
    // The client's responses to a question arrive on this same stream.
    server.ask_clients_to_confirm(server::ELICIT_TIMEOUT);
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        if !text.trim().is_empty() {
            server.handle_line(text.trim_end());
        }
    }
    server.drain(DRAIN);
    Ok(())
}
