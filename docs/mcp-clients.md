# MCP clients

Connect an MCP client to `simee mcp` (the stdio server in [mcp.md](mcp.md)) with one command:

```bash
simee mcp install claude-code        # write the entry into the client's config
simee mcp install cursor --dry-run   # print the file it would write, change nothing
simee mcp config vscode              # print the snippet and the file it belongs in
```

Clients: `claude-code`, `claude-desktop`, `cursor`, `vscode`, `windsurf`, `codex`, `gemini`.

`install` merges one entry into the client's file. Other servers, their order and (for Codex's TOML)
comments stay as they are. It refuses to replace an existing entry of the same name without
`--force`; an identical entry is left alone. A file that isn't plain JSON (comments, trailing
commas) is refused untouched instead of rewritten. The write is atomic (a temporary file in the
same directory, then a rename), keeps the original file's permissions, and writes through a
symlink to the file it points to; a symlink to a missing file is refused rather than replaced. If
another program rewrites the file while `install` works (Claude Code updates `~/.claude.json`
itself), `install` merges again from the new content once, then refuses. A new file is created
private (`0600`). Restart the client afterwards.

| Option | Meaning |
| --- | --- |
| `--scope user\|project` | The client's per-user file (default) or the file in the current directory |
| `--name NAME` | The server's name in the client's config (default `simee`) |
| `--toolsets LIST` | Sets `SIMEE_MCP_TOOLSETS` for the server, e.g. `core,debug` (validated; see [Toolsets](mcp.md#toolsets)) |
| `--env KEY=VALUE` | More environment for the server (repeatable) |
| `--command PATH` (alias `--bin`) | The binary the client starts. Default: the path you ran `simee` as, when that is a symlink to this binary (a Homebrew `bin/simee` outlives `brew upgrade`), else this binary's own path. Always absolute |
| `--dry-run`, `--force` | `install` only |

The command is an absolute path because GUI clients (Claude Desktop, VS Code, Cursor) don't start
with your shell's `PATH`.

## Where each client reads its config

| Client | `--scope user` | `--scope project` | Key | Docs |
| --- | --- | --- | --- | --- |
| Claude Code | `~/.claude.json` | `.mcp.json` | `mcpServers` | [Claude Code MCP][cc] |
| Claude Desktop | macOS `~/Library/Application Support/Claude/claude_desktop_config.json`; Windows `%APPDATA%\Claude\claude_desktop_config.json`; Linux `$XDG_CONFIG_HOME` or `~/.config`, then `Claude/claude_desktop_config.json` | none | `mcpServers` | [Connect to local servers][local] |
| Cursor | `~/.cursor/mcp.json` | `.cursor/mcp.json` | `mcpServers` | [Cursor MCP][cursor] |
| VS Code | macOS `~/Library/Application Support/Code/User/mcp.json`; Windows `%APPDATA%\Code\User\mcp.json`; Linux `~/.config/Code/User/mcp.json` | `.vscode/mcp.json` | `servers` | [VS Code MCP][vscode] |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | none | `mcpServers` | [Windsurf MCP][windsurf] |
| Codex | `$CODEX_HOME/config.toml`, else `~/.codex/config.toml` | `.codex/config.toml` (trusted projects) | `[mcp_servers.NAME]` | [Codex MCP][codex] |
| Gemini CLI | `~/.gemini/settings.json` | `.gemini/settings.json` | `mcpServers` | [Gemini CLI MCP][gemini] |

Claude Code's `~/.claude.json` is also where its local and user scopes live; `install` adds the
top-level `mcpServers` entry, which Claude Code treats as user scope. Claude Code asks for approval
before using a project `.mcp.json` server the first time.

Claude Desktop's Linux path is the one the MCP docs give; Anthropic ships Claude Desktop for macOS
and Windows, so treat Linux as unverified. The VS Code and Windsurf paths are not on the pages cited (VS Code's page defers to its
configuration reference); they are the clients' usual locations and were not checked by running
each client. The table's per-OS entries are covered by unit tests.

## Snippets

`simee mcp config <client>` prints the same content under a `# Client: path` line, with this
machine's binary path (and the arrays spread over several lines; they are shortened here). With
`--toolsets core,debug`, each gets `SIMEE_MCP_TOOLSETS`:

Claude Code and Cursor (`type` is part of their stdio entries):

```json
{
  "mcpServers": {
    "simee": {
      "type": "stdio",
      "command": "/usr/local/bin/simee",
      "args": ["mcp"],
      "env": { "SIMEE_MCP_TOOLSETS": "core,debug" }
    }
  }
}
```

Claude Desktop, Windsurf and Gemini CLI (same, without `type`):

```json
{
  "mcpServers": {
    "simee": {
      "command": "/usr/local/bin/simee",
      "args": ["mcp"],
      "env": { "SIMEE_MCP_TOOLSETS": "core,debug" }
    }
  }
}
```

VS Code (`servers`, not `mcpServers`):

```json
{
  "servers": {
    "simee": {
      "type": "stdio",
      "command": "/usr/local/bin/simee",
      "args": ["mcp"],
      "env": { "SIMEE_MCP_TOOLSETS": "core,debug" }
    }
  }
}
```

Codex:

```toml
[mcp_servers.simee]
command = "/usr/local/bin/simee"
args = ["mcp"]

[mcp_servers.simee.env]
SIMEE_MCP_TOOLSETS = "core,debug"
```

Claude Code can also add it itself: `claude mcp add simee -- simee mcp`.

HTTP: `install` and `config` write the stdio entry only. For `simee mcp --http` (a bearer token
per request, see [mcp.md](mcp.md#over-http)), put the client's HTTP entry by hand; writing it with
the token is not done yet.

## Compatibility

What each client's own documentation says it supports of simee's MCP features
([mcp.md](mcp.md)). "Not documented" means the page cited doesn't mention it, not that it is
absent; simee doesn't depend on any one cell, because tools work everywhere and the rest is
additive. Checked 2026-11-28 against the pages linked above.

| Client | Tools | Resources | Prompts | Tasks | App (`device_view`) | Elicitation | Roots |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Claude Code | yes | yes (`resources/list`, `list_changed`) | yes (`prompts/list`) | yes, v2.1.304+ | not documented | yes | yes |
| Claude Desktop | yes | not documented | not documented | not documented | not documented | not documented | not documented |
| Cursor | yes | yes | yes | not documented | yes (MCP Apps) | yes | yes |
| VS Code | yes | not documented | not documented | not documented | yes, experimental | yes | not documented |
| Windsurf | docs moved; not rechecked | not documented | not documented | not documented | not documented | not documented | not documented |
| Codex | yes (allow and deny lists) | not documented | not documented | not documented | not documented | not documented | not documented |
| Gemini CLI | yes | yes (`@server://…`) | yes (slash commands) | not documented | not documented | not documented | not documented |

Notes from the sources:

- Claude Code: tasks "require v2.1.304 or later" and only some tool calls become tasks; it answers
  `roots/list` with the launch directory plus extra working directories ([Claude Code MCP][cc]).
- Codex reads the server's `instructions` at initialization ([Codex MCP][codex]).
- Gemini CLI names tools `mcp_{server}_{tool}`; keep the server name free of underscores
  ([Gemini CLI MCP][gemini]).
- Windsurf's docs now sit under Devin's documentation, whose MCP page covers a different product;
  the config path above is the long-standing one and was not re-confirmed against a current page.
- `simee`'s default `core` toolset keeps the tool count modest for clients that cap or search
  tools; widen it with `--toolsets` ([Toolsets](mcp.md#toolsets)).

## Check the connection with MCP Inspector

[MCP Inspector][inspector] needs Node.js and runs through `npx`:

```bash
# Web UI: prints a URL with a per-launch token
npx @modelcontextprotocol/inspector simee mcp

# Scriptable: list the tools and exit
npx @modelcontextprotocol/inspector --cli simee mcp --method tools/list
```

The command after the Inspector's own flags is the one a client would start, so a working
Inspector session is the same server a client gets. Environment goes in the Inspector's server
settings (for example `SIMEE_MCP_TOOLSETS`), or set it in the shell that runs `npx`. The Inspector
keeps stdio `env:` values in the OS keychain or, without one, in `~/.mcp-inspector/secrets.json`;
don't put the HTTP bearer token there on a shared machine.

If a client doesn't show simee: run `simee mcp config <client>` to see which file it reads, check
that the `command` path exists, restart the client, and look at its MCP log (Claude Desktop:
`~/Library/Logs/Claude/mcp-server-simee.log` on macOS).

[cc]: https://code.claude.com/docs/en/mcp
[local]: https://modelcontextprotocol.io/docs/develop/connect-local-servers
[cursor]: https://cursor.com/docs/context/mcp
[vscode]: https://code.visualstudio.com/docs/copilot/customization/mcp-servers
[windsurf]: https://docs.windsurf.com/windsurf/cascade/mcp
[codex]: https://developers.openai.com/codex/mcp
[gemini]: https://geminicli.com/docs/tools/mcp-server/
[inspector]: https://modelcontextprotocol.io/docs/tools/inspector
