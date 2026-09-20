# Agent instructions for `codex-code-router`

This experimental MIT fork routes Codex Responses traffic to native OpenAI or GitHub Copilot. Preserve the original project's license and attribution. This public repository starts from a reviewed source snapshot, not the private development history.

## Read first

Before changing code, read the public project guidance:

1. `AGENTS.md` — core doctrine and safety rules.
2. `README.md` — usage, configuration, validation, and public-facing behavior.
3. `docs/SETUP.md` — complete installation, configuration, safety and verification reference.

## Core doctrine

- **Provider adapter, not transformer.** Treat GitHub Copilot like an OpenAI-compatible cloud provider with different auth, headers, and endpoint conventions.
- **Responses-only.** Do not add Chat Completions, Anthropic Messages, or Anthropic SSE conversion paths.
- **Codex-native request shape.** Let Codex own request bodies, tool schemas, reasoning settings, history, compaction behavior, and SSE parsing.
- **Stream bytes through.** The proxy should forward request bodies and response streams with as little interpretation as possible.
- **No proactive tool rewriting or truncation.** Only add targeted compatibility fixes after a concrete Copilot error proves they are required.
- **Rate-limit retry is allowed.** HTTP `429` retry based on status/headers is a provider transport behavior, not protocol transformation.
- **Every mutation must be explicit.** If request or response mutation becomes necessary, keep it isolated, documented, logged, and tested.

## Hard constraints

- Do **not** recreate a transformer architecture.
- Do **not** add Chat Completions, Anthropic Messages, or Anthropic SSE bridges.
- Do **not** rewrite, truncate, summarize, or reshape tool calls by default.
- Combined routing may inspect and replace only the top-level model token. Never reconstruct conversations or translate tool protocols.
- Do **not** parse response-message text to detect rate limits; use HTTP `429` and headers only.
- Do **not** log bearer tokens, GitHub OAuth tokens, authorization headers, or unredacted request dumps.
- Do **not** add Docker/container support.
- Keep the model believing it is running inside **Codex**. GitHub Copilot is only the upstream provider.

## Current intended shape

- `/v1/models` and `/v1/responses` remain Copilot-only, authenticated by the private local bearer.
- `/combined/v1/responses` uses a separate local-key header. Native model IDs go to OpenAI; `copilot/<id>` aliases go only to Copilot.
- The combined catalogue is the startup allowlist. Discover only enabled, visible HTTP Responses models with streaming and tool calls; verify candidates before activation.
- Native credentials stay Codex-managed and are never forwarded to Copilot. Copilot credentials never go to OpenAI; neither upstream receives the local key.
- Native body bytes and SSE streams remain unchanged. Copilot routing changes only the model token.
- Missing/invalid catalogues disable combined Copilot aliases, not Native or the dedicated Copilot API. Never fall back between providers.
- Preserve finite body/retry limits, loopback/Origin/Host guards, secure token persistence, and private configuration permissions.
- Use validated account endpoint metadata when explicit Copilot URL overrides are absent. Never trust an arbitrary metadata host with credentials; never let Copilot discovery alter native destinations.

## Implementation direction

- Rust is canonical.
- Preserve the external contract: `GET /health`, `GET /v1/models`, `POST /v1/responses`.
- Preserve the minimal-adapter doctrine; do not add generic protocol translation behavior for its own sake.
- If a body compatibility pass becomes necessary, isolate it behind tests and document exactly why.

## Fresh-session onboarding

- Read `AGENTS.md` and `README.md` at the start of a new assistant session.
- Run the Rust validation commands before and after behavioral changes when possible.

## Commands
- Prepare offline private runtime files and a manual-merge config fragment: `python3 scripts/prepare_setup.py --codex /absolute/path/to/codex`. It must not edit Codex settings, authenticate, activate models, or start services.

- Foreground service or explicit login: `~/.local/bin/codex-copilot-router serve` / `login`.
- Prepare startup registration: `python3 scripts/install_launchagent.py`; `--load` registers only on a free port.
- Generate candidates: `python3 scripts/refresh_catalog.py --help`.
- Verify candidates using supervised isolated Codex sessions and reviewed synthetic patches; never treat model claims or process exit alone as proof.
- Format: `cargo fmt --check`.
- Rust tests: `cargo test --locked`.
- Python tests: `python3 -m unittest discover -s tests -p 'test_*.py'`.
- Lint: `cargo clippy --locked --all-targets -- -D warnings`.
- Build: `cargo build --release --locked --bins`.

## Maintenance and publication

- Registry publication is disabled by `publish = false`. The inherited self-update command is removed; upgrades require a reviewed local checkout, locked build, and deliberate install/restart.
- Keep credentials, runtime catalogues, native prompt caches, logs, and user configuration outside Git. Inspect the exact staged file set and scan it before an authorized push.
- Fetch upstream changes for review; do not automatically merge, install, or deploy an unreviewed release.
- Commit/push only with user authorization. Preserve upstream attribution and license; do not imply this was written entirely from scratch or import private development history.
- A clean working tree is not a clean history. Review every intended public ref for credentials, private runtime data and internal-model references before publishing. Do not rewrite private history or change repository visibility without explicit authorization.
- Describe only exercised compatibility. In the tested `0.154.0-alpha.6.2` custom-provider configuration, Codex compacts via ordinary Responses summarization; that is distinct from provider-native remote compaction, and offline fixtures do not prove live summary quality.
- Install from reviewed local source using `cargo install --path . --bins --locked --root "$HOME/.local/share/codex-code-router"`.
- Launchd owns the normal background service. Avoid competing supervisors or killing unverified processes. Coordinate any restart with active work.

## Coding conventions

- Keep Rust boring and maintainable.
- Prefer small modules with explicit responsibilities.
- Prefer byte-forwarding request/response behavior over JSON parsing.
- Use `axum` for the local server and `reqwest` for upstream HTTP.
- Keep logs on stderr and never include token-bearing values.
- Keep docs updated when behavior changes.

## Safety notes

- Never log bearer tokens or full authorization headers.
- Never log GitHub OAuth tokens.
- Redact sensitive headers in diagnostics and future request dumps.
- `codex-code-router print-token` stdout must contain only the token for Codex command-backed auth; diagnostics go to stderr.

## Debug helper: verify reasoning effort in raw diagnostics

When raw diagnostics are enabled and a task needs proof that Codex effort reached the proxy request, use the tightened request snapshot schema:

- event kind: `inbound_request_content`
- path: `fields.snapshot.extracted.reasoning_effort`

Level semantics:

- `metadata`: no content snapshot event
- `content_redacted`: value is `<redacted-content>`
- `full_content`: concrete effort value is visible (for example `high`)

Correlate with `fields.local_id` across `inbound_request`, `upstream_response_ready`, and stream terminal events for a full request trace.
