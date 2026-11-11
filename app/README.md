# Drive tray app

Tauri 2 + Svelte 5 UI for Drive: tray icon, status popover, onboarding and settings.
All sync logic lives in the `drive daemon` service (`crates/drive-daemon`); this app talks
to it over the local socket protocol in `crates/drive-core/src/ipc.rs`.

## Develop

```sh
pnpm install
pnpm dev                 # browser-only UI at http://localhost:1420 with a JS mock
pnpm tauri dev           # the real app (Rust backend)
```

Browser preview parameters: `?view=popover|settings|onboarding`, `&theme=light|dark`,
`&scenario=onboarding|massdelete|offline|signedout|paused|error|unavailable|idle`, and
`transfers|transfers-night|transfers-paused` for the transfer list (every state, uploads held for
tonight, paused uploads).

The Rust backend falls back to a mock service when `DRIVE_UI_MOCK=1` is set, or in debug
builds when the service can't be reached. `DRIVE_UI_MOCK=onboarding` starts not set up;
`DRIVE_UI_MOCK_MASS_DELETE=1` adds a pending mass-delete confirmation;
`DRIVE_UI_MOCK_TRANSFERS=1` starts with a long transfer list (one item in every state). Release builds never
use the mock.

## Sidecar

Bundles ship the service as `externalBin` `binaries/drive`. Before `pnpm tauri build` (or
`pnpm tauri dev` without the mock), build and stage it:

```sh
scripts/prepare-sidecar.sh [--debug] [--target <triple>]
scripts/prepare-sidecar.ps1 [-Debug] [-Target <triple>]
```

The main binary is named `drive-app` (`mainBinaryName`) so it doesn't collide with the
`drive` service binary on case-insensitive file systems.

To type-check the Rust crate without a staged sidecar, drop `externalBin` for that build:
`TAURI_CONFIG='{"bundle":{"externalBin":[]}}' cargo check -p drive-app`.
