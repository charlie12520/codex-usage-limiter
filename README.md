# Codex Usage Limiter

Codex Usage Limiter is an unofficial Windows desktop utility that watches the signed-in Codex account's rate limits and applies a chosen response when your remaining quota drops below a floor you set.

It is derived from [Dimillian/CodexMonitor](https://github.com/Dimillian/CodexMonitor). The upstream copyright and MIT license are retained. This project is not affiliated with or endorsed by OpenAI.

![Compact mode](docs/screenshots/limiter-compact.png)

## What it does

- Polls the primary and secondary rate-limit windows from the local Codex app-server (immediately on launch, then every 10 seconds) and shows the window closer to running out as one **% remaining** figure with its reset countdown.
- Lets you drag a grabber along the usage bar to set the floor — the bar turns red once remaining quota falls below it.
- Fires one of three responses when remaining quota crosses the floor:
  - **Notify only** — show a native Windows notification and keep working.
  - **Finish current turn** — block new inference, let turns active at the crossing finish, then pause.
  - **Interrupt immediately** — block new inference and interrupt owned active turns.
- The green titlebar switch arms/disarms responses. Disarmed, the app keeps tracking and displaying usage but takes no action.
- Persists quota state, owned-turn identities, deadlines, and reset verification across restarts, and revalidates account identity before reopening inference.

The limiter controls only Codex sessions launched through this application. It does not control separate terminals or other Codex clients — but it tracks and displays account-level usage regardless of where you spend it.

## Window modes

Pick a size under Settings → Window size. Settings also has a **Keep in foreground** toggle (always-on-top) and light/dark appearance.

| Compact (420×240) | Mini (320×168) | Pill (280×72) |
| --- | --- | --- |
| ![Compact](docs/screenshots/limiter-compact.png) | ![Mini](docs/screenshots/limiter-mini.png) | ![Pill](docs/screenshots/limiter-pill.png) |

Compact keeps every control on the surface. Mini is a glance card. Pill is a titlebar-less sliver you can park in a screen corner — drag anywhere on it to move it.

![Settings](docs/screenshots/limiter-settings.png)

## Install

Grab a build from the [latest release](../../releases/latest):

- **Windows** (primary, fully tested): run the `-setup.exe` installer — it adds the app to the Start Menu so Windows search finds it, and installs per-user without admin rights. Prefer a portable copy instead? Grab the zip and run `codex-usage-limiter.exe` from anywhere (portable exes aren't indexed by Start Menu search). Windows 10/11.
- **macOS** (experimental, unsigned): open the `.dmg`, drag the app to Applications, then right-click → Open the first time to get past Gatekeeper.
- **Linux** (experimental): `.AppImage` (chmod +x and run) or `.deb`.

All platforms need the [Codex CLI](https://developers.openai.com/codex/cli) installed and signed in. The macOS and Linux builds compile in CI but the redesigned UI has only been hands-on verified on Windows — issue reports welcome.

Because releases are not code-signed yet, SmartScreen may show "Windows protected your PC" on first launch — click **More info → Run anyway**. The source is right here and every release is built by [GitHub Actions](.github/workflows/release.yml) from the tagged commit.

On first run, connect the folder where you use Codex when prompted — the limiter reads usage and proves account identity through a local Codex app-server session in that workspace. Tracking starts automatically once a workspace is connected.

Closing the window hides the limiter to the system tray; use the tray menu to reopen or quit. On Windows and macOS the tray icon's tooltip shows your remaining quota and left-clicking it reopens the window; Linux tray backends don't support hover tooltips or plain-click handlers, so use the menu's **Show** item there. Settings has a **Start at login** toggle to launch it at sign-in.

## Build from source

Requirements: Node.js 20+, npm, and a stable Rust toolchain (on Windows, rustup's default MSVC toolchain — accept its Visual Studio Build Tools prompt during setup). On Linux, also install the WebKitGTK stack: `sudo apt-get install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev`. No other native tooling is needed.

```bash
git clone https://github.com/charlie12520/codex-usage-limiter.git
cd codex-usage-limiter
npm ci
npm run build
cd src-tauri
cargo build --release --bin codex-usage-limiter --features custom-protocol
```

The exe lands at `src-tauri/target/release/codex-usage-limiter.exe`.

For development with hot reload:

```bash
npm run tauri:dev:win
```

Verification:

```bash
npm test
npm run typecheck
npm run build
cd src-tauri && cargo test
```

## Safety model

Quota enforcement is backend-authoritative. Inference admission closes before interrupt, drain, parked, verification, and intervention states. The frontend controls are only a projection of the Rust quota-guard state.

A connected local workspace is required before the limiter can track usage because account identity and turn ownership must be proven through a local Codex app-server session.

## License

MIT — see [LICENSE](LICENSE). Original work copyright (c) 2026 Thomas Ricouard ([Dimillian/CodexMonitor](https://github.com/Dimillian/CodexMonitor)); modifications for the standalone usage limiter build on that foundation.
