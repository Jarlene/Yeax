# codex-workflow

Pure Rhai workflow engine migrated from Yeah `crates/codegen/xai-workflow`.

The crate has no dependency on `codex-core`, `codex-protocol`, or the TUI. All
side effects are requests sent through `WorkflowHostRequest`; replies travel
back over `oneshot` channels.

## Modules

- `meta` — parse and validate the `let meta = #{...}` header.
- `engine` — sandboxed Rhai runtime, script API, phases, budgets, pause tokens.
- `journal` — append-only journal with `seq + kind + req_hash` replay.
- `host` — host request/response protocol and agent option/result types.
- `run` — `PauseKind` and `WorkflowOutcome`.
- `validate` — `validate_only` entry points.

## Script surface

- `parallel([{prompt: ...}, ...])` takes an **array of option maps** (not
  thunks); at most 1024 items, results keep input order, and a failed item is
  represented by `null`.
- `pipeline(items, stage1, ...)` runs its **items serially**; within one item
  the stages run in order. The stage-array form `pipeline(items, [closures])`
  is also accepted.
- `eval`, `timestamp`, `sleep`, and `exit` are not available inside workflow
  scripts. Workflows must stay deterministic; pass timestamps through `args`
  and finish with `complete(value)` or `pause(kind, message)`.

## Tests

```bash
cargo test -p codex-workflow --all-targets
cargo run -p codex-workflow --example validate -- <script.rhai>
```
