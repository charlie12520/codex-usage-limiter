# Orchestration Bugs

## Current issues

### Missing orchestration brief — non-blocking

- Expected path: `C:/tmp/CodexMonitor/ORCHESTRATION.md`.
- Result: file not found; repository-wide search found no `ORCHESTRATION*.md` or `ORCHESTRATOR*.md`.
- Active fallback: authoritative `local://plan.md` plus `docs/multi-agent-sync-runbook.md`.

### Terra-high worker discovery — recovered

- Generic unpinned workers were canceled before implementation.
- The first pinned retry used `C:/tmp/CodexMonitor/.omp/agents/terra-high-task.md`, but OMP discovery is rooted at the session cwd (`C:/tmp`), so both workers stopped with `Unknown agent \"terra-high-task\"`.
- Recovery applied: the same agent definition is registered at `C:/tmp/.omp/agents/terra-high-task.md` with `model: openai-codex/gpt-5.6-terra` and `thinking-level: high`.

### Rust verification prerequisites and compile failures — recovered

- Initial `cargo test quota_guard` stopped in `whisper-rs-sys` because no `clang.dll`/`libclang.dll` was installed.
- Recovery applied: installed pinned PyPI package `libclang==18.1.1` (26.4 MB) with Python 3.11 and set `LIBCLANG_PATH` to its bundled DLL; no executable or SDK installer was launched.
- Project parser/import/model compile errors were corrected; `cargo test quota_guard` passes 28 focused tests.

### Daemon/shared coordinator boundary — recovered

- The actor-less daemon path-includes `shared/mod.rs`; app-only state/notification dependencies in the first shared coordinator draft failed daemon compilation.
- Recovery applied: shared coordinator/event sink is backend-neutral; concrete Tauri state, notification, command, setup, and actor code lives in lib-only `src/quota_guard_runtime.rs`.
- Merged proof: full-target actor smoke test passes and `cargo check` succeeds, including `codex_monitor_daemon`.

### Desktop prototype smoke tooling — recovered

- `npm run tauri:dev:win` stopped in the project doctor because it requires command-line `cmake` and LLVM detection even though Rust builds succeed with the pip-provided libclang.
- Recovery applied: launched the committed Tauri CLI directly with the same Windows config, `LIBCLANG_PATH`, and WebView2 CDP enabled. The app started as `codex-monitor.exe` with main window title `Codex Monitor`.
- Orca computer-use runtime was unavailable (`stale_bootstrap`), so the smoke attached to the real Tauri WebView2 CDP target instead.
- CDP automation initially used a non-matching page title and a non-string click selector; both were corrected without changing application code.

### Quota badge panel smoke — recovered

- Root cause: Rust returned a nested prototype payload instead of the complete flattened camel-case `QuotaGuardPublicState`; the panel also assumed list projections were always present at the external Tauri boundary.
- Recovery applied: Rust now emits all required top-level state fields with null/empty defaults and serialization coverage; frontend panel/admission helpers tolerate partial stale payloads with a focused regression test.
- Merged desktop proof: real `quota_guard_get_state` returned the complete disabled state, clicking the visible `Quota guard: Disabled` badge opened the panel with account/freshness/breach/deadline/monitor/activity content, and **Close quota guard** returned to the intact app.
