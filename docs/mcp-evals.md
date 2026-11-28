# MCP evals

The eval suite measures how well an agent can drive simee over MCP ([mcp.md](mcp.md)): for each
task, whether it reaches its end state, how many tool calls it takes, and what it costs the agent's
context in bytes and estimated tokens. It runs in CI against a fake backend, with no devices, network
or model, so its numbers are the same on every run. A change to a tool's description or schema shows
up in the table as a change in tokens, and fails the suite if it grows the default tool list more
than 10%.

```bash
cargo test -p simee-mcp --test evals          # what CI runs; the table is in target/mcp-evals/results.md
scripts/mcp-evals.sh                          # the same, then prints the table
scripts/mcp-evals.sh --update-baseline        # rewrite the baseline (commit it with the change that moved it)
scripts/mcp-evals.sh --agent '<command>'      # also hand each goal to an agent (live mode; never in CI)
```

## What a task is

Each task is a JSON file in `crates/sim-mcp/tests/evals/`, named after the task:

| Field | What it holds |
| --- | --- |
| `goal` | What the user asks, in a sentence: what a live agent gets |
| `toolsets` | The toolsets it needs, as `SIMEE_MCP_TOOLSETS` spells them, starting with `core` (`core,input` for `fold`) |
| `prompt` | For a task that follows one of the server's prompts: `{name, arguments}` for `prompts/get` |
| `world` | The fake backend's starting state: `devices` (udid, name, platform, form factor, state, and optionally `apps`, `foreground`, `appearance`, `orientation`, `posture`, `logs`), `packages` (host paths `install` takes and the bundle id each installs), `crash` (a tap region that crashes an app, the log line and the crash report it leaves), `icons` (home-screen tap regions that launch an app), `contrast` (per app, per appearance, the findings `contrast_check` reports) and `processes` (per device, what `processes` lists) |
| `plan` | The reference plan: the tool calls a competent agent makes, in order, as `{tool, args}` or, to read a resource instead, `{resource: uri}`, each with an optional `expect: {contains: [...]}` that its result's text must hold (what the agent reads before its next call: a tool result's text, or a resource's contents) |
| `end` | The end state: `booted` and `shutdown` devices; per device its `foreground` app (`null`: none, as after a crash), `appearance`, `orientation`, `posture` and `installed` apps; `screenshot_shows`, what the device's last screenshot must show; and `seen`, text the agent must have read in a result (the reference run) or written in its answer (live mode) |

The tasks, from #296 and one per prompt in [mcp.md](mcp.md#prompts):

| Task | Toolsets | What it checks |
| --- | --- | --- |
| `boot-tablet-open-settings` | `core` | Find the iOS tablet, boot it, launch Settings; the phone stays shut down |
| `install-and-launch` | `core` | Install an .apk on the booted Android phone, find its package, launch it |
| `dark-mode-screenshot` | `core` | Set dark mode and verify it from a screenshot |
| `crash-log-line` | `core` | Launch an app, tap what crashes it, return the log line that says why |
| `foldable-tabletop` | `core,input` | Rotate an Android foldable to landscape and fold it half open |
| `audit-contrast` | `core,observe` | Report each contrast failure, light and dark, with its ratio; the appearance is put back |
| `read-resources-find-heavy-process` | `core,debug` | Read `simee://devices` and the iPhone's `processes` resource, no tool, and name the process using the most CPU |
| `prompt-boot-and-install` | `core,lifecycle` | The prompt with no tablet to hand: create one, boot it, install and launch |
| `prompt-reproduce-bug` | `core,debug` | The prompt's steps with logs and screenshots, ending in the crash line and report |
| `prompt-audit-contrast` | `core,observe` | The prompt from the home screen: open the app, then check it in both appearances |
| `prompt-find-slowness` | `core,host,debug` | The prompt down to the heavy process's memory and leaks |
| `prompt-across-form-factors` | `core,lifecycle` | The prompt for phone, tablet and auto on iOS: skip auto, create the tablet, boot both, install, launch, screenshot, shut down |

A prompt task also fetches its prompt and fails unless the prompt says this server serves every
tool it names, and unless the plan calls every tool the prompt's text names (the tools in
backticks), so a prompt and its task can't drift apart. Every prompt in `prompts.rs` must have a
task: the suite fails, naming the file to add, when one doesn't.

To add a task, write its file and run `scripts/mcp-evals.sh`; the table shows it as `new` against
the baseline until the baseline is rewritten. The fake answers the methods the tasks use (listing,
creating, booting and shutting down devices; installing and launching apps; the screen, input,
appearance, rotation, the hinge, logs, crash reports, contrast, the host, usage, processes and the
memory tools) and refuses the rest with "the eval fake doesn't model"; a task that needs another
grows the fake in `tests/evals.rs`. Its screenshots are PNG headers of the screen's real size with
a text chunk saying what is on the screen (the device, the app in front, appearance, orientation,
posture), which is what `screenshot_shows` reads.

## How it runs

For each task, under two settings, the test starts the server's stdio loop (`simee_mcp::serve`, what
`simee mcp` runs) on in-memory pipes with the task's world as its backend, then plays the plan as a
client of the current revision would: `server/discover`, `tools/list`, `prompts/get` for a prompt
task, then each `tools/call`, one at a time, waiting for each response. A step fails the task when
the server refuses it (a tool outside the setting's toolsets, a bad argument), when the tool reports
an error, or when its result lacks what `expect` names. After the plan, the world is checked against
`end`.

- **`core`** serves the task's own `toolsets`: what its user would set `SIMEE_MCP_TOOLSETS` to.
- **`all`** serves every tool, so the table shows what an agent pays for a server of everything.

## Reading the table

`target/mcp-evals/results.md` (also printed with `-- --nocapture`, and appended to
`$GITHUB_STEP_SUMMARY` in a GitHub Actions job) starts with the two tool-list sizes and each
setting's success count, then a row per task and setting:

| Column | Meaning |
| --- | --- |
| Calls | `tools/call` and `resources/read` requests in the plan |
| Bytes out, Bytes in | The JSON text of every message the client sent, and of every one the server sent (responses and any notification before them), including `server/discover` and `tools/list` |
| tools/list ≈tok | The exact `tools/list` response for that setting, in bytes / 4 |
| Messages ≈tok | Every other message, in bytes / 4, except that an image's base64 counts as the image: width × height / 750, after scaling it to fit 1568 pixels on its long edge and about 1.15 megapixels, as vision models commonly resize. A real screenshot's base64 is far longer than what a model is charged for it |
| Total ≈tok | The tool list once plus every message: what the run put in the agent's context. A real agent re-reads the tool list on every turn, so the tool list's share of a long session is larger than this |
| vs baseline | The total's change against `baseline.json`'s number for that task and setting (`new`: it has none) |
| ms | Wall time of the run, on this host; not compared |

Tokens are estimates (bytes / 4 is near what tokenizers average on JSON, and differs by model); use
them to compare runs, not as a bill. The bytes are exact.

## The baseline

`crates/sim-mcp/tests/evals/baseline.json` holds the `core` and `all` `tools/list` sizes in bytes
and each task's total tokens per setting. The suite fails when the default (`core`) `tools/list`
grows more than 10% over it: that list is in every agent's context on every turn. The failure names
the size and the growth. Trim the descriptions or schemas, or, when the growth is worth it, rewrite
the baseline (`SIMEE_EVALS_UPDATE_BASELINE=1`, or `scripts/mcp-evals.sh --update-baseline`) and
commit it with the change, so the diff shows the cost. The tasks' tokens and the `all` size are only
compared in the table. The tool list itself is pinned byte for byte in
`crates/sim-mcp/src/snapshots/tools-list-core.json` ([mcp.md](mcp.md#toolsets)); the baseline
lets it change within a budget instead.

## Live mode

`SIMEE_EVALS_AGENT=<command>` also runs each task's goal through an agent (the `live_agent` test,
which does nothing without it). It is not run in CI, and it costs whatever the agent's model costs.

```bash
SIMEE_EVALS_AGENT='./my-agent.sh' cargo test -p simee-mcp --test evals live_agent -- --nocapture
scripts/mcp-evals.sh --agent './my-agent.sh'
```

For each task, the test serves the task's world over Streamable HTTP on a loopback port with no
token (`simee_mcp::http`, as `simee mcp --http --no-auth` would), with the task's toolsets, and runs
the command with `sh -c` and these variables:

| Variable | Value |
| --- | --- |
| `SIMEE_EVALS_MCP_URL` | The server's endpoint, `http://127.0.0.1:<port>/mcp` |
| `SIMEE_EVALS_GOAL` | The task's goal |
| `SIMEE_EVALS_PROMPT` | For a prompt task, `{"name": ..., "arguments": {...}}` as JSON (the agent may fetch it with `prompts/get`); else empty |
| `SIMEE_EVALS_TASK` | The task's name |
| `SIMEE_EVALS_TOOLSETS` | The toolsets the server serves |

The agent connects to the URL as an MCP server, works toward the goal, and prints its answer on
stdout. When it exits (or after `SIMEE_EVALS_TIMEOUT_S`, 600 by default, when it is killed), the
world is checked against the task's `end`, with `seen` read from what the agent printed. A last
line `simee-evals-tokens: <n>` on stdout is recorded as the tokens it used. The table goes to
`target/mcp-evals/live.md`: per task, success, the backend calls it made, its tokens and its time,
and why it failed. Live results vary from run to run, so the test reports them and doesn't fail on
them.

A wrapper for an agent CLI writes an MCP config naming `$SIMEE_EVALS_MCP_URL` as an HTTP server and
passes `$SIMEE_EVALS_GOAL` as the prompt; see the agent's own documentation for its flags. Real
devices instead of the fake (the nightly run #296 describes) are not built yet: they need the tasks'
worlds set up on a device and their end states read back from one.
