# `fuzzyland` — Specification & Execution Plan

A fast, theme-agnostic, single-binary **Rust** shell daemon and CLI for **Hyprland + Waybar + Fuzzel**. Replaces the Python/Bash prototype scripts from `../hypr-mocha-dots/bin/` (excluding wallpapers and `desktop-context-menu`) with native Linux protocol integrations while keeping external dependencies minimal and low-maintenance.

---

## Core Design

- **Single binary (`fuzzyland`)**: runs both as a persistent session daemon (`fuzzyland daemon`) and as thin CLI subcommands plugged into Waybar, Hyprland keybinds, and Hyprlock.
- **Daemon + transparent fallback**: CLI subcommands talk to `$XDG_RUNTIME_DIR/fuzzyland/daemon.sock` (newline-delimited JSON) for instant startup from warm in-memory state and streaming Waybar updates. If the daemon is not running, commands fall back transparently to direct execution.
- **Runtime**: `tokio` (`current_thread` + blocking pool for I2C DDC/CI & `libpulse`), single-instance advisory `flock` on `$XDG_RUNTIME_DIR/fuzzyland/daemon.lock`, target ~5–12 MB RSS.
- **Theme-agnostic by default**: zero hardcoded Catppuccin hex colors. Tooltips use inherited Pango foreground + alpha dimming; `slurp` uses neutral defaults; optional overrides live in `~/.config/fuzzyland/config.toml`.
- **Lean dependencies**: `tokio`, `zbus`, `clap` (derive), `serde`, `serde_json`, `toml`, `libpulse-binding`, `libc`, `dirs`, `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`.
- **Non-goals**: no wallpaper rotation, no desktop right-click context menu, no OSD popups, no notification center, no per-app audio mixer.

### Protocol & External Tool Matrix

| Subsystem | Native Backend | External Tools Used |
| --- | --- | --- |
| **Network** | `org.freedesktop.NetworkManager` (`zbus`) | `nm-connection-editor` *(optional GUI button)*, `wl-copy` |
| **Bluetooth** | `org.bluez` (`zbus`, `GetManagedObjects` + embedded `Agent1`) + `/dev/rfkill` | `wl-copy` |
| **Battery & Power Profiles** | `org.freedesktop.UPower` + `org.freedesktop.UPower.PowerProfiles` (`zbus`) | None |
| **Audio Output & Input** | `libpulse-binding` (`pipewire-pulse` / PulseAudio) + `org.freedesktop.systemd1` (`zbus`) | `easyeffects` *(optional, only when installed)* |
| **External Backlight** | Direct Linux `/dev/i2c-*` DDC/CI (VCP `0x10`) + `/sys/class/drm` EDID + Hyprland `socket2` | None |
| **Power / Session** | `org.freedesktop.login1` (`zbus`) + Hyprland IPC | `hyprlock` *(fallback if `hypridle` inactive)*, `uwsm` *(if active)* |
| **Hyprland Queries & Events** | Direct Unix sockets (`.socket.sock` & `.socket2.sock`) | None |
| **Notifications** | `org.freedesktop.Notifications` (`zbus`, synchronous replace hints) | None |
| **Media** | `org.mpris.MediaPlayer2.*` (`zbus`) | None |
| **Clipboard** | Daemon Menu Coordinator + Hyprland cursor output | `cliphist`, `wl-copy` |
| **Screenshots** | Hyprland socket geometry + native PNG IHDR parser + D-Bus thumbnail notify | `grim`, `slurp`, `wl-copy` |
| **Menu UI** | Spawned & managed by `MenuCoordinator` | `fuzzel` |

### CLI Interface

```text
fuzzyland
├── daemon                          Run the persistent background daemon
├── network                         Open Wi-Fi / Ethernet / VPN / Hotspot menu
├── bluetooth [--toggle-power]      Open Bluetooth menu, or toggle adapter power
├── battery
│   ├── status [--once]             Stream Waybar JSON (or single snapshot with --once)
│   ├── menu                        Open Power Profile menu
│   └── profile <name>              Set power profile directly
├── audio
│   ├── output [--toggle-eq]        Open Output & EasyEffects EQ menu, or toggle EQ bypass
│   └── input  [--toggle-fx]        Open Input & Mic FX menu, or toggle FX bypass
├── backlight
│   ├── status [--once]             Stream Waybar JSON for DDC/CI external monitors
│   ├── adjust <DELTA>              Adjust external monitor brightness (e.g. +5, -5)
│   └── refresh                     Force re-probe of external monitors
├── power                           Open Session / Power menu
├── clipboard [--clear]             Open Clipboard History menu, or confirm-clear
├── screenshot [MODE] [FLAGS]       MODE: menu|region|window|active-window|monitor|all
│                                   FLAGS: -s/--save, --dir <PATH>, --delay <SECS>
└── media <ACTION>                  ACTION: track|previous|toggle|next|play-pause|prev|skip
```

### Target Crate Layout

```text
fuzzyland/
├── Cargo.toml
├── README.md
├── TODO.md
├── examples/
│   ├── waybar-config.jsonc
│   ├── fuzzyland.service
│   └── catppuccin-mocha.toml
└── src/
    ├── main.rs
    ├── config.rs
    ├── daemon/
    │   ├── mod.rs
    │   ├── ipc.rs
    │   └── menu.rs
    ├── core/
    │   ├── mod.rs
    │   ├── fuzzel.rs
    │   ├── hyprland.rs
    │   ├── notify.rs
    │   ├── clipboard.rs
    │   └── waybar.rs
    └── modules/
        ├── mod.rs
        ├── network.rs
        ├── bluetooth.rs
        ├── battery.rs
        ├── audio.rs
        ├── backlight.rs
        ├── power.rs
        ├── clipboard.rs
        ├── screenshot.rs
        └── media.rs
```

---

## Sequence 1 — Foundation & Shared Contracts *(Sequential — Must Complete First)*

Build and freeze the shared core, configuration, daemon skeleton, IPC protocol, and stubbed module signatures so `cargo check` passes cleanly before parallel module work begins.

- [x] **1.1 Toolchain & Cargo Scaffold**
  - Ensure Rust (`rustc` / `cargo`) is installed (`libpulse` pkg-config is already present).
  - Initialize `git`, `.gitignore`, and `Cargo.toml` with pinned dependencies (`tokio`, `zbus`, `clap`, `serde`, `serde_json`, `toml`, `libpulse-binding`, `libc`, `dirs`, `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`).
- [x] **1.2 Configuration (`src/config.rs`)**
  - Optional `~/.config/fuzzyland/config.toml` with zero-config theme-agnostic defaults:
    - `[fuzzel]` (`binary`, `anchor_margin_x`, `anchor_margin_y`)
    - `[tooltip]` (`text`, `subtext`, `muted`, `separator` — empty strings mean inherited foreground + Pango `alpha`)
    - `[battery]` (`warning_threshold = 25`, `critical_threshold = 10`)
    - `[backlight]` (`debounce_ms = 120`, `poll_interval_secs = 30`, `i2c_settle_ms = 40`, `internal_prefixes = ["eDP-", "LVDS-", "DSI-"]`)
    - `[power]` (`confirm = ["logout", "reboot", "shutdown"]`, `hide_unavailable = false`, optional `lock_cmd` / `logout_cmd`)
    - `[screenshot]` (`save_dir = "~/Pictures/Screenshots"`, neutral `slurp_*` defaults, overridable via `SCREENSHOT_SLURP_*` env vars)
    - `[media]` (`max_track_len = 44`)
    - `[icons]` (Nerd Font defaults matching `hypr-mocha-dots`, all overridable)
- [x] **1.3 Shared Core Helpers (`src/core/`)**
  - `core/waybar.rs`: `WaybarOutput` struct (`text`, `tooltip`, `class`, `percentage`), Pango HTML escaping, and theme-agnostic tooltip formatting helpers (`header`, `subtext`, `divider`, `footer`).
  - `core/hyprland.rs`: direct Unix socket client for `.socket.sock` (`cursorpos`, `monitors`, `clients`, `activewindow`, `dispatch`, `detect_cursor_output()` handling monitor scale & transform) and `.socket2.sock` async event stream (`monitoradded`, `monitorremoved`), with graceful fallback outside Hyprland.
  - `core/notify.rs`: native `org.freedesktop.Notifications` (`zbus`) client supporting `app_name`, `summary`, `body`, `icon`, `urgency`, and `x-canonical-private-synchronous` / `replaces_id`.
  - `core/fuzzel.rs`: typed `fuzzel` runner (`select`, `input`, `password`, custom keybinds, `--select-index`, `--counter`, `--mesg`, anchors/margins/widths) wired into `MenuCoordinator`.
  - `core/clipboard.rs`: `wl-copy` helper (text and `image/png` with `Stdio::null()` stdout/stderr so `wl-copy`'s forked daemon never blocks).
- [x] **1.4 Daemon, IPC & Menu Coordinator (`src/daemon/`)**
  - `daemon/menu.rs`: central `MenuCoordinator` tracking the single active interactive `(MenuKind, pid)` in memory — invoking the same menu toggles it closed (`SIGTERM`); invoking another menu closes the previous one first (works for both `fuzzel` and `slurp`).
  - `daemon/ipc.rs`: newline-delimited JSON RPC & streaming subscription protocol over `$XDG_RUNTIME_DIR/fuzzyland/daemon.sock` (request/response + continuous NDJSON streams for `battery status` and `backlight status`).
  - `daemon/mod.rs`: `flock` on `daemon.lock`, `tokio` `current_thread` runtime, shared `DaemonState`, `SIGTERM`/`SIGINT` cleanup, socket server loop.
- [x] **1.5 CLI & Module Contracts (`src/main.rs`, `src/modules/mod.rs`)**
  - Full `clap` CLI matching the subcommand tree above.
  - Client dispatch with automatic standalone fallback when `daemon.sock` is unavailable.
  - Stubbed `src/modules/{network,bluetooth,battery,audio,backlight,power,clipboard,screenshot,media}.rs` with agreed function/state traits so `cargo check` compiles cleanly.

---

## Sequence 2 — 9 Independent Modules *(Fully Parallelizable)*

Each task owns a single isolated module file (`src/modules/<name>.rs`) against the frozen Sequence 1 contracts. Reference behavior lives in `../hypr-mocha-dots/bin/`.

- [ ] **2.1 Network (`src/modules/network.rs`)** — *replaces `waybar-network-menu`*
  - Pure `zbus` client for `org.freedesktop.NetworkManager` (`.Device`, `.Device.Wireless`, `.AccessPoint`, `.Connection.Active`, `.IP4Config`, `.IP6Config`, `.Settings`, `.Settings.Connection`).
  - Global networking toggle (`Enable`) & Wi-Fi radio toggle (`WirelessEnabled`).
  - Ethernet list & actions (connected submenu: Info / Disconnect / Edit; disconnected: auto-connect single profile or picker; unplugged toast).
  - Wi-Fi list deduplicated by SSID (`active desc, signal desc, ssid asc`), 5-step signal + lock icons.
  - Active Wi-Fi actions (Info, Disconnect, Forget w/ confirm); inactive Wi-Fi connect (match saved profile by SSID first; inspect AP `Flags`/`WpaFlags`/`RsnFlags` for Open vs. `wpa-psk` vs. `sae`/WPA3; prompt password via Fuzzel → `AddAndActivateConnection`).
  - Rescan (`RequestScan({})` awaiting `LastScan` update with ~3s timeout), Hidden Wi-Fi (`hidden = true`).
  - Hotspot (create/reuse single `Hotspot` AP profile with `ipv4.method = "shared"`, WPA2 PSK ≥8 chars; active view with copyable SSID & PSK via `GetSecrets` + Stop).
  - VPN / WireGuard toggle submenu + `nm-connection-editor` launcher.
  - Connection Info view (Interface, Type, Connection, MAC, IPv4, Gateway, DNS, global IPv6 excluding `fe80::`, Wi-Fi Signal/Bitrate/Security — selecting copies value).

- [ ] **2.2 Bluetooth (`src/modules/bluetooth.rs`)** — *replaces `waybar-bluetooth-menu`*
  - Pure `zbus` client for `org.bluez` (`ObjectManager::GetManagedObjects`, `Adapter1`, `Device1`, `Battery1`, `AgentManager1` + daemon-registered `NoInputNoOutput` `Agent1`).
  - Native `/dev/rfkill` reader/writer (detect soft/hard block, soft-unblock on power-on).
  - Daemon state cache updated reactively via `InterfacesAdded`, `InterfacesRemoved`, `PropertiesChanged`.
  - `--toggle-power` action + full Fuzzel menu: sorted `(connected desc, paired desc, alias asc)`, device icons (`Icon` property + alias heuristics), `Battery1.Percentage`, filter nameless MAC-only BLE ads during scan.
  - Actions: Connect, Disconnect, Pair + Trust + Connect (new devices), Scan 5s (`StartDiscovery` / `StopDiscovery`), Manage Paired submenu, Device Info (copyable rows), Trust/Untrust, Forget w/ confirm (`RemoveDevice`), Discoverable toggle, Power On/Off.

- [ ] **2.3 Battery & Power Profiles (`src/modules/battery.rs`)** — *replaces `waybar-battery-status` & `waybar-battery-menu`*
  - Pure `zbus` client for `org.freedesktop.UPower` (`EnumerateDevices`, `DisplayDevice`, `DeviceAdded`, `DeviceRemoved`, `PropertiesChanged`) and `org.freedesktop.UPower.PowerProfiles` (`ActiveProfile`, `Profiles`, `PropertiesChanged`).
  - `battery status` (streaming via daemon `watch` channel, plus `--once`): primary battery (`PowerSupply && IsPresent`, fallback `DisplayDevice`) + peripheral batteries in tooltip (mouse, keyboard, headset, headphones, gamepad, phone, tablet, stylus, speaker).
  - Emits Waybar JSON (`text`, `tooltip`, `percentage`, `class`: `charging`/`plugged`/`warning`/`critical`), formats `TimeToEmpty`/`TimeToFull` cleanly (e.g. `3h 24m`), includes energy rate (W), health (%), active power profile, theme-agnostic Pango tooltip.
  - `battery menu` & `battery profile <name>`: ordered `power-saver`, `balanced`, `performance` with active checkmark; sets `ActiveProfile` over D-Bus and notifies.

- [ ] **2.4 Audio Output & Input (`src/modules/audio.rs`)** — *replaces `waybar-output-menu` & `waybar-input-menu`*
  - Native `libpulse-binding` client (`pipewire-pulse` / PulseAudio) + `org.freedesktop.systemd1` (`StartUnit("easyeffects.service", "replace")`).
  - **Output (`audio output`, `--toggle-eq`):** list available sinks (exclude `easyeffects_sink` and unavailable ports, always include default sink), device-type icons, switch default sink + move active non-EasyEffects `sink-inputs`. Optional EasyEffects section (when on `PATH`): output preset submenu (`~/.local/share/easyeffects/output` & `~/.config/easyeffects/output`), bypass toggle, start service, open GUI.
  - **Input (`audio input`, `--toggle-fx`):** list available sources (exclude `.monitor`, `monitor_of`, `easyeffects_source`, unavailable ports), switch default source + move active non-EasyEffects `source-outputs` on valid capture sources. Optional EasyEffects section: input preset submenu, mic monitoring toggle, bypass toggle, start service, open GUI.

- [ ] **2.5 External Backlight (`src/modules/backlight.rs`)** — *replaces `waybar-external-backlight`*
  - Native Linux `/dev/i2c-*` DDC/CI VCP `0x10` (Luminance) get/set with DDC checksum, configurable settle delay (`40ms`) & retries, plus `/sys/class/drm/card*-*` (`status`, `edid`, `ddc/i2c-*`) discovery & EDID fingerprinting (filtering internal `eDP-`, `LVDS-`, `DSI-`).
  - Subscribes to Hyprland `socket2` (`monitoradded` / `monitorremoved`) with 3-attempt 1.5s hotplug retry backoff.
  - Periodic idle OSD sync (default 30s, skipped while pending or within 3s grace window).
  - `backlight status` (streaming via daemon `watch` channel, plus `--once`): emits `{"text": ""}` when no DDC external monitor is present, otherwise icon + % + theme-agnostic Pango tooltip listing all external monitors.
  - `backlight adjust <DELTA>`: updates state & pushes Waybar JSON immediately, debounces (120 ms), writes VCP `0x10` serially on blocking I2C worker.
  - `backlight refresh`: forces re-probe.

- [ ] **2.6 Power Menu (`src/modules/power.rs`)** — *replaces `waybar-power-menu`*
  - Native `org.freedesktop.login1` (`Manager` & `Session`) + `org.freedesktop.systemd1` + Hyprland IPC.
  - Ordered actions: Lock, Suspend, Hibernate, Logout, Reboot, Shutdown (optional `hide_unavailable` via `CanSuspend`/`CanHibernate`).
  - Confirmation submenu for configured `confirm` actions (`logout`, `reboot`, `shutdown`).
  - Lock calls `login1` `Session.Lock()` if `hypridle.service` is active, else spawns `hyprlock` (or `config.power.lock_cmd`); Logout uses `uwsm stop` if under UWSM else Hyprland `dispatch exit` (or `config.power.logout_cmd`).

- [ ] **2.7 Clipboard (`src/modules/clipboard.rs`)** — *replaces `waybar-clipboard-menu`*
  - Backed by `cliphist` (`list`, `decode`, `delete`, `wipe`) + `wl-copy` (`--clear` on wipe).
  - Fuzzel menu anchored `top`, width 56, `--no-sort`, `--counter`, custom binds (`Enter` copy, `Shift+Delete` delete & reopen preserving index, `Ctrl+Shift+Delete` confirm clear), distinct icons for text vs. binary/image previews, bottom `Clear history…` row.
  - `--clear` opens confirmation dialog directly.

- [ ] **2.8 Screenshot (`src/modules/screenshot.rs`)** — *replaces `screenshot`*
  - Hyprland IPC socket (`monitors`, `cursorpos`, `clients`, `activewindow`) for monitor detection & visible window geometry boxes (mapped, non-hidden, active/special workspaces).
  - Modes: `menu`, `region`, `window`, `active-window`, `monitor`, `all`; flags `-s/--save`, `--dir`, `--delay`.
  - Capture via `grim -`, selection via `slurp` (managed by `MenuCoordinator`, theme-agnostic configurable colors), copy PNG via `wl-copy --type image/png`, parse PNG IHDR dimensions natively in Rust, write timestamped PNG or `$XDG_RUNTIME_DIR/fuzzyland/screenshot-preview.png`, send thumbnail toast via D-Bus `org.freedesktop.Notifications`.
  - Menu includes live `Save to file: On/Off` toggle and post-Fuzzel fade delay (250 ms monitor/all, 50 ms region/window).

- [ ] **2.9 Media (`src/modules/media.rs`)** — *replaces `hyprlock-media`*
  - Native `org.mpris.MediaPlayer2.*` session D-Bus client (prefers `Playing` over `Paused`, tracking most recently active player in daemon).
  - Actions: `track` (Pango-escaped `title — artist`, UTF-8 char-safe truncation at `max_track_len`, empty when stopped/none), `previous` (`⏮`), `toggle` (`⏸`/`▶`), `next` (`⏭`), plus playback controls (`play-pause`, `prev`, `skip`).

---

## Sequence 3 — Integration, Verification & Docs *(Sequential — Final Pass)*

- [ ] **3.1 Wire Modules into Daemon & CLI**
  - Connect all 9 modules into `src/daemon/mod.rs` (warm caches, background D-Bus/Hyprland/I2C tasks, IPC dispatch) and `src/main.rs` (client & standalone fallback paths).
- [ ] **3.2 Build, Lint & Tests**
  - Unit tests for EDID/DDC checksums, Pango escaping & tooltip rendering, battery time/icon formatting, PNG IHDR parsing, Hyprland monitor hit-testing (scale + transform), Wi-Fi SSID deduplication/security mapping, config deserialization.
  - Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo build --release`.
- [ ] **3.3 Examples & Documentation**
  - `examples/waybar-config.jsonc` (showing streaming `custom/battery` and `custom/external-backlight` with no `interval`/`signal`, plus all `fuzzyland` click bindings).
  - `examples/fuzzyland.service` (systemd user service for `fuzzyland daemon`).
  - `examples/catppuccin-mocha.toml` (optional theme preset matching `hypr-mocha-dots`).
  - `README.md` (install, setup, config reference, Waybar/Hyprland/Hyprlock migration guide).
- [ ] **3.4 Live Session Smoke Test**
  - Verify `fuzzyland daemon` startup/RSS, streaming Waybar JSON output, menu toggling/dismissal, and subcommands against the running Hyprland session.
