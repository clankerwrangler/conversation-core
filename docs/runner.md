# Runner integration

## System prompt

The core builds the changing parts of the system prompt as
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
`prepared.instructions` comes first, and `prepared.sections`
(`{name: text}`) follow the core's, a same-named one replacing it. `"basePrompt": false`
sends `instructions` alone. Outside Codex the prompt is the first input message, with
role `developer` for a reasoning model (unless `compat.supportsDeveloperRole` is false),
otherwise `system`.

## One writer per session

`core run` takes the session's `writer.lock` (an `flock`) before it reads or
writes anything and holds it while it runs; a second core started on the same
session stops at once with `another core is writing this session`. The operating
system releases the lock when the core ends, also after a crash. `history`,
`context` and `referenced` only read and never take it. Modeled in
`specification/CoreWriterLock.tla`.

## Cleaning up old stored data

`core reap <session-dir> <age-seconds> [--keep-details <a,b,...>]` deletes the tool
output and reasoning files (`log/NNNNNN.payload.jsonl`, `log/NNNNNN.reasoning.jsonl`) of
every chunk all of whose records are provably older than the age (RFC 3339 times, any
offset; a record with a missing or unreadable time keeps its chunk), except those
`core referenced` lists. It never touches the current chunk, the records (all messages
stay) or the images meant for the user. Before a payload file goes, `log/NNNNNN.kept.jsonl`
gets every tool call's arguments whole (they read back as before) and the named top-level
fields of each tool result's `details`, the runner's own small facts (an empty object
when there are none); a history selection of only kept fields then reads as before. Tool
output and reasoning are what goes: they show in history as `"expired": true`. The
context is unchanged.
It prints `{"deleted": [files], "bytes": n}`. A runner calls it on a schedule (the
harness: daily, 14 days). A checkpoint that refers to deleted stored data is refused.

`--protocol 2` names exactly this behaviour. A runner should always pass it: cores built
before it reject the flag before touching anything, and some of them reap with weaker
rules (later tool calls' arguments lost, offset times misread), so they must never run a
runner's cleanup. Any other protocol value is refused.

It holds the session's `references.lock` (an `flock`) from listing the referenced
files until it has finished deleting. The core takes the same lock only while
accepting a new context, so a new reference never names a file being deleted.
Modeled in `specification/ReaperReferences.tla`.

## Request options in `prepared`

- `reasoning`: the session's level (`off` .. `max`). It is fitted to the model using `reasoning` and `thinkingLevelMap` from
  the model object or the models file.
- `serviceTier`: sent as `service_tier`, e.g. `"priority"` for Fast mode.
- `reviewRequest: true`: before sending, the core emits `review_request` with the exact
  `{url, headers, body}`; answer `{"op":"review_request","id",...}` with a changed `body`
  and/or `headers`, or neither to keep them.
  Use `reviewRequest: "body"` to review only the body, without sending the URL or headers to the runner.
- `streamStallMs`: give up an attempt after this long without a sign of life
  (default 10 minutes, as in the harness); it is retried like a dropped connection.

- Default headers include content type, accept, authorization, and `session_id`
  (Codex: `session-id`, `originator`, `chatgpt-account-id`, taken from `credentials.accountId`
  or the token's claims). `credentials.headers` and `model.headers` (for example,
  Grok CLI's client identification) are merged over them; a null removes a header.
  `credentials.baseUrl` replaces the model's endpoint. The runner can also make
  provider-specific request changes using `reviewRequest`.
- `transport`: Codex requests go over one WebSocket kept open between requests (as the
  harness sends them), with plain HTTP when no socket can be opened; `"sse"` keeps them on
  HTTP. Every request carries the whole context (no `previous_response_id`), so the usage
  line stays once per request. A socket closes after 5 idle minutes, after 55 minutes, or
  on any trouble; a rejected or stale socket is replaced once without a retry.

Each attempt reports `provider_response` with the status and headers (101 and
`"transport": "websocket"` when a socket is opened; none when a kept one is reused).

## What a runner can do besides turns

Additional ops on `core run` (each may carry an `id`, echoed in its event):

- `add_context`: a message the model sees, `{customType, content, display, details}`. It wakes the model: when idle it starts a turn; during a turn it is added at the next safe point and answered in that turn, or with `"deliver": "follow_up"` added after the turn, which then starts another. `"wake": false` (internal notes) only adds it. After an abort nothing is woken. Event `added`.
- `record`: `{ns, data}` bookkeeping the model never sees. Event `recorded`.
- `set_model`: `{provider, model}` and/or `{reasoning}`, saved as Pi-compatible `settings` records and in meta.json; `prepare_request` offers it as `selected`. Event `model_set`.
- `history`: the current chunk's entries (everything since the latest checkpoint), or with `from` / `to` older ones (scroll-back); the core reads from `from` through the tail so it can join replies. Entries are complete: a reply saved in parts comes as one message, stored data is read back (an entry whose data was deleted keeps its references and says `"expired": true`). Entries use the Pi-compatible types (`message`, `custom_message`, `custom`, `model_change`, `thinking_level_change`, with `id` and `timestamp`); `id` is the sequence number as text, with no parent links. A reply still being written is held back with everything after it, so handed-out entries never change and `from` = last id + 1 continues. Event `history` with `entries`. For a session that is not running: `core history <dir> [<from> <to>]`.
  For metadata, pass `select: {where, fields, details}`. `where` matches top-level
  message fields; `fields` names the message fields to return. `details` names detail
  fields, with one dot for a nested field. The core scans payloads one line at a time,
  returns only those values, and marks an entry `expired` if its details are unavailable.
- `complete`: `{model, instructions, messages, reasoning}`, one request outside the conversation, never saved. It asks for credentials like a turn; event `completed` with `message` or `error`.

A `send` or `steer` input may carry an `id` (the runner's name for it), kept on the saved user message and never sent to the model. Each turn begins with `turn_start`. `text_delta` carries `item`, the provider's id for the text item when it has one (kept in the saved text's `textSignature`). A saved reply's `timestamp` is when its request began; each of its text blocks carries `startedAt`, when that text's first words arrived (milliseconds, like `timestamp`; never sent to a provider). `text_delta`, `reasoning_delta` and `message_end` carry `attempt`, naming the reply being streamed (a retry streams a new one), and `message_end` carries `n`, its saved entry's id. An op the core refuses is answered with `error` carrying the op's `id` and `op`; during a
turn it does not end the turn. After turns, once nothing taken is waiting, the core reports
`settled` with `through`, the id of the last op it has taken (answers to
its own questions aside), so a runner knows whether ops it sent since are still to come.
An async tool result saved after a request's context snapshot wakes a follow-up, including
when it arrives after `settled`; duplicate or cancelled results do not wake another. The
wake is in-memory and is not recovered if the core stops before the follow-up. During one
core process, repeated provider call items with the same call identity are not saved or
dispatched again.
Responses replay keeps its chronological order. Chat Completions requires adjacent
call/results, so a late result is projected as a clearly labeled, core-supplied user-role
message after the intervening reply; the earlier call gets a pending placeholder.

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
result. `waitForToolHandlers(name)` waits for the matching handlers already running and for
their returned results to be written to the core; it does not wait for a separately supplied
pending result. Tests: `CORE_BIN=target/release/core node --test js/*.test.mjs`.
