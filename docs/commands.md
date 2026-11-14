# Command reference

```text
core run <session-dir> [--no-tool <name>]... [--models <file>] [--agent-dir <dir>] [--skills <path>]... [--catalog <dir>] [--auth <file>]
                              run one session: JSON lines on stdin/stdout
core prompt [--agent-dir <dir>] [--skills <path>]... [--tool <name>]...
                              print the core's system prompt sections
core context <session-dir>    print the context the next request would send
core input <session-dir> <api> <provider> <model>
                              print the Responses request input
core chat-body <session-dir> <model-json> <level>
                              print the Chat Completions request body
core history <session-dir> [<from> <to>]
                              print complete history entries
core import <new-session-dir> <id> < records
                              build a session from record lines on stdin
core referenced <session-dir> print stored files referenced by context and recent checkpoints
core reap <session-dir> <age-seconds> [--keep-details <a,b,...>] [--protocol 2]
                              delete unreferenced old tool output and reasoning
core tool <name> <arguments-json>
                              run a built-in tool in the current folder
core usage <provider> [--auth <file>]
                              print remaining provider usage as JSON
core login <provider> [device|browser|key] [--auth <file>] [--json]
core logout <provider> [--auth <file>]
                              sign in or remove a stored sign-in
```

See [Configuration and providers](configuration.md#sign-in) for sign-in methods.
