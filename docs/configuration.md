# Configuration and providers

## Where things are kept

By default, the core uses the following Linux locations. `XDG_CONFIG_HOME`,
`XDG_DATA_HOME`, and `XDG_CACHE_HOME` can select different base directories:

- configuration: `~/.config/conversation-core` (`models.json`, the AGENTS file, `skills/`);
- data: `~/.local/share/conversation-core` (`auth.json`; runners keep sessions in `sessions/`);
- cache: `~/.cache/conversation-core` (`catalog/`, fetched model lists).

Relative XDG values are ignored. For credentials, if neither `XDG_DATA_HOME` nor
`HOME` is an absolute path, commands that need credentials fail unless `--auth <file>`
is provided. An explicit path can be relative, so place it
outside version-controlled worktrees.

## Models file and model lists

The runner only has to name a model (`provider` and `id`); the core fills in its details
(API, endpoint, limits, prices, reasoning levels, compatibility), under any the runner sets:

- OpenRouter: OpenRouter's own list, fetched from OpenRouter (`/models/user` with the
  account's key) and cached in `--catalog <dir>` (default
  `~/.cache/conversation-core/catalog`), refreshed every 4 hours. OpenRouter does not say
  which models cannot switch reasoning off: when it refuses "off", the core sends again
  without the setting and remembers that for the model (`learned.json`).
- Codex: OpenAI's own list, as the Codex CLI fetches it (`<base>/codex/models` with the
  account's sign-in, revalidated with its ETag), cached and refreshed the same way. Its
  reasoning levels become the `thinkingLevelMap`. It has no prices, so Codex replies have no cost.
  OpenAI's list never offers "none"; "off" is sent as "none" and a model that refuses it
  is learned the same way.
- xAI, and Codex when OpenAI's list cannot be fetched: the models file,
  `~/.config/conversation-core/models.json` or `--models <file>`. `models.json` in this
  repository has the model definitions in the Pi-compatible format.

The `models` op (`{"op": "models", "provider"?}`) lists them. Every request ends with a context
usage line ("Context: about 91k of 500k tokens (84k measured + ~7k new)"), sent in the
instruction role (developer or system, as for the instructions; a tagged user message for
Grok, whose proxy refuses both) so the model does not take it for the user. It is never saved. If the next request is over the window, the overflow
guard sets tool results aside (oldest first), then tool-call arguments and
images, and if it still does not fit it sends nothing and reports
`context_too_large`. For a model missing from the file the core reports
`window_unknown` once, and the guard cannot protect its requests.

## Sign-in

The core signs in, stores credentials, and renews them using the providers' flows.
Credentials live in `--auth <file>` (default `~/.local/share/conversation-core/auth.json`)
in a Pi-compatible `auth.json` format. The file is owner-only. A request uses
the stored sign-in for its provider, renewed shortly before it expires; only when there is
none is the runner asked (`need_credentials`). Renewal holds `<file>.lock` and re-reads the
file first, so sessions sharing it never send a refresh token twice (providers rotate them).
Modeled in `specification/CredentialRefresh.tla`.

- `core login openai-codex|grok-cli [device|browser]`, `core login openrouter [key|browser]`,
  `core logout <provider>`: from a terminal. Device sign-in prints a page and a code to
  approve; browser sign-in prints a page and waits for the address it ends on to be pasted;
  an API key is read from standard input. With `--json`, `core login <provider> device`,
  for programs that run a sign-in for someone, prints one JSON object per line instead:
  `{"step": "device_code", "url", "code", "expiresIn"}`, then `{"done": ...}` once the
  sign-in is saved or `{"error": ...}`.
- The same as ops: `{"op": "login", "id", "provider", "method", "key"?}` with `login` events
  (`step`: `device_code`, `open_and_paste`, `done`, `failed`), `{"op": "login_input", "id",
  "input"}` for the pasted address, `logout`, and `auth_status` (who is signed in, never the
  secrets). A failed renewal is reported as `sign_in_needed`.

## Grok (grok-cli)

The Grok CLI proxy uses client and conversation headers, a model list fetched from
the proxy's `/models` with the sign-in, and a fallback list in `models.json` without
prices. Requests move the system prompt into `instructions`, reshape reasoning and images,
move tool-result images to a message, and fit reasoning effort to the model.

## Chat Completions (OpenRouter)

A model with `"api": "openai-completions"` (OpenRouter and other OpenAI-compatible
endpoints) is sent to `<baseUrl>/chat/completions`. Compatibility rules come from the
provider and endpoint and can be overridden by the model's `compat`. Another model's
history is replayed without its thinking and with shortened tool call IDs. Requests support
OpenRouter's `reasoning` setting, session header and billed cost, and cache markers for
anthropic/ models.
`core chat-body <session-dir> <model-json> <level>` prints the body for a session.
A model's `contextWindow` and `cost` on the model object are used when the models file has
none.

## Usage and cost

Each reply's `message_end` carries its `usage` (`input`, `output`,
`cacheRead`, `cacheWrite`, `reasoning`, `totalTokens`, and `cost` in dollars) and
`contextUsage` (`tokens`, `contextWindow`, `percent`); the saved reply has the same usage.
Cost uses the model's `cost` rates (from the model object, else the
models file; per million tokens, with `tiers` above an input size), then service-tier
pricing: flex half, priority double (Codex reports "default" for a priority request, which
still counts as priority). Without rates the cost stays zero.

`core usage <provider>` asks the provider how much of the account's allowance is left and
prints `{"provider", "model": null, "status", "remaining"}`. `status` is `ready`,
`sign_in_required` (no sign-in, or the provider refused it), `unavailable` (the question
failed) or `unsupported` (a provider without a usage endpoint). For Codex, `remaining` is the
most-used rate-limit window (`kind: "rate_limits"`); for Grok it is the weekly allowance
with the plan (`kind: "weekly_allowance"`). It uses and renews the stored sign-in like a
request does, and never prints credentials.

## Errors

A reply that fails is saved (`stopReason: "error"`, never sent to the model
again), and the `error` event names it (`n`). Its `errorMessage` is a fixed code, never the
provider's text: `provider_http_<status>`, `provider_request_failed`,
`provider_stream_incomplete`, `provider_stream_stalled`, `invalid_provider_json`,
`context_length_exceeded`, `provider_error`, `provider_response_failed`. When the provider
sent its own error, `providerError` describes it in fixed tokens as the harness does
(`code` and `category` from a known list, else "unknown", plus what shape the error had).
