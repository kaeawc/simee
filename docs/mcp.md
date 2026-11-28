# MCP server

`simee mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server on stdin and
stdout (or, with `--http`, on MCP's Streamable HTTP transport), so an agent can do everything simee can do: create, boot and drive iOS and Android devices,
look at their screens, read their logs, and watch them change.

| MCP feature | What simee gives it |
| --- | --- |
| Tools | One per method in `sim-api`'s table, generated from it, and `device_view`, with output schemas and annotations, in toolsets a client picks (`core` by default) |
| Resources | Devices, runtimes, form factors, camera sources, each device's info, screen, screenshot, apps, logs and processes, and an app's preferences by template |
| Subscriptions | Change notifications for those resources and for the resource list, throttled by kind, by re-reading them only while subscribed |
| Prompts | Common workflows (boot and install, reproduce a bug, audit contrast, find slowness, test across form factors) that name the tools to call and their toolsets |
| Tasks | `boot`, `boot_many`, `create`, `clone`, `install`, `settle`, `erase`, `shutdown`, `export`, `import`, `mem_leaks`, `mem_heap` as pollable, cancellable tasks |
| App | `device_view`: a device's screen with tap, swipe, type and button controls, in the conversation |

The default transport is stdio: nothing on the network can reach it. `simee mcp --http` serves the
same server over HTTP for agents in other processes, containers or machines ([Over HTTP](#over-http)).
The server runs in its user account, with the same reach as `simee` itself.

## Set it up

Claude Code, in the project's `.mcp.json` (or `claude mcp add simee -- simee mcp`):

```json
{
  "mcpServers": {
    "simee": {
      "command": "simee",
      "args": ["mcp"]
    }
  }
}
```

Give `command` the full path of the binary if `simee` isn't on the agent's `PATH`
(`cargo run -p simee -- mcp` works too). The SDK and AVD home come from the environment
(`ANDROID_HOME`, `ANDROID_AVD_HOME`), as for the CLI; add them under `"env"` to pin them.

To look at the protocol by hand, one line per message:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}' | simee mcp
```

## Protocol

The server speaks the current revision, [2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28):
stateless, with no handshake. Every request names its version and carries its client's capabilities
in `_meta` (`io.modelcontextprotocol/protocolVersion`, `io.modelcontextprotocol/clientCapabilities`);
`server/discover` returns what the server offers; results say `resultType`, and the list and read
results say how long they may be cached (`ttlMs`, `cacheScope`).

A client of an earlier revision (2025-11-25, 2025-06-18, 2025-03-26) opens with `initialize` and is
served in that shape: no `_meta`, results without `resultType` or cache hints, `structuredContent`
as an object, `resources/subscribe` for change notifications, and a long method blocks (with
`notifications/progress` if the request has a progress token) instead of becoming a task. The older
revisions' own experimental tasks (`tasks/result`, `tasks/list`) are not implemented.

Extensions: `io.modelcontextprotocol/tasks` is advertised by the server and used for a client that
declares it; `io.modelcontextprotocol/ui` (MCP Apps) is how the view reaches a host that renders
apps, and costs a host that doesn't nothing, since the view tool also answers with text.

Stdout carries protocol messages and nothing else; diagnostics go to stderr. The server exits when
stdin closes, after answering the requests it has in flight (waiting at most 10 s).

### Why the protocol layer is hand-written

The official Rust SDK (`rmcp`, 3.11) implements this revision and its Tasks extension, and was
considered first. It is not used because of what it costs this workspace and what it gives back:

- It is async on `tokio`; everything here, from `sim-api`'s sessions to the display clients, is
  blocking and thread-per-connection, and an iOS session's service connection must stay on the
  thread that made it. Each call would cross `spawn_blocking` for nothing.
- Its dependency tree is 75 crates (`cargo tree -e normal`, with `default-features = false` and
  only `server` and `transport-io`) and an empty program that depends on it builds in 41 s on two jobs;
  the whole workspace's `Cargo.lock` has 140 packages. The hand-written layer adds none: `sim-mcp`
  uses `serde_json` and `sim-api` ([benchmarks](benchmarks.md#mcp-server)).
- The revision is small now that it is stateless: a request, a result, ten methods. The parts
  that need care (`_meta` handling, the task table, `subscriptions/listen` ordering) are
  specified closely, and the tests here check the shapes the specification gives. The SDK is
  moving fast (3.6 to 3.11 were released between 2026-10-20 and 2026-11-17), so depending on it
  means a dependency to keep bumping.

If the protocol surface grows past what is easy to keep right by hand (sampling, elicitation,
completions), moving this layer onto the SDK is the thing to revisit; the tools, resources and tasks
are written against a `Backend` trait and `serve` function, not against the transport.

## Tools

Every method in `sim-api`'s table is a tool of the same name with the same parameters; `simee call
help` lists them. A method added to the table is a tool in the next build, without touching this
crate:

- The description is written for an agent, from the same text `help` and [the API reference](api.md)
  show, so the two can't drift: the method's description from the table, then what it does to the
  device (read-only, sets a value, acts, caution, destructive: from the annotations below), that
  coordinates are device pixels where it takes them, whether it is slow (a task), whether it
  pages, `Platforms: Android, iOS.` (where it works), its parameter list as written there and its
  toolset.
- The input schema is generated from the parameter list (`sim_api::input_schema`): names, which are
  required, choices (`display?(main|carplay)`), ranges (`level(0-100)`), defaults
  (`settle_foreground?(true)`), which platform reads a parameter (`iOS: device_type`), and its unit
  from its name (`_ms` milliseconds, `_s` seconds, `_pct` percent; `x`/`y` device pixels on `tap`,
  `swipe`, `touch`, `stylus` and `pointer`). A quoted single value (`profile?("slim")`) is the one
  value an optional parameter takes, with no default: leaving `profile` out applies none. A test
  checks every advertised `default` against what the dispatchers do without the key (the options
  each parses the parameters into, with the key and without), so a default the server doesn't apply
  can't be advertised. A parameter's type comes from its name, in `crates/sim-api/src/schema.rs`; a
  test lists any name that doesn't have one yet.
  JSON Schema can't say "one of `ticks` or `delta`" without `anyOf` at the top, which tool
  clients refuse, so such a group's members are each optional and their descriptions say which they
  go with; `required` holds what every platform needs. A test checks the lists against the
  dispatchers: the Android dispatcher is driven on running fake devices (a method it refuses before
  reading parameters doesn't count as probed, and fails the test), and the iOS arms' own required
  reads are scanned from their source, since that backend only builds on macOS.
- The output schema says what the result is (`sim_api::output_schema`, from a line per method in
  `crates/sim-api/src/outputs.rs`, which a test checks against the results of fake Android devices
  and the listings), and every result carries it as `structuredContent` beside the JSON text. A
  method with nothing to report answers `ok` and `{}`. For the current revision `structuredContent`
  is the result as it is (an array for `devices`); for 2025-11-25 and 2025-06-18 it is an object,
  with anything else under `result`, and the output schema says the same. 2025-03-26 has no output
  schemas, so its clients get none.
- Annotations follow what the method does, from one table in `crates/sim-api/src/catalog.rs` that
  classifies every method (a test fails for one that isn't listed). Read-only means it changes
  nothing on the device or the host: listings, reads, `screenshot`. Settings are idempotent;
  `exec`, `launch`, `push`, `network_shape`, `console`, `kill`, `export`, `audio_capture_start` and
  `stream_start` are marked destructive (they run code, reboot or restart things, write a file, or
  send the screen elsewhere), and so are
  `delete`, `erase`, `shutdown`, `rm`, `uninstall`, `terminate`, `clear_container`,
  `reset_keychain`, `write`, `defaults_write`, `prefs_write`, `prefs_delete`, `prefs_batch` and
  the other overwrites. `openWorldHint` is true for `open_url` (the device may fetch the URL
  from anywhere) and `stream_start` (it publishes to a WHIP server you name). A method nobody has classified gets the specification's defaults (writes, may
  destroy), not a claim of safety.
- `processes`, `logs`, `connections` and `devices_usage` (its `devices`) page: with `limit` (1 to
  1000) the result is a page, `{items, next_cursor, total}` (`devices_usage` keeps its shape, with
  `devices` cut to the page and `next_cursor` and `total` beside it); pass `next_cursor` back as
  `cursor` for the next page, until it is `null`. The pages after the first come from a snapshot
  of the first call's list, kept 10 minutes (4 at most, and 32 MiB of JSON in all: the oldest go first, and a first page that holds the whole list keeps none), so a process list or a log window doesn't
  shift between pages. Without `limit` or `cursor` the result is the whole list, as before.
- A failing method is a tool result with `isError: true` and its message, so the agent can correct
  itself; protocol errors are for unknown tools and malformed requests.
- `screenshot` returns an image block (and the size). It doesn't take the API's `path`: writing a
  file anywhere on the host isn't something a read-only tool may do, so the schema leaves it out
  and a call that passes it gets a tool error. Other results are JSON as text and as
  `structuredContent`.
- `contrast_check` is held to the same rule: no `path`, and each annotated PNG (one per appearance
  with `appearance: both`) is an image block, so the findings, a few lines of JSON, are the only
  text (each result's `image` is its place among the image blocks). It is marked as an action, not a read: with `appearance: both` it switches an iOS
  device's appearance and puts it back.

`device_view` is the one tool that is not an API method (see [the app](#the-device-view-app)).

### Toolsets

A hundred tools are more than most agents should choose from, so the server serves toolsets, named
in `SIMEE_MCP_TOOLSETS` (commas between names; `all` for every tool). Unset, it serves `core`:

| Toolset | Tools |
| --- | --- |
| `core` (the default) | `devices`, `boot`, `shutdown`, `info`, `screen`, `screenshot`, `tap`, `swipe`, `type`, `key`, `button`, `rotate`, `open_url`, `install`, `uninstall`, `launch`, `terminate`, `apps`, `logs`, `appearance`, `location`, `device_view` |
| `lifecycle` | Devices: `form_factors`, `devices`, `runtimes`, `device_types`, `create`, `delete`, `boot`, `boot_many`, `admission_check`, `settle`, `profile`, `clone`, `export`, `import`, `shutdown`, `erase`, `info` |
| `apps` | `open_url`, `install`, `uninstall`, `launch`, `terminate`, `apps`, `privacy`, `push` |
| `input` | `tap`, `swipe`, `touch`, `button`, `type`, `key`, `rotate`, `fold`, `xr`, `touchpad`, `stylus`, `rotary`, `watch_gesture`, `remote_swipe`, `pointer`, `biometric` |
| `observe` | `screen`, `screenshot`, `contrast_check`, `logs`, `device_view` |
| `settings` | `appearance`, `location`, `status_bar`, `clear_status_bar`, `content_size`, `increase_contrast`, `dynamic_island`, `backlight`, `keyboard_language`, `hardware_keyboard`, `memory_warning`, `notify`, `carplay`, `vehicle` |
| `debug` | `ca_debug`, `ca_debug_state`, `exec`, `processes`, `kill`, `connections`, `traffic`, `mem_leaks`, `mem_heap`, `mem_vmmap`, `mem_history`, `crash_reports`, `console` |
| `files` | `containers`, `ls`, `read`, `write`, `rm`, `du`, `clear_container`, `defaults_read`, `defaults_write`, `reset_keychain` |
| `prefs` | `prefs_stores`, `prefs_read`, `prefs_write`, `prefs_delete`, `prefs_batch`, `prefs_watch` |
| `network` | `network_shape`, `network_state`, `network_flows`, `network_clear`, `network_env` |
| `audio` | `audio_state`, `audio_volume`, `audio_route`, `audio_capture_start`, `audio_capture_stop`, `audio_mute` |
| `stream` | `stream_start`, `stream_status`, `stream_stop`, `streams` |
| `camera` | `camera_sources`, `camera_link`, `camera_unlink`, `camera_links` |
| `host` | `help`, `identity`, `host`, `devices_usage` |
| `all` | Every tool |

```json
{"mcpServers": {"simee": {"command": "simee", "args": ["mcp"], "env": {"SIMEE_MCP_TOOLSETS": "core,debug,prefs"}}}}
```

Every method is in one group (`sim_api::catalog::group`, a test fails for one that isn't), and
`core` is a list there too. `tools/list` holds what was picked; a call to a tool that isn't served
is refused (`-32602`) with the toolset that has it and the setting to add it, and never reaches the
API. An unknown toolset name stops `simee mcp` with the names there are. The default list is
pinned in `crates/sim-mcp/src/snapshots/tools-list-core.json` (`SIMEE_MCP_UPDATE_SNAPSHOT=1` rewrites
it), so a change to what most agents see shows up in review.

Not every method works on every device: the description of each says which platforms, and a call on
the wrong one fails with the platforms it works on. On a Linux host there are no iOS devices (and no
way to boot an Android one), so listings are empty or Android only.

The tools, with the platforms each works on (iOS-only methods fail on Android devices and the
reverse):

| Tools | Platforms |
| --- | --- |
| `help`, `identity`, `host`, `devices_usage`, `form_factors`, `devices`, `runtimes`, `create`, `delete`, `boot`, `boot_many`, `admission_check`, `clone`, `shutdown`, `screen`, `screenshot`, `contrast_check`, `tap`, `swipe`, `touch`, `button`, `type`, `key`, `open_url`, `install`, `uninstall`, `launch`, `terminate`, `apps`, `location`, `info`, `rotate`, `rotary`, `stylus`, `processes`, `kill`, `exec`, `settle`, `profile`, `device_types`, `device_view` | Android, iOS |
| `console`, `fold`, `xr`, `vehicle`, `touchpad`, `export`, `import` | Android (iOS bundles: the next step of #170) |
| `stream_start`, `stream_status`, `stream_stop`, `streams` | Android, iOS (macOS: VideoToolbox, both platforms; Linux: Android only, in a build with `--features openh264`) |
| `camera_sources` | Android, iOS (the host's cameras and the synthetic sources; needs no device) |
| `camera_link`, `camera_unlink`, `camera_links` | Android, iOS (iOS: device-wide shim injection, needs `simee serve`; see [camera.md](camera.md)) |
| `prefs_stores`, `prefs_read`, `prefs_write`, `prefs_delete`, `prefs_batch`, `prefs_watch` | Android, iOS (iOS is implemented; Android answers "not yet available") |
| `connections`, `traffic`, `network_shape`, `network_state`, `network_clear` | Android, iOS (Android: the emulator console's `network speed` and `network delay`, airplane mode for offline, and the guest's socket tables and per-uid counters over adb; loss, rules, proxy and inject are iOS's) |
| `erase`, `status_bar`, `clear_status_bar`, `increase_contrast`, `hardware_keyboard`, `memory_warning` | Android, iOS (Android: SystemUI's demo mode, settings and `am send-trim-memory` over adb; `erase` on the AVD's disks) |
| `containers`, `ls`, `read`, `write`, `rm`, `du`, `clear_container` | Android, iOS (Android: in an app's container, through `run-as` for a debuggable app, root otherwise; `adb root` only with `root: true`, `pm clear` only with `pm_clear: true`) |
| `appearance`, `privacy`, `push`, `content_size`, `dynamic_island`, `backlight`, `keyboard_language`, `ca_debug`, `ca_debug_state`, `reset_keychain`, `notify`, `biometric`, `logs`, `defaults_read`, `defaults_write`, `crash_reports`, `network_flows`, `network_env`, `audio_state`, `audio_volume`, `audio_route`, `audio_capture_start`, `audio_capture_stop`, `audio_mute`, `watch_gesture`, `remote_swipe`, `pointer`, `carplay` | iOS |
| `mem_leaks`, `mem_heap`, `mem_vmmap`, `mem_history` | iOS (the host's memory tools on one app; Android answers that they aren't available there) |

A test keeps this page honest: it fails if a tool isn't named here.

## Resources

| URI | Contents |
| --- | --- |
| `simee://devices` | Every device on both platforms, with its state (JSON) |
| `simee://runtimes` | iOS runtimes and Android system images |
| `simee://device-types` | iOS device types |
| `simee://form-factors` | The form-factor matrix, with inputs or why one is unsupported |
| `simee://camera/sources` | Every source a device camera can link to, and the host's camera permission (`camera_sources`) |
| `simee://devices/{device}` | The device's info (`info`): identity, form factor, sub-platform, profile, paths |
| `simee://devices/{device}/screen` | The screen now, a PNG shrunk so its longest side is at most 1024 px (a 1206×2622 iPhone is 402×874): what an agent reads (a booted device) |
| `simee://devices/{device}/screenshot` | The screen now, a PNG at the device's own size (a booted device) |
| `simee://devices/{device}/logs` | The newest 200 lines of the last 5 seconds of the unified log (`text/plain`; iOS) |
| `simee://devices/{device}/apps` | Installed bundle ids (iOS) |
| `simee://devices/{device}/processes` | The device's processes with CPU and memory (`processes`; iOS) |
| `simee://devices/{device}/prefs/{bundle_id}` | Every key of an app's preference stores, typed (`prefs_read`, read fresh) |
| `simee://devices/{device}/prefs/{bundle_id}/{store}` | One preference store of an app (a `store` id from `prefs_stores`) |
| `ui://simee/device` | The device view's page (`text/html;profile=mcp-app`) |

`{device}` is a UDID or an AVD name; `{bundle_id}` and `{store}` are as the `prefs_*` tools take
them. Each is percent-encoded as one path segment. `simee://device/{device}…` is read as
`simee://devices/{device}…`, and a subscription is announced under the spelling the client gave.
A resource is served only when the server serves the tool it reads through (`processes` is the
`debug` toolset's, `prefs_read` the `prefs` toolset's, `runtimes`, `device_types` and
`form_factors` the `lifecycle` toolset's, `camera_sources` the `camera` toolset's): a client of the
default `core` server can't read, list, template or subscribe to them, and a read says which
toolset has it. `resources/list` includes each device's resources that apply to it (screens for a booted device,
logs and processes for a booted iOS one, apps for an iOS one), and `resources/templates/list` all
eight templates. Preferences are templates only, since every app has some. Android has no log or
process resource (its console is the `console` tool) and no preferences yet (they answer as
not found, like the tools' "not yet available"). A device that doesn't exist is the
specification's "resource not found" (`-32602` for the current revision, `-32002` for the earlier
ones); one that isn't running is an internal error that says so.

`screen` is the one to subscribe to. A client that wants the device's own pixels reads
`screenshot`, the `screenshot` tool's answer unchanged. Neither takes a size.

**Subscriptions.** The API has nothing to subscribe to, so a subscription re-reads: while a client
is subscribed (`subscriptions/listen` with `resourceSubscriptions`, or `resources/subscribe` for an
older client), a watcher re-reads each subscribed resource at that kind's pace and sends
`notifications/resources/updated` when it differs from the last read. The pace is also the most
often it can be announced, so a burst of changes is one notification, and the client reads the
state at that moment:

| Resource | Re-read every | Counts as a change |
| --- | --- | --- |
| `screen`, `screenshot` | 1 s (`SIMEE_MCP_SCREEN_FPS`, above 0 and up to 30 frames a second) | The device's frame differs; an unchanged frame isn't shrunk or sent |
| `logs` | 1 s, through the device's log stream (the first read starts it; later reads spawn no `log show`) | A new newest line: lines aging out of the window say nothing, and a burst is one notification |
| `processes` | 2 s | A process starts or exits (its pid and path); CPU and memory moving don't count |
| `devices` and the resource list (`resourcesListChanged`, a server that serves `devices`) | 1 s | A device appears, disappears, or starts or stops running (`notifications/resources/list_changed`) |
| Everything else (`info`, `apps`, `prefs`, the catalogues) | 2 s | The bytes differ |

What is already there when a subscription starts is its baseline: nothing is sent for it. A
subscription added to a connection that has others keeps their baselines. A resource that can't
be read (a device that isn't running) counts as a state, so booting a device announces its screen.

**Cost.** A connection with no subscriptions has no thread and makes no read: reading, listing
and listing templates are the calls they ask for and no more. A `subscriptions/listen` is one thread (and an older client's
subscriptions together another, per connection or HTTP session), reading what it watches one
resource after another, and sleeping until the next read is due: 1 wake-up a second for a screen,
the list, or both, and no CPU a test can see
([benchmarks.md](benchmarks.md#mcp-idle-subscription)). The reads themselves are what costs:
a subscribed screen is one `screenshot` a second (more at a higher `SIMEE_MCP_SCREEN_FPS`), and
a subscribed iOS `logs` keeps the device's log stream running for as long as someone reads it (it
stops after 5 idle minutes) and reads its last 5 seconds from it, so only the first 5 seconds of
a subscription (while the stream is younger than its window) run `log show` on the device. Each
read is one period after the last finished, so a slow screenshot lowers the rate instead of
piling up. The thread stops when the client cancels the listen (`notifications/cancelled`),
unsubscribes its last resource, or its connection ends (stdin closes; the HTTP session is deleted
or expires), and no notification follows.

Subscribing to `simee://device/{device}/…` and `simee://devices/{device}/…` is one read, announced
under each spelling. An older client (`initialize`) has no way to ask for `list_changed`, so it is
not offered it and the device list isn't read for it; `subscriptions/listen` is where
`resourcesListChanged` is. The server never changes its tool list, so it doesn't offer
`toolsListChanged`.

Over HTTP every stream of notifications queues at most 256 of them for a client that reads
slowly: a session's `GET` stream, and the response stream of a `POST` (a `subscriptions/listen`'s
notifications, a long call's progress). A queue keeps only the latest update for each resource
(and for the list, for the request's progress, and for each task's state), so a screen announced
30 times a second never pushes out a logs or prefs update queued behind it. Past 256 different
things the oldest notification is dropped (each is only "something changed"; the client reads the
state when it next looks), so a stalled client never holds up the watcher. Two messages are never
dropped or merged: a `subscriptions/listen`'s acknowledgement, which comes first, and a request's
response, which comes after the notifications queued before it.

## Prompts

`prompts/list` and `prompts/get` serve workflows an agent (or its user, from a prompt menu) can
start with a few arguments. Each prompt's text is one user message that names the tools to call, in
order, with their arguments, and the end state to reach, then a line saying which toolset has each
tool, whether this server serves them, and if not, the `SIMEE_MCP_TOOLSETS` value to restart it
with (the agent can't change it itself). The texts are kept short, since they land in the agent's
context.

| Prompt | Arguments | Tools, in order | End state |
| --- | --- | --- | --- |
| `boot-and-install` | `platform` (`ios` or `android`), `form_factor`, `app_path` | `devices`; if none, `form_factors`, `runtimes`, `device_types` (iOS), `create`; `boot` with `admission: "wait"`; `install`, `apps`, `launch`; `screenshot` | A booted device of that form factor running the app in front |
| `reproduce-bug` | `device?`, `steps`, `capture` (`logs`, `recording` or `both`) | `devices` (no `device`); `screen`, `screenshot`; `logs` with `stream` (logs); `tap`, `swipe`, `type`, `key`, `button`, a `screenshot` after each step (recording); `logs`, `crash_reports` | Whether it reproduced, the steps as run, the screenshots and log lines |
| `audit-contrast` | `device?`, `screen_description?` | `devices` (no `device`); `screenshot`, then `tap`, `swipe`, `button` to reach the screen; `contrast_check` with `appearance: "both"` | Each finding with its element, box, colours and ratio, worst first |
| `find-slowness` | `device` | `host`, `devices_usage`, `processes`, then `mem_vmmap`, `mem_heap`, `mem_leaks` on the app's process (iOS) | The cause with its numbers: the host, the device's share, or a process |
| `across-form-factors` | `app_path`, `form_factors` (separated by commas) | `form_factors`, `devices`, `create`; `boot_many` with `admission: "wait"`; per device `install`, `apps`, `launch`, `screenshot`; `shutdown` when done | A screenshot per form factor with its layout problems |

- Arguments are strings, as MCP has them. `prompts/get` refuses (`-32602`) an unknown prompt,
  an argument the prompt doesn't take, a value that isn't a string or isn't one of its choices,
  and missing or blank required arguments (all of them, in `data.missing`).
- Every revision the server speaks has prompts; 2025-03-26 has no `title` on a prompt or its
  arguments, so its clients get none. The capability is `prompts: {listChanged: false}`: the
  prompts are fixed for a build.
- `prompts/list` pages by `cursor` and `nextCursor`, 50 to a page (so one page today); a cursor
  that isn't a previous page's `nextCursor` is `-32602`. Its result carries the same cache hints
  as `tools/list`.
- There is no screen recording over MCP (recording isn't an API method), so `reproduce-bug`'s
  `recording` is a screenshot after each step. There are no stream tools, so there is no prompt to
  stream a device.
- The prompts are a table in `crates/sim-mcp/src/prompts.rs`, each with the tools its text may
  name. Tests check that each is a tool (a renamed tool fails them), that the text names nothing
  else in backticks and every one of them in some set of arguments, and that each named tool is
  either served or named with the toolset and setting that serve it. The eval suite
  ([mcp-evals.md](mcp-evals.md)) plays each prompt's reference plan against the server and checks
  its end state.

## Tasks

A client that declares `io.modelcontextprotocol/tasks` gets a task, instead of a blocked call, for
`boot`, `boot_many`, `create`, `clone`, `install`, `settle`, `erase`, `shutdown`, `export`, `import`, `mem_leaks` and
`mem_heap` (seconds each; see their notes in docs/api.md): the result
has `resultType: "task"`, a `taskId`, `pollIntervalMs` (1 s) and `ttlMs` (an hour). The methods are
a list in `crates/sim-mcp/src/tools.rs`; every other tool answers inline.

- `tasks/get` returns the task: `working` (with `statusMessage` saying how long it has run and
  the call's last step, below), then `completed` with the tool's result in `result`, or `failed`
  with a JSON-RPC `error`. `failed` is for the server's own failures; a tool that returns
  `isError: true` completes, as the extension specifies.
- `notifications/tasks` carries the whole task when it ends, and at each step while it works, to a
  client that listened for its id (`subscriptions/listen` with `taskIds`).
- `tasks/cancel` (and `notifications/cancelled`, for a blocked call) is cooperative, as the
  extension allows: the task is `cancelled` at once, and the call stops at its next safe point,
  undoing what it had half done:
  - `boot_many` starts no more devices and shuts down, within a second, those it was booting that
    it had started: the ones it found off when their turn came and booted itself. A device that was
    running or booting when its turn came (another caller's, even one started after this call was
    admitted) is left as it was. Its result has the devices that finished, a reason for every
    other one and `cancelled: true`, and the cancelled task keeps it as its `result`: listeners
    hear the task again (`notifications/tasks`, still `cancelled`) once that result is in.
  - `boot` shuts down the device it was booting, if it started it (by the same rule).
  - `create`, `clone` and `import` delete the device they made, if the cancel came while they ran.
  - A call waiting for the host to admit its devices (`admission: "wait"`) stops waiting and starts
    nothing.
  - `install`, `erase`, `settle`, `shutdown`, `export` and the memory tools stop only before they
    start (an install after measuring its bundle): once running they can't be interrupted, finish
    in the background, and their result is discarded.

  The status message says which applies.

### Progress

A long call reports its steps as `sim-api` reaches them: `boot_many` the devices done of all
(`3 of 8 done`, as each finishes), `boot`, `create`, `clone` and the other long methods their phases
(`install`: checking, then installing with the bundle's size, 0 to 2). A client that gave a
progress token on a blocked call gets each step as `notifications/progress` with `progress` and
`total` in those units (devices, phases), at once; between steps a heartbeat every 2 s keeps it
informed (and its timeout reset) with a value partway to the next unit, so the value only
increases. A call that counts nothing, or whose first progress was a heartbeat, reports seconds
instead (`progress` is the seconds it has run, no `total`). Nothing is sent after the response. A
task's steps are its `statusMessage`. The older revisions get the same notifications.

`boot_many` boots the same way whether or not anyone watches it: its backend's window keeps
`concurrency` devices booting and starts the next as soon as any finishes, with the same ports,
shaper builds and settle timeouts. A hook in the window tells the call as each device finishes
(the step a watcher hears) and is asked before each start (how a cancel starts no more). Phases
inside a device's boot (settled), bytes for `install`, `export` and `import`, and stopping a
`settle` or a memory tool mid-run need hooks in the backends (`ios.rs`, `sim-android`,
`memtools.rs`) and are not here yet.
- A `boot`, `boot_many`, `create` or `clone` result carries `admission` (and `pacing`) in
  `structuredContent` only; the text is one line, such as `ok (admit)`, so it doesn't repeat them.
- At most 8 task calls run at once; a ninth is refused with a tool error until one ends. A
  cancelled task's call counts until it has stopped (an install until it ends), so cancelling
  makes room only as calls stop. Finished tasks are kept for an hour.
- `tasks/update` only acknowledges: nothing a simee task does needs the client's input.

`provision` (SDK downloads) and recording are not API methods, so they are not tools or tasks yet;
when they become methods they join the list above by a one-line change.

## The device view (app)

`device_view` (`device`, default `booted`) is an [MCP App](https://modelcontextprotocol.io/extensions/apps/overview):
its tool definition points at `ui://simee/device`, and a host that renders apps shows that page in a
sandboxed frame next to the conversation. The page:

- shows the device's screen, refreshed once a second while it is visible;
- sends a click as `tap`, a drag as `swipe` (in the device's pixels, mapped from where the pointer
  is on the picture), the text box as `type`, and the Home, Back, Recents and Lock buttons as
  `button`;
- does all of this by calling the same tools an agent calls, through the host, so it works on
  either platform and needs no connection of its own. A button the device's form factor lacks
  shows the tool's error in the status line.

The page is one file (`crates/sim-mcp/src/app.html`) with no external origins; it takes the host's
theme variables where it is given them. A host without app support sees the tool as an ordinary
one that returns the screen size. The frame rate is one screenshot a second; a live stream would
need a push channel the API doesn't have.

## Over HTTP

```bash
simee mcp --http                       # http://127.0.0.1:8377/mcp, loopback; token from the state directory
simee mcp --http 127.0.0.1:9000 --token-file ~/.config/simee/mcp-token   # loopback, a token file of your choice
simee mcp --http 0.0.0.0:8377 --tls-cert cert.pem --tls-key key.pem   # every interface, HTTPS (see TLS)
simee mcp --http 0.0.0.0:8377 --tls-self-signed --tls-name box.lan      # HTTPS, a development certificate
simee mcp --http 0.0.0.0:8377 --insecure-remote   # every interface, plain HTTP (see Security)
simee mcp --http --no-auth             # loopback, no token: any local user or process can connect
```

Every request carries `Authorization: Bearer <token>`. On its first run the server writes a random
token to its default token file and prints the file's path (never the token); later runs reuse it.

This is the Streamable HTTP transport: one endpoint, `/mcp`.

- `POST` carries one JSON-RPC message. A notification or a response is accepted with 202. A request
  is answered with `application/json`, or with a `text/event-stream` when something about it comes
  before its response (`notifications/progress` for a long call with a progress token, a
  `subscriptions/listen`'s acknowledgement and notifications); that stream ends with the response.
  A client that accepts only JSON gets the response alone.
- A client of the current revision (2026-07-28) is stateless: it needs no session, and its requests
  share one server (tasks are found by their unguessable ids). It cancels a request by closing its
  response.
- A client of an earlier revision opens with `initialize`, whose response carries an
  `Mcp-Session-Id`. Every request after it must name that session (400 without one, 404 for one
  that ended or expired); each session is its own server, as each stdio connection is, with its
  own negotiated revision, requests and subscriptions. `GET` opens the session's stream for what
  belongs to no request (`notifications/resources/updated`); `DELETE` ends the session.
  `notifications/cancelled` names one of the session's own requests.
- `MCP-Protocol-Version`, when sent, must be a revision the server speaks (400 otherwise) and agree
  with the request's `_meta` version.
- Request ids and progress tokens are replaced by ones unique to the process on the way in and put
  back on the way out, so clients that reuse ids don't see each other's answers. Every message goes
  through the same server code as stdio: the transport only carries it.
- Streams carry no event ids, so a dropped stream can't be resumed with `Last-Event-ID`; a long call
  is better made a task (Tasks extension) and polled.

### TLS

`simee mcp --http` serves HTTPS itself when asked, so a client on another machine can connect with
a token and nothing else in between. TLS is a cargo feature that is off by default (it builds
[rustls](https://github.com/rustls/rustls) and its crypto, ring, with the host's C compiler, and
nothing else on the system): build with `cargo build -p simee --features tls`. A build without it refuses the
flags below and says how to build it.

```bash
# A certificate you have: PEM files, the chain leaf first (Let's Encrypt's fullchain.pem works).
simee mcp --http 0.0.0.0:8377 --tls-cert fullchain.pem --tls-key privkey.pem

# A development certificate: made on first use, kept in the state directory, named for the
# addresses clients will use.
simee mcp --http 0.0.0.0:8377 --tls-self-signed --tls-name box.lan
```

- **What it speaks.** TLS 1.3 only (a TLS 1.2 client is refused), ALPN `http/1.1`, no client
  certificates, no 0-RTT. The key can be RSA, ECDSA (P-256 or P-384) or Ed25519; others are refused at start. The certificate and key are read
  once, at start: restart the server after renewing them.
- **Plaintext on a TLS port** is refused: the client gets a `400` in the clear that says this port
  serves HTTPS, and the connection closes. A bind that isn't loopback with neither TLS nor
  `--insecure-remote` (also spelled `--insecure`) is refused at start; with TLS it needs no
  acknowledgement. `--insecure-remote` is for plain HTTP, and TLS doesn't relax any other rule:
  the bearer token (below) is still required on every request, and `--no-auth` still only works on
  loopback.
- **The self-signed certificate** is ECDSA P-256, valid for two years, and names `localhost`,
  `127.0.0.1`, `::1`, the address the server binds when that is one address (not `0.0.0.0`) and
  each `--tls-name`. It lives in `mcp-tls.pem` in the state directory (with the token file's
  rules: a directory only you can write, a regular file you own, mode 0600, no symbolic link), and
  later runs reuse it, so the fingerprint stays the same. It is made again, and says so, when it
  was made for other names, has under 30 days left, or is damaged. A copy without the key,
  `mcp-tls-cert.pem`, is written beside it: that file is what to give a client. Never copy
  `mcp-tls.pem` off the machine: it holds the key.
- **Trusting it.** The server prints the certificate's SHA-256 fingerprint in the form
  `openssl x509 -fingerprint -sha256` prints, so a client can pin it or be checked against it:

  ```bash
  curl --cacert mcp-tls-cert.pem -H "Authorization: Bearer $(cat mcp-token)" https://box.lan:8377/mcp ...
  openssl s_client -connect box.lan:8377 </dev/null 2>/dev/null | openssl x509 -noout -fingerprint -sha256
  ```

  A client that connects to an address or name the certificate doesn't carry fails its name check
  (that is what `--tls-name` is for).
- **Limits.** The handshake runs on the connection's own thread, holds one of its slots and has
  the same deadline as a request head (`request_timeout` from the accept), so a client that stalls
  or trickles a handshake is cut off as one that trickles a request is, and TLS doesn't raise the
  connection caps. After it, a head or a body has one deadline however slowly its bytes arrive,
  even when they trickle inside one TLS record; bodies and heads are bounded and framing is as
  strict as over plain HTTP. A connection turned away for the limits can't be told so in TLS: it
  gets the `503` in the clear. A failed handshake is logged (at most once a second), with the
  client's alert when it sent one (`UnknownCA`: the client doesn't trust the certificate).
- **Cost.** On a kept-alive connection TLS adds nothing a test can see. A call on a new connection
  pays the handshake ([benchmarks](#tls-cost) below).

#### TLS cost

`https_call_overhead` (`cargo test --release -p simee-mcp --features tls https_call_overhead --
--ignored --nocapture`) times `identity` `tools/call`s against the server in the same process,
next to `http_call_overhead`'s plain rows. Medians of 5 rounds, two runs, a 4-vCPU Xeon @ 2.2 GHz
Linux VM (not an Apple silicon host), 2026-11-28:

| Call | Plain HTTP | HTTPS (TLS 1.3) |
| --- | --- | --- |
| Kept-alive connection, bearer token | 100 µs | 93-102 µs |
| New connection, one call (plain: no token) | 227 µs | 1.5-1.6 ms resuming the last session; 2.1 ms with a full handshake |

A client that keeps its connection pays the handshake once. Not measured: macOS, TLS under
concurrent load, a network between the two ends. These rows are not in
[benchmarks.md](benchmarks.md) yet.

### Security

- OAuth is not built in (a follow-up issue tracks it), and without `--features tls` neither is
  TLS. Over HTTPS the token is protected from the network. Without TLS, for access from another
  machine put the server behind a TLS-terminating reverse proxy (nginx, Caddy) on the same host,
  keep it bound to loopback, and keep the token on. A non-loopback bind without TLS serves plain
  HTTP, where the token crosses the network in the clear, so it is refused unless
  `--insecure-remote` acknowledges that (a trusted network only).
- Authentication is on by default, on every bind, loopback included: a request without a valid
  `Authorization: Bearer <token>` gets 401. Tokens come from `--token-file` (default: `mcp-token`
  in simee's state directory, `$SIMEE_STATE_DIR`, else `~/Library/Application Support/simee` on
  macOS and `$XDG_STATE_HOME/simee` or `~/.local/state/simee` elsewhere). A missing file is created
  with one random 256-bit token, mode 0600, in a directory created 0700. The file must be a regular
  file (a symbolic link is refused), owned by the user running the server and readable by them
  only, and its directory must not be writable by group or others (or owned by another user):
  these are checked on the open file, so it can't be swapped in between. One token per line (one
  per client, say); `#` lines are comments. Rotate by rewriting the file: it is re-read when it
  changes, and a removed token is refused on the next request.
- `--no-auth` turns the token off, and is refused on a non-loopback bind. Loopback is not private:
  without a token, every other user and process on the host can drive the devices (another account
  on a shared Mac, a CI job on a shared runner, a container on the host network), and so can anyone
  who reaches a reverse proxy on this host, since the proxy connects from loopback (nginx's default
  `proxy_pass` even sends a loopback `Host`). Use it only on a single-user machine with no proxy.
- A session belongs to the token that opened it: a request, `GET` stream or `DELETE` naming another
  token's session gets 404, as for a session that doesn't exist, and a full session table makes
  room only from the caller's own sessions. (Without a token every client is the same caller.)
- A request with an `Origin` header (a browser page) is refused with 403 unless the origin is a
  loopback one, and on a loopback bind a `Host` that isn't a loopback name is refused too: a page on
  another site can't reach the server through DNS rebinding.
- Framing is strict, so a proxy and this server can't read one request two ways: `Content-Length`
  is ASCII digits only, chunk sizes hex digits only, `Transfer-Encoding` at most once and exactly
  `chunked`, never both; a `GET`, `DELETE` or other non-`POST` with a body is refused. Any 4xx or 5xx
  answer closes the connection (`Connection: close`), so nothing after an error is read as a request.
- Bounds: a body is at most 8 MiB (413 past it, before it is read) and a request head 16 KiB (431).
  A connection's first request head must be in within 10 s of the connection opening, and a later
  one within 10 s of its first byte (at most 8 blank lines may come before a request line); the
  deadline is never extended by what arrives. A body then has 10 s of its own. An idle kept-alive
  connection closes after 15 s. At most 64 connections are served at once, and 8 from one address that
  have not yet authenticated (a connection gives its address slot back with its first request that
  carries a valid token, or at once with `--no-auth`); 503 past either, each on its own thread
  (no async runtime), and 32 sessions. A session with no request for an hour is closed, its `GET`
  stream with it (the stream's pings don't count as use).
- A waiting response checks every 15 s that its client is still there (an event stream sends a
  comment), and a client that has gone (a write to it fails, or the connection is reset) cancels its
  request. A client that only shut down its sending side is still served: if it accepts
  `text/event-stream` its response becomes a stream whose pings tell whether it is still reading;
  otherwise its call runs to the end.

Per-call overhead, a `tools/call` of `identity` (`sim-api`'s real method) in a loop: each run is the
median of 5 rounds, and the table gives the median of 3 runs, µs per call. Apple M3 Max,
2026-11-27, release build, with other builds running (load average 20 to 30):

| Path | µs per call |
| --- | --- |
| stdio (the server reading from one OS pipe and writing to another, as `simee mcp` does) | 19.5 |
| HTTP on loopback, one kept-alive connection | 44.9 |
| HTTP, kept-alive, with a bearer token | 45.6 |
| HTTP, a new connection per call | 106.4 |

So HTTP adds about 25 µs per call on a kept-alive connection (checking a token adds about 1 µs)
and about 85 µs with a connection per call: small next to a device method (a `tap` or `screenshot`
takes milliseconds). Reproduce: `cargo test --release -p simee-mcp http_call_overhead -- --ignored --nocapture`.

## Limits and what is not here

- Completions, logging, sampling and elicitation are not offered; the specification deprecates
  three of them.
- Cancelling stops `boot`, `boot_many`, `create`, `clone` and `import` at their next safe point
  and undoes what they half did; it doesn't stop an install, erase, settle or memory tool already
  running (above).
- The change notifications are polled, at each resource's pace above, and only for what a client
  subscribed to; a change shows up at the next read, not when it happens. A subscribed screen
  costs a screenshot a read even when it doesn't change (no cheaper change probe exists in the
  API), and an input made through simee's own tools doesn't bring the next read forward.
  A change between a client's last read and its subscription's baseline read isn't announced.
- Over HTTP: no OAuth (a bearer token from a file is the only authentication; tracked in a
  follow-up issue), TLS only in a build with `--features tls` and with certificates read at start,
  no resumable streams, and an `Origin` allowlist beyond loopback and the connection limits aren't
  configurable.
- Tool calls run in the server's process, on its own threads, with a session per thread; they don't
  go through a running `simee serve`, so a long-lived MCP server holds its own simulator connection.
- A tool is as powerful as the method: `exec` runs programs inside an iOS device, `write` and `rm`
  change its files (confined to the device's data directory), `delete` and `erase` destroy data.
  Review a client's approval prompts accordingly; the annotations say which tools only read.

## Sources

- [Specification 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28) and its
  [changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog): stateless
  requests, `server/discover`, `subscriptions/listen`, error codes, the stdio transport.
- Transports: [2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports)
  and [2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
  (Streamable HTTP: one endpoint, `Mcp-Session-Id`, `MCP-Protocol-Version`, `Origin` validation).
- [Tasks extension](https://github.com/modelcontextprotocol/ext-tasks) (`io.modelcontextprotocol/tasks`).
- [MCP Apps](https://github.com/modelcontextprotocol/ext-apps) (`io.modelcontextprotocol/ui`,
  specification 2026-01-26): `ui://` resources, `_meta.ui.resourceUri`, the postMessage protocol.
- [`rmcp`](https://crates.io/crates/rmcp), the official Rust SDK, for the comparison above.
