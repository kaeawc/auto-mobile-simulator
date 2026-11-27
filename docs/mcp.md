# MCP server

`simee mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server on stdin and
stdout, so an agent can do everything simee can do: create, boot and drive iOS and Android devices,
look at their screens, read their logs, and watch them change.

| MCP feature | What simee gives it |
| --- | --- |
| Tools | One per method in `sim-api`'s table, generated from it, and `device_view`, with output schemas and annotations, in toolsets a client picks (`core` by default) |
| Resources | Devices, runtimes, form factors, and each device's info, screenshot, logs and apps |
| Subscriptions | Change notifications for those resources, by re-reading them while subscribed |
| Tasks | `boot`, `boot_many`, `create`, `clone`, `install`, `settle`, `erase`, `shutdown`, `export`, `import`, `mem_leaks`, `mem_heap` as pollable, cancellable tasks |
| App | `device_view`: a device's screen with tap, swipe, type and button controls, in the conversation |

Transport is stdio only: there is no HTTP or SSE listener, so nothing on the network can reach it.
The server runs in the agent's user account, with the same reach as `simee` itself.

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
  `exec`, `launch`, `push`, `network_shape`, `console`, `kill`, `export` and `audio_capture_start`
  are marked destructive (they run code, reboot or restart things, or write a file), and so are
  `delete`, `erase`, `shutdown`, `rm`, `uninstall`, `terminate`, `clear_container`,
  `reset_keychain`, `write`, `defaults_write`, `prefs_write`, `prefs_delete`, `prefs_batch` and
  the other overwrites. `openWorldHint` is true for `open_url` alone (the device may fetch the URL
  from anywhere). A method nobody has classified gets the specification's defaults (writes, may
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
| `help`, `identity`, `host`, `devices_usage`, `form_factors`, `devices`, `runtimes`, `create`, `delete`, `boot`, `boot_many`, `admission_check`, `clone`, `shutdown`, `screen`, `screenshot`, `contrast_check`, `tap`, `swipe`, `touch`, `button`, `type`, `key`, `open_url`, `install`, `uninstall`, `launch`, `terminate`, `apps`, `location`, `info`, `rotate`, `rotary`, `stylus`, `device_view` | Android, iOS |
| `console`, `fold`, `xr`, `vehicle`, `touchpad`, `export`, `import` | Android (iOS bundles: the next step of #170) |
| `prefs_stores`, `prefs_read`, `prefs_write`, `prefs_delete`, `prefs_batch`, `prefs_watch` | Android, iOS (iOS is implemented; Android answers "not yet available") |
| `device_types`, `settle`, `profile`, `erase`, `appearance`, `privacy`, `push`, `status_bar`, `clear_status_bar`, `content_size`, `increase_contrast`, `dynamic_island`, `backlight`, `keyboard_language`, `hardware_keyboard`, `ca_debug`, `ca_debug_state`, `reset_keychain`, `memory_warning`, `notify`, `biometric`, `exec`, `logs`, `processes`, `kill`, `connections`, `traffic`, `containers`, `ls`, `read`, `write`, `rm`, `du`, `clear_container`, `defaults_read`, `defaults_write`, `crash_reports`, `network_shape`, `network_state`, `network_flows`, `network_clear`, `network_env`, `audio_state`, `audio_volume`, `audio_route`, `audio_capture_start`, `audio_capture_stop`, `audio_mute`, `watch_gesture`, `remote_swipe`, `pointer`, `carplay` | iOS |
| `mem_leaks`, `mem_heap`, `mem_vmmap`, `mem_history` | iOS (the host's memory tools on one app; Android answers that they aren't available there) |

A test keeps this page honest: it fails if a tool isn't named here.

## Resources

| URI | Contents |
| --- | --- |
| `simee://devices` | Every device on both platforms (JSON) |
| `simee://runtimes` | iOS runtimes and Android system images |
| `simee://device-types` | iOS device types |
| `simee://form-factors` | The form-factor matrix, with inputs or why one is unsupported |
| `simee://devices/{device}` | The device's info (`info`) |
| `simee://devices/{device}/screenshot` | The screen now, a PNG (`image/png`; a booted device) |
| `simee://devices/{device}/logs` | The last minute of the unified log (`text/plain`; iOS) |
| `simee://devices/{device}/apps` | Installed bundle ids (iOS) |
| `ui://simee/device` | The device view's page (`text/html;profile=mcp-app`) |

`{device}` is a UDID or an AVD name. `resources/list` includes each device's resources that apply
to it (a screenshot for a booted device, logs and apps for an iOS one), and
`resources/templates/list` the four templates. Android has no log resource: its console is the
`console` tool. A device that doesn't exist is the specification's "resource not found" (`-32602`
for the current revision, `-32002` for the earlier ones); one that isn't running is an internal
error that says so.

**Subscriptions.** The API has nothing to subscribe to, so a subscription re-reads: while a client
is subscribed (`subscriptions/listen` with `resourceSubscriptions`, or `resources/subscribe` for an
older client), one thread reads each subscribed resource once a second and sends
`notifications/resources/updated` when the bytes differ. `resourcesListChanged` sends
`notifications/resources/list_changed` when a device appears, disappears, or starts or stops
running. Subscribing to a screenshot costs a screenshot a second; the thread stops when the client
cancels the listen (`notifications/cancelled`). The server never changes its tool list, so it
doesn't offer `toolsListChanged`.

## Tasks

A client that declares `io.modelcontextprotocol/tasks` gets a task, instead of a blocked call, for
`boot`, `boot_many`, `create`, `clone`, `install`, `settle`, `erase`, `shutdown`, `export`, `import`, `mem_leaks` and
`mem_heap` (seconds each; see their notes in docs/api.md): the result
has `resultType: "task"`, a `taskId`, `pollIntervalMs` (1 s) and `ttlMs` (an hour). The methods are
a list in `crates/sim-mcp/src/tools.rs`; every other tool answers inline.

- `tasks/get` returns the task: `working` (with `statusMessage` saying how long it has run),
  then `completed` with the tool's result in `result`, or `failed` with a JSON-RPC `error`.
  `failed` is for the server's own failures; a tool that returns `isError: true` completes, as the
  extension specifies.
- `notifications/tasks` carries the whole task when it ends, to a client that listened for its id
  (`subscriptions/listen` with `taskIds`).
- `tasks/cancel` is cooperative, as the extension allows. The task is `cancelled` at once and its
  result is discarded, but the API can't interrupt a device call that is already running, so a
  cancelled `boot` still finishes booting the device in the background. The status message says
  so.
- At most 8 task calls run at once; a ninth is refused with a tool error until one ends. A
  cancelled task's call keeps running, so it still counts: cancelling doesn't make room. Finished
  tasks are kept for an hour.
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

## Limits and what is not here

- Prompts, completions, logging, sampling and elicitation are not offered; the specification
  deprecates three of them.
- Cancelling a task doesn't stop a running device call (above).
- The change notifications are polled, once a second, and only for what a client subscribed to.
- Tool calls run in the server's process, on its own threads, with a session per thread; they don't
  go through a running `simee serve`, so a long-lived MCP server holds its own simulator connection.
- A tool is as powerful as the method: `exec` runs programs inside an iOS device, `write` and `rm`
  change its files (confined to the device's data directory), `delete` and `erase` destroy data.
  Review a client's approval prompts accordingly; the annotations say which tools only read.

## Sources

- [Specification 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28) and its
  [changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog): stateless
  requests, `server/discover`, `subscriptions/listen`, error codes, the stdio transport.
- [Tasks extension](https://github.com/modelcontextprotocol/ext-tasks) (`io.modelcontextprotocol/tasks`).
- [MCP Apps](https://github.com/modelcontextprotocol/ext-apps) (`io.modelcontextprotocol/ui`,
  specification 2026-01-26): `ui://` resources, `_meta.ui.resourceUri`, the postMessage protocol.
- [`rmcp`](https://crates.io/crates/rmcp), the official Rust SDK, for the comparison above.
