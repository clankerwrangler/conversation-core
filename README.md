# conversation-core

One conversation with one model, as a standalone program. It holds the
transcript, builds requests, streams replies, hands tool calls to whoever
runs it, and keeps the context within budget. It knows nothing about the
program that drives it.

```
core run <session-dir> [--no-tool <name>]... [--models <file>]   one session as a process: JSON lines on stdin/stdout
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

## Cleaning up old stored data

Deleting old data is not the core's job. A cleanup must hold the session's
`references.lock` (an `flock`) from running `core referenced` until it has
finished deleting, and must only delete files not listed. The core takes the
same lock only while accepting a new context, so a new reference never names
a file being deleted. Modeled in `specification/ReaperReferences.tla`.

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
