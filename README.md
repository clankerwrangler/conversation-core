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

## Where things are kept

By the usual Linux convention (`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` move them):

- configuration: `~/.config/conversation-core` (`models.json`, the AGENTS file, `skills/`);
- data: `~/.local/share/conversation-core` (`auth.json`; runners keep sessions in `sessions/`);
- cache: `~/.cache/conversation-core` (`catalog/`, fetched model lists).

## Models file and model lists

The runner only has to name a model (`provider` and `id`); the core fills in its details
(API, endpoint, limits, prices, reasoning levels, compatibility), under any the runner sets:

- OpenRouter: OpenRouter's own list, fetched from OpenRouter (`/models/user` with the
  account's key, as pi does) and cached in `--catalog <dir>` (default
  `~/.cache/conversation-core/catalog`), refreshed every 4 hours. OpenRouter does not say
  which models cannot switch reasoning off: when it refuses "off", the core sends again
  without the setting and remembers that for the model (`learned.json`).
- Codex: OpenAI's own list, as the Codex CLI fetches it (`<base>/codex/models` with the
  account's sign-in, revalidated with its ETag), cached and refreshed the same way. Its
  reasoning levels become pi's level map. It has no prices, so Codex replies have no cost.
  OpenAI's list never offers "none"; "off" is sent as "none" and a model that refuses it
  is learned the same way.
- xAI, and Codex when OpenAI's list cannot be fetched: the models file,
  `~/.config/conversation-core/models.json` or `--models <file>`. `models.json` in this
  repository has the model definitions, in pi's shape.

The `models` op (`{"op": "models", "provider"?}`) lists them. Every request ends with a context
usage line ("Context: about 91k of 500k tokens (84k measured + ~7k new)"),
which is never saved. If the next request is over the window, the overflow
guard sets tool results aside (oldest first), then tool-call arguments and
images, and if it still does not fit it sends nothing and reports
`context_too_large`. For a model missing from the file the core reports
`window_unknown` once, and the guard cannot protect its requests.

## Sign-in

The core signs in, stores credentials and renews them itself, with the providers' own flows
(as pi and the pi-grok-cli add-on run them). Credentials live in `--auth <file>` (default
`~/.local/share/conversation-core/auth.json`, pi's `auth.json` shape, owner-only). A request uses
the stored sign-in for its provider, renewed shortly before it expires; only when there is
none is the runner asked (`need_credentials`). Renewal holds `<file>.lock` and re-reads the
file first, so sessions sharing it never send a refresh token twice (providers rotate them).
Modeled in `specification/CredentialRefresh.tla`.

- `core login openai-codex|grok-cli [device|browser]`, `core login openrouter [key|browser]`,
  `core logout <provider>`: from a terminal. Device sign-in prints a page and a code to
  approve; browser sign-in prints a page and waits for the address it ends on to be pasted;
  an API key is read from standard input.
- The same as ops: `{"op": "login", "id", "provider", "method", "key"?}` with `login` events
  (`step`: `device_code`, `open_and_paste`, `done`, `failed`), `{"op": "login_input", "id",
  "input"}` for the pasted address, `logout`, and `auth_status` (who is signed in, never the
  secrets). A failed renewal is reported as `sign_in_needed`.

## Grok (grok-cli)

Grok through the Grok CLI proxy works as the pi-grok-cli add-on makes it work: its client
headers and conversation header, its model list (fetched from the proxy's `/models` with the
sign-in, the add-on's list shipped in `models.json`, without prices), and its request
changes (system prompt into `instructions`, reasoning items and images reshaped, tool-result
images moved to a message, effort fitted per model).

## Chat Completions (OpenRouter)

A model with `"api": "openai-completions"` (OpenRouter and other OpenAI-compatible
endpoints) is sent to `<baseUrl>/chat/completions` as pi 1.5 sends it: pi's compatibility
rules from the provider and endpoint, overridden by the model's `compat`; another model's
history replayed without its thinking and with shortened tool call ids; OpenRouter's
`reasoning` setting, session header and billed cost; cache markers for anthropic/ models.
`core chat-body <session-dir> <model-json> <level>` prints the body for a session.
A model's `contextWindow` and `cost` on the model object are used when the models file has
none.

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
- `history`: the current chunk's entries (everything since the latest checkpoint), or with `from` / `to` older ones (scroll-back); only the chunks holding the range are read. Entries are complete: a reply saved in parts comes as one message, stored data is read back (an entry whose data was deleted keeps its references and says `"expired": true`). Entries are pi's (`message`, `custom_message`, `custom`, `model_change`, `thinking_level_change`, with `id` and `timestamp`); `id` is the sequence number as text, with no parent links. A reply still being written is held back with everything after it, so handed-out entries never change and `from` = last id + 1 continues. Event `history` with `entries`. For a session that is not running: `core history <dir> [<from> <to>]`.
- `complete` (pi's `completeSimple`): `{model, instructions, messages, reasoning}`, one request outside the conversation, never saved. It asks for credentials like a turn; event `completed` with `message` or `error`.

A `send` or `steer` input may carry an `id` (the runner's name for it), kept on the saved user message and never sent to the model. Each turn begins with `turn_start` (pi's agent_start). `text_delta` carries `item`, the provider's id for the text item when it has one (kept in the saved text's `textSignature`); `text_delta`, `reasoning_delta` and `message_end` carry `attempt`, naming the reply being streamed (a retry streams a new one), and `message_end` carries `n`, its saved entry's id. An op the core refuses is answered with `error` carrying the op's `id` and `op`; during a
turn it does not end the turn. After turns, once nothing taken is waiting, the core reports
`settled` (pi's agent_settled) with `through`, the id of the last op it has taken (answers to
its own questions aside), so a runner knows whether ops it sent since are still to come.

## JavaScript API

`js/core.mjs` is the core's API for JavaScript runners: one `core run` process as an object
with plain functions, the JSON lines hidden. Handlers answer the core's questions; every
event is also emitted by name (`error` as `core_error`).

```js
import { Core } from "./js/core.mjs";
const core = Core.start({ bin, dir, args: ["--auth", file], cwd,
  prepare: async ({ context, selected, contextFiles }) => ({ model, instructions, tools }),
  credentials: async (provider) => ({ token }),   // only when the core holds no sign-in
  review: async (request) => ({ body, headers }), // with reviewRequest
  tool: async ({ call, name, arguments: args }, signal) => ({ content, isError, details }),
});
core.on("text_delta", (e) => process.stdout.write(e.delta));
core.send("hello"); await core.idle();
```

Turns: `send`, `steer`, `abort`, `idle()`, `busy`, `addContext(message, {wake, deliver})`,
`toolResult(call, result)` for async calls. Any time: `record`, `setModel`, `history`,
`models`, `complete`, `login` / `loginInput` / `logout` / `authStatus`. When idle:
`context`, `checkpoint`. `close()` ends input and waits for the core to exit. A handler
that throws aborts the turn (`handler_error`); a tool handler that throws gives an error
result. Tests: `CORE_BIN=target/release/core node --test js/core.test.mjs`.

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
