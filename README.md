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
