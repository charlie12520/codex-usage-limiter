# Codex Usage Limiter

Codex Usage Limiter is an unofficial Windows desktop utility that watches the signed-in Codex account's quota and applies a selected response when usage reaches a configured threshold.

It is derived from [Dimillian/CodexMonitor](https://github.com/Dimillian/CodexMonitor). The upstream copyright and MIT license are retained. This project is not affiliated with or endorsed by OpenAI.

## What it does

- Reads the primary and secondary rate-limit windows from the local Codex app-server.
- Displays the more-used window as one current-usage percentage.
- Applies one threshold to both windows.
- Offers three responses:
  - **Notify only** — show a native Windows notification and allow work to continue.
  - **Finish current turn** — block new inference, let turns active at the crossing finish, then pause.
  - **Interrupt immediately** — block new inference and interrupt owned active turns.
- Persists quota state, exact owned-turn identities, deadlines, and reset verification across restarts.
- Revalidates account identity before reopening inference.

The limiter controls only Codex sessions launched through this application. It does not control separate terminals or other Codex clients.

## Requirements

- Windows 10 or 11
- Codex CLI installed and signed in
- Node.js and npm for frontend builds
- Rust stable toolchain for desktop builds
- CMake and LLVM/Clang for the inherited native Rust dependencies

## Development

Install the lockfile-pinned JavaScript dependencies:

```bash
npm ci
```

Run the desktop app in development mode:

```bash
npm run tauri:dev:win
```

Run verification:

```bash
npm test
npm run typecheck
npm run build
cd src-tauri && cargo test
```

Build the Windows application and installers:

```bash
npm run tauri:build:win
```

Artifacts are written under `src-tauri/target/release/` and `src-tauri/target/release/bundle/`.

## Safety model

Quota enforcement is backend-authoritative. Inference admission closes before interrupt, drain, parked, verification, and intervention states. The frontend controls are only a projection of the Rust quota-guard state.

A connected local workspace is required before the limiter can be enabled because account identity and turn ownership must be proven through a local Codex app-server session.
