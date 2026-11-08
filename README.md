# conversation-core

One conversation with one model, as a standalone program. It holds the
transcript, builds requests, streams replies, hands tool calls to whoever
runs it, and keeps the context within budget. It knows nothing about the
program that drives it.

```
core run <session-dir> [--no-tool <name>]... [--models <file>] [--agent-dir <dir>] [--skills <path>]...
                              one session as a process: JSON lines on stdin/stdout
core prompt [--agent-dir <dir>] [--skills <path>]... [--tool <name>]...   print the core's part of the system prompt here
core context <session-dir>    print the context the next request would send
core input <session-dir> <api> <provider> <model>   print the Responses request input
core referenced <session-dir>   print the stored files the context and the latest two checkpoints refer to
core tool <name> <arguments-json>   run one built-in tool in the current folder, print its result
```

## Models file

Context windows come from a models file, not the code:
`~/.config/conversation-core/models.json`, or `--models <file>`. `models.json`
in this repository lists the model definitions. Every request ends with a context
usage line ("Context: about 91k of 500k tokens (84k measured + ~7k new)"),
which is never saved. If the next request is over the window, the overflow
guard sets tool results aside (oldest first), then tool-call arguments and
images, and if it still does not fit it sends nothing and reports
`context_too_large`. For a model missing from the file the core reports
`window_unknown` once, and the guard cannot protect its requests.

## Usage and cost

Each reply's `message_end` carries its `usage` in pi's shape (`input`, `output`,
`cacheRead`, `cacheWrite`, `reasoning`, `totalTokens`, and `cost` in dollars) and
`contextUsage` (`tokens`, `contextWindow`, `percent`); the saved reply has the same usage.
Cost is pi's `calculateCost` with the model's `cost` rates (from the model object, else the
models file; per million tokens, with `tiers` above an input size), then service-tier
pricing: flex half, priority double (Codex reports "default" for a priority request, which
still counts as priority). Without rates the cost stays zero.

## Errors

A reply that fails is saved as pi saves it (`stopReason: "error"`, never sent to the model
again), and the `error` event names it (`n`). Its `errorMessage` is a fixed code, never the
provider's text: `provider_http_<status>`, `provider_request_failed`,
`provider_stream_incomplete`, `provider_stream_stalled`, `invalid_provider_json`,
`context_length_exceeded`, `provider_error`, `provider_response_failed`. When the provider
sent its own error, `providerError` describes it in fixed tokens as the harness does
(`code` and `category` from a known list, else "unknown", plus what shape the error had).

## System prompt

The core builds the parts of the system prompt that change, the way pi 1.5 does, as
named sections (`<tools>`, `<rules>`, `<project_context>`, `<skills>`, `<cwd>`):

- the tools on offer, each with its one-line `promptSnippet`, and their `promptGuidelines`
  (a runner tool may carry both; they are taken off before the tool is sent);
- the AGENTS files: the one in the agent folder (`--agent-dir`, default
  `~/.config/conversation-core`), then one per folder from the filesystem root down to the
  working folder (`AGENTS.override.md`, `AGENTS.md`, `CLAUDE.md`; a nested git worktree's
  file replaces its main checkout's). They are read for every request, and `prepare_request`
  lists them as `contextFiles`;
- the skills in `<agent-dir>/skills` and each `--skills` path (SKILL.md frontmatter), listed
  only when `read` or `bash` is on offer;
- the working folder.

There is no fixed wording: no opening line, no standing rules. Those belong in an AGENTS file.
`prepared.instructions` comes first (pi's preamble), and `prepared.sections`
(`{name: text}`) follow the core's, a same-named one replacing it. `"basePrompt": false`
sends `instructions` alone. Outside Codex the prompt is the first input message, with
role `developer` for a reasoning model (unless `compat.supportsDeveloperRole` is false),
otherwise `system`.

## Cleaning up old stored data

Deleting old data is not the core's job. A cleanup must hold the session's
`references.lock` (an `flock`) from running `core referenced` until it has
finished deleting, and must only delete files not listed. The core takes the
same lock only while accepting a new context, so a new reference never names
a file being deleted. Modeled in `specification/ReaperReferences.tla`.

## Request options in `prepared`

- `reasoning`: the session's level (`off` .. `max`). It is fitted to the model as pi does
  (`clampThinkingLevel`, `thinkingLevelMap`), using `reasoning` and `thinkingLevelMap` from
  the model object or the models file.
- `serviceTier`: sent as `service_tier`, e.g. `"priority"` for Fast mode.
- `reviewRequest: true`: before sending, the core emits `review_request` with the exact
  `{url, headers, body}`; answer `{"op":"review_request","id",...}` with a changed `body`
  and/or `headers`, or neither to keep them (pi's before_provider_request/headers).
- `streamStallMs`: give up an attempt after this long without a sign of life
  (default 10 minutes, as in the harness); it is retried like a dropped connection.

- Headers are the harness's: content type, accept, authorization, and `session_id`
  (Codex: `session-id`, `originator`, `chatgpt-account-id`, taken from `credentials.accountId`
  or the token's claims). Then `credentials.headers` and the model object's `headers` (pi's
  `model.headers`, e.g. grok-cli's client identification) are merged over them, a null
  removing one; `credentials.baseUrl` replaces the model's endpoint. Provider-specific
  request changes (grok-cli's payload fixes) belong to the runner, via `reviewRequest`.
- `transport`: Codex requests go over one WebSocket kept open between requests (as the
  harness sends them), with plain HTTP when no socket can be opened; `"sse"` keeps them on
  HTTP. Every request carries the whole context (no `previous_response_id`), so the usage
  line stays once per request. A socket closes after 5 idle minutes, after 55 minutes, or
  on any trouble; a rejected or stale socket is replaced once without a retry.

Each attempt reports `provider_response` with the status and headers (101 and
`"transport": "websocket"` when a socket is opened; none when a kept one is reused).

## What a runner can do besides turns

The same things pi offered, as ops on `core run` (each may carry an `id`, echoed in its event):

- `add_context` (pi's custom message): a message the model sees, `{customType, content, display, details}`. Like pi's `triggerTurn` it wakes the model: when idle it starts a turn; during a turn it is added at the next safe point and answered in that turn, or with `"deliver": "follow_up"` added after the turn, which then starts another. `"wake": false` (internal notes) only adds it. After an abort nothing is woken. Event `added`.
- `record` (pi's custom entry): `{ns, data}` bookkeeping the model never sees. Event `recorded`.
- `set_model`: `{provider, model}` and/or `{reasoning}`, saved as pi-style `settings` records and in meta.json; `prepare_request` offers it as `selected`. Event `model_set`.
- `history`: records `from`..`to`, optionally with stored data read back (`resolve`). Event `history`. For a session that is not running: `core history <dir> <from> <to>`.
- `complete` (pi's `completeSimple`): `{model, instructions, messages, reasoning}`, one request outside the conversation, never saved. It asks for credentials like a turn; event `completed` with `message` or `error`.

## Built-in tools

The core has pi 1.5's seven basic tools built in: `read`, `bash`, `edit`,
`write`, `grep`, `find`, `ls`. Names, parameters, output text, limits and
edit rules are pi's; `grep` and `find` use ripgrep's and fd's libraries
instead of the programs. They work in the folder the core was started in.

All seven are offered by default. `--no-tool <name>` turns one off; a tool
the runner sends in `prepared.tools` with the same name replaces it. A
built-in call's `tool_call` event has `"builtin": true` (the runner only
shows it), and a `tool_result` event follows once its result is saved.

The session folder format, the context rule and the message protocol are
described in the design document. `specification/` holds the TLA+ models;
`specification/check.sh` checks them (needs Java and `tla2tools.jar`).

```
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```
