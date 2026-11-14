# conversation-core

A standalone program for one conversation with one model. It stores the transcript,
builds requests, streams replies, runs built-in tools or hands calls to a runner, and
keeps the context within budget.

## Build and run

Build with a Rust toolchain that supports the 2024 edition:

```sh
cargo build --release
```

Start a session process with:

```sh
./target/release/core run SESSION_DIR
```

The process reads operations and writes events as JSON lines. A runner supplies the
model and responds to the core's requests. To sign in from a terminal, run
`./target/release/core login PROVIDER`; the core can also ask the runner for credentials.

By default, sign-ins are stored in an owner-only file at
`~/.local/share/conversation-core/auth.json`. Use `--auth <file>` to select another
file. Place explicit credential files outside version-controlled worktrees.

## Reference

- [Commands](docs/commands.md): command-line operations and options.
- [Configuration and providers](docs/configuration.md): locations, models, sign-in,
  provider behavior, usage, and errors.
- [Runner integration](docs/runner.md): the prompt, session lifecycle, cleanup,
  request options, operations, events, and JavaScript API.
- [Built-in tools and development](docs/tools.md): tools, tests, and models.

The project is distributed under the [MIT License](LICENSE). Adapted components and
their notices are listed in [Third-party notices](THIRD_PARTY_NOTICES.md).
