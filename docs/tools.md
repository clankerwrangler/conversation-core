# Built-in tools and development

## Built-in tools

The core has seven built-in tools: `read`, `bash`, `edit`,
`write`, `grep`, `find`, `ls`. Their names, parameters, output text, limits, and edit rules are Pi-compatible.
`grep` and `find` use ripgrep and fd libraries instead of the programs. They work in the folder the core was started in.

All seven are offered by default. `--no-tool <name>` turns one off; a tool
the runner sends in `prepared.tools` with the same name replaces it. A
built-in call's `tool_call` event has `"builtin": true` (the runner only
shows it), and a `tool_result` event follows once its result is saved.

The [runner reference](runner.md) describes session behavior. `specification/` holds the TLA+ models;
`specification/check.sh` checks them (needs Java and `tla2tools.jar`).

```
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```
