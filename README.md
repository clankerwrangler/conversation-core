# conversation-core

One conversation with one model, as a standalone program. It holds the
transcript, builds requests, streams replies, hands tool calls to whoever
runs it, and keeps the context within budget. It knows nothing about the
program that drives it.

```
core run <session-dir>        one session as a process: JSON lines on stdin/stdout
core context <session-dir>    print the context the next request would send
core input <session-dir> <api> <provider> <model>   print the Responses request input
```

The session folder format, the context rule and the message protocol are
described in the design document. `specification/` holds the TLA+ models;
`specification/check.sh` checks them (needs Java and `tla2tools.jar`).

```
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```
