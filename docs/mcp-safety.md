# MCP safety

An agent that drives [`simee mcp`](mcp.md) can erase a device, kill a process or boot a dozen
simulators on a laptop that has room for three. This page is what the server does about that: it
asks a person before a destructive call, limits what each client may touch, applies the host's
health check to boots, and writes down every call. None of it needs the agent's cooperation, and
all of it is on without a flag except the limits on devices and tools, which you choose.

| Default | What it does | How to change it |
| --- | --- | --- |
| Destructive tools confirm | `delete`, `erase`, `kill`, `clear_container`, `reset_keychain`, and `shutdown` of a device this client didn't boot ask a person first | Add tools with `confirm` in the policy file. Nothing turns it off |
| Boots are admitted | `boot`, `boot_many`, `create` and `clone` fail fast, with the verdict, when the host can't take another device | `--admission ignore` |
| Calls are audited | Every `tools/call` is a line in `mcp-audit.log` in simee's state directory | `--audit-log PATH`, `--no-audit-log` |
| No limits on devices or tools | A client may touch any device and use the toolsets the server serves | `--allow-device`, `--created-devices-only`, `--read-only`, `--policy FILE` |

The rules apply to MCP only. `simee call`, the socket API and the CLI are the user's own hands and
are not held to a client's policy.

## Confirming destructive calls

A call to a tool in the first row of the table is not run until a person has agreed to it. How the
person is asked depends on the client:

- **A client that declares `elicitation`** is asked through MCP. For the current revision
  (2026-07-28) the call returns an `input_required` result whose `inputRequests` holds one
  `elicitation/create` form: a yes/no, defaulting to no, with a message that names the device and
  what will happen (`erase 'iPhone 17' (UDID): its content and settings are deleted`). The client
  shows it, and retries the call with `inputResponses` and the `requestState` it was given. Over
  stdio, a client of 2025-11-25 or 2025-06-18 (the revisions that have elicitation) is asked with an `elicitation/create`
  request of the server's own, which the client answers on the same stream. Either way only an
  accepted form that says yes runs the call; a decline, a dismissal, a no, an error or no answer
  within ten minutes refuses it.
- **Any other client**, and a client of an earlier revision over HTTP (that transport can't carry the
  question back), has to pass `confirm: true` in the arguments. The refusal says so:

  ```text
  refused: `erase` needs the user's confirmation: erase `iPhone 17` (UDID): its content and
  settings are deleted. This client doesn't support elicitation, so it can't be asked. Ask the
  user, and only if they agree call `erase` again with `confirm: true` (nothing was done).
  ```

The server never confirms for the person. In particular, `confirm: true` from a client that **can**
be asked counts for nothing: an agent can write it as easily as any argument, so such a client is
asked every time. `confirm` is removed from the arguments before the method runs.

The `requestState` of the current revision's flow is a keyed hash of the client, the tool and the
arguments, so a yes given for one call can't be replayed on another (a different device, say). It is
not a one-time token: the same yes retried with the same arguments runs the call again, which is the
same call the person agreed to.

`shutdown` is destructive only for a device this client didn't boot: a device that wasn't running
when the client's `boot` or `boot_many` asked for it is the client's own to stop, and needs no
confirmation. Booting a device that was already running doesn't make it the client's.

A policy can add tools to the confirming set (`"confirm": ["uninstall", "rm"]`).

## Which devices and tools a client may use

The rules come from flags, from a policy file, or both:

```bash
simee mcp --read-only                          # look, never change
simee mcp --allow-device 'ci-*' --allow-device 'pixel-?'    # these devices only
simee mcp --created-devices-only               # only the devices this client made
simee mcp --policy ~/.config/simee/mcp-policy.json
```

A policy file is one JSON object. `default` is every client's rules; `clients` changes them for the
client of that name. A client's rules are the default's with its own fields on top.

```json
{
  "default": {"devices": ["ci-*", "@created"], "toolsets": ["core", "debug"]},
  "clients": {
    "review-bot": {"read_only": true, "devices": ["*"]},
    "ops": {"admission": "ignore", "confirm": ["uninstall"]}
  }
}
```

| Field | Meaning |
| --- | --- |
| `read_only` | Only tools that change nothing run, and only those are listed (the catalog's `Read` effect in `sim-api`; a tool nobody classified counts as changing something) |
| `devices` | UDID or name patterns, where `*` matches any run of characters and `?` one; `@created` is the devices this client made. A device outside the list is left out of what the client lists and refused when named. An empty list allows none |
| `toolsets` | The toolsets this client may use, within what the server serves (`SIMEE_MCP_TOOLSETS`): the client gets the tools that are in both. Tools outside it aren't listed and are refused, naming the toolset |
| `confirm` | More tools that need confirmation |
| `admission` | `refuse` (default) or `ignore`, below |

Flags set the `default` and replace the same field of the file's `default` (`--allow-device` replaces
its `devices`). `--allow-device` and `--created-devices-only` together mean the pattern's devices and
the client's own. A key the server doesn't know is an error: a policy that silently skips a rule is
worse than none, and a file that doesn't parse stops the server before it serves anything.

How `devices` is applied:

- Every call that names a device (`device`, or `devices` for `boot_many`) resolves the name the way
  the method does, through `info`, and checks the device it is. A UDID, a name and `booted`
  can't be used to name a device around the list. A name that doesn't resolve is refused, since it
  can't be checked.
- The check is in the backend every tool, resource and subscription reads through, so
  `simee://devices/...` resources and change notifications hide a device the same way `devices` does.
- `devices_usage` leaves out the devices a client may not see. `streams` and `camera_links` list
  every device's, so without `device` they are refused for a client with a device list.
- `create`, `clone` and `import` make a device: its name has to be one the client may touch
  afterwards (matching a pattern, or `@created` in the list), or the call is refused before it makes
  anything. What the client creates is added to its devices as `@created` counts them, until it
  deletes it.
- A refusal says what was named, what the policy allows and who changes it:

  ```text
  refused: `erase` names `prod-1` (UDID), which this client may not touch (its policy allows ci-*).
  The operator widens it with `--allow-device` or the policy file (docs/mcp-safety.md)
  ```

### Who a client is

A client is named by what its transport vouches for (`Server::set_identity`, for a transport that
authenticates one: not wired for HTTP bearer tokens yet), else by the `clientInfo.name` it declares in
`initialize` or in each request's `_meta`, else `unknown`. A name a client declares is its own word:
two clients that share a transport can claim each other's name. So per-client rules are a convenience
for clients you run yourself (one `simee mcp` per client, over stdio, is the case where the name is
the client), and the **default** rules are the boundary: make them the strictest you accept, and don't
let a client's own entry be what keeps it from a device a stranger on the same HTTP server could ask
for under the name of another.

What a client has created and booted is kept in the server's memory by that name, so it is lost when
the server stops: after a restart, `--created-devices-only` clients can't touch devices an earlier
session made, and a `shutdown` of a device an earlier session booted asks for confirmation.

## Host-health admission

`boot`, `boot_many`, `create` and `clone` carry `admission` in `sim-api`: `ignore` reports what the
device would do to the host (the `admission_check` verdict: admit, hold or backoff, with every
reason) and goes ahead, `refuse` fails fast unless the verdict is admit, `wait` (boots) waits for room.
Over MCP the server applies `refuse` to a call that doesn't say, so an agent can't pile devices onto
a loaded host by not asking:

- The verdict comes back with the result (`admission` in `structuredContent`, `ok (admit)` in the
  text), and a refusal is an error that says it (`admission hold: memory used is 88% ... (nothing was
  done)`).
- An agent can ask for `wait`. It can't ask for `ignore`: that call is refused, saying the operator
  allows it (`simee mcp --admission ignore`, or `"admission": "ignore"` for a client in the policy
  file).
- A host with little memory refuses a `create` too, since the verdict is about what booting the device
  would cost. Pass `--admission ignore` on a machine that is meant to run at its limit.

## The audit log

Every `tools/call` is one JSON line in `mcp-audit.log` in simee's state directory (`$SIMEE_STATE_DIR`,
else `~/Library/Application Support/simee` on macOS and `$XDG_STATE_HOME/simee` or
`~/.local/state/simee` elsewhere), or in `--audit-log PATH`:

```json
{"ts_ms":1795830000123,"client":"claude-code","tool":"erase","args":{"device":"ci-1","confirm":true},"status":"ok","duration_ms":4120}
```

| Field | |
| --- | --- |
| `ts_ms` | Milliseconds since the epoch, when the call ended |
| `client` | Who it was (above) |
| `tool`, `args` | The tool, and the arguments as the client sent them, redacted |
| `status` | `ok`; `error` (the method failed); `refused` (policy, read-only, no confirmation, admission, a tool that isn't served); `input_required` (the client was asked to confirm; its retry is the next entry) |
| `duration_ms` | How long the server took, including a wait for the person's answer in the earlier revisions' flow |
| `detail` | Why it was refused or failed, shortened |
| `admission` | The verdict a boot came back with |

A task (a `boot` for a client with the tasks extension) is logged when it ends, as it ended. A call
for a tool that doesn't exist is logged as refused with the name the client gave.

**Redaction.** Before a line is written:

- a value whose key contains `password`, `passwd`, `passcode`, `secret`, `token`, `apikey`,
  `api_key`, `api-key`, `authorization`, `credential`, `cookie`, `private_key` or `bearer` (any case,
  at any depth: `env`, `args`) becomes `[redacted]`;
- text typed, written or delivered (`type`'s `text`, `write`'s `text` and `base64`, `defaults_write`
  and `prefs_write`'s `value`, `prefs_batch`'s `ops`, `push`'s `payload`) is logged as its size;
- inside any string, the value after `password=`, `token:`, `Bearer`, `Authorization:` and the like,
  and the user and password of a URL, are masked;
- a string longer than 256 characters is cut, saying how long it was.

That is a list of what's recognised, not a guarantee: a secret in a free-form string that doesn't
look like one (`curl -u me:hunter2`) is logged as written. Don't put credentials in tool arguments.

**Size and files.** The log is capped at `--audit-max-bytes` (8 MiB): past it, `mcp-audit.log`
becomes `mcp-audit.log.1`, the older files move up one, and the oldest of the three kept is deleted,
so the history is at most four times the cap. Each file is mode 0600 in a directory made 0700, a link
in the log's place is refused, and an existing file with looser permissions is tightened. A line is
one append, so servers sharing a file don't tear each other's lines (they can race at a rotation: give
each its own `--audit-log` for a clean history). The server won't start if it can't open the log.
Once it runs, a write that fails (a full disk) is reported on stderr, not on the call.

**Reading it.** `simee mcp-audit` prints the newest 50 entries, oldest first; `--limit`, `--tool`,
`--client` and `--status` narrow them, `--json` prints the lines as they are, and `--log` names
another file.

```bash
simee mcp-audit --status refused --client claude-code
simee mcp-audit --tool erase --json | jq .args
```

## What this doesn't do

- It isn't a sandbox. A client allowed `exec` runs programs inside an iOS device, and `write` and `rm`
  change its files; the policy decides which devices and tools, not what they do to a device it may
  touch. Withhold `debug` and `files` from a client you don't trust with them.
- A policy on `devices` doesn't hide that other devices exist from the host: it hides them from the
  client's tools, resources and notifications. `host`, `devices_usage` (filtered) and `identity` tell
  a client about the machine.
- `stream_status`, `stream_stop` and `audio_capture_stop` name a stream or a capture by the id its
  starting call returned, not by a device, so a device list can't check them.
- The client's name is self-declared (above), and HTTP bearer tokens don't name a client yet.
- Created and booted devices are remembered for one run of the server (above).
- Confirmation for a client of an earlier revision over HTTP is `confirm: true`: the transport doesn't
  hand the client's response to the server. The current revision's flow works over both transports.
- The log is written when a call ends, so a call that never ends (the server killed in the middle of
  an `erase`) isn't in it.
- `confirm_also` and `devices` match tool and device names as written; a tool added to `sim-api`
  later is not confirming until the policy names it. The six built in are what the issue
  (#294) names.

## Sources

- [Elicitation, 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25/client/elicitation):
  the `elicitation/create` request and its three actions, the capability a client declares, and that a
  server must not ask for secrets in a form.
- [Multi Round-Trip Requests, 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr):
  how the current revision asks (`input_required`, `inputRequests`, `inputResponses`, `requestState`,
  which a server must treat as attacker-controlled and protect when it affects authorization), and
  [the changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog) that replaces
  server-initiated requests with it.
- [Tools, 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25/server/tools): "there
  SHOULD always be a human in the loop with the ability to deny tool invocations", servers
  implementing access controls, clients logging tool usage for audit. It also says why a refusal is a
  tool result with `isError: true` and not a protocol error: clients give those to the model, which can
  correct itself, and a refusal that says how to confirm is feedback for it.
