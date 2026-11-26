# `fuzzyland`

A fast, theme-agnostic, single-binary **Rust** shell daemon and CLI for **Hyprland + Waybar + Fuzzel**.

Provides interactive Fuzzel menus, streaming Waybar status widgets, and desktop controls using native Linux protocol integrations (D-Bus via `zbus`, PulseAudio/PipeWire via `libpulse-binding`, Linux `/dev/i2c-*` DDC/CI, `/sys/class/drm` EDID, `/dev/rfkill`, and direct Hyprland Unix sockets) while keeping memory usage low and startup instant.

## Features

- **Single binary (`fuzzyland`)** — runs both as a persistent user daemon (`fuzzyland daemon`) and as thin CLI subcommands.
- **Daemon + transparent standalone fallback** — CLI commands talk to `$XDG_RUNTIME_DIR/fuzzyland/daemon.sock` (newline-delimited JSON) for instant warm-cache responses and streaming Waybar JSON updates. If the daemon is not running, every subcommand falls back transparently to direct execution.
- **Centralized menu coordination** — invoking an open menu toggles it closed (`SIGTERM`); invoking a different menu closes the previous one first (covering both `fuzzel` and `slurp`).
- **Theme-agnostic by default** — zero hardcoded theme colors. Pango tooltips inherit foreground color with alpha dimming; `slurp` uses neutral defaults. Optional theme presets (such as [examples/catppuccin-mocha.toml](examples/catppuccin-mocha.toml)) live in `~/.config/fuzzyland/config.toml`.

## Subsystems & Native Backends

| Subsystem | Native Backend | Optional External Tools |
| --- | --- | --- |
| **Network** | `org.freedesktop.NetworkManager` (`zbus`) | `nm-connection-editor`, `wl-copy` |
| **Bluetooth** | `org.bluez` (`zbus` + embedded `Agent1`) + `/dev/rfkill` | `wl-copy` |
| **Battery & Power Profiles** | `org.freedesktop.UPower` + `PowerProfiles` (`zbus`) | None |
| **Audio Output & Input** | `libpulse-binding` (`pipewire-pulse` / PulseAudio) + `systemd1` (`zbus`) | `easyeffects` *(when installed)* |
| **External Backlight** | Linux `/dev/i2c-*` DDC/CI (VCP `0x10`) + `/sys/class/drm` EDID + Hyprland `socket2` | None |
| **Power / Session** | Live-cached `wlogout` layout (`~/.config/wlogout/layout`) | `wlogout` layout actions *(optional)* |
| **Hyprland IPC** | Direct Unix sockets (`.socket.sock` & `.socket2.sock`) | None |
| **Notifications** | `org.freedesktop.Notifications` (`zbus`) | None |
| **Media** | `org.mpris.MediaPlayer2.*` (`zbus`) | None |
| **Clipboard** | `MenuCoordinator` + Hyprland cursor output | `cliphist`, `wl-copy` |
| **Screenshots** | Hyprland IPC + native PNG IHDR parser + D-Bus thumbnail notify | `grim`, `slurp`, `wl-copy` |
| **Menu UI** | Managed by `MenuCoordinator` | `fuzzel` |

## Build & Install

### Prerequisites

- Rust toolchain (`rustc` / `cargo`)
- `libpulse` development headers (`pkg-config --modversion libpulse`)
- Runtime tools as needed: `fuzzel`, `wl-clipboard` (`wl-copy`), `cliphist` (clipboard), `grim` & `slurp` (screenshots)
- For DDC/CI external monitor brightness without root, ensure your user is in the `i2c` group with `/dev/i2c-*` udev permissions.

### Compile & Install

```bash
cargo build --release
install -Dm755 target/release/fuzzyland ~/.local/bin/fuzzyland
```

### Enable the User Daemon (systemd)

```bash
install -Dm644 examples/fuzzyland.service ~/.config/systemd/user/fuzzyland.service
systemctl --user daemon-reload
systemctl --user enable --now fuzzyland.service
```

Or start directly from `hyprland.conf`:

```ini
exec-once = fuzzyland daemon
```

## CLI Reference

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

## Configuration

`fuzzyland` works with zero configuration. To customize behavior, colors, or Nerd Font icons, create `~/.config/fuzzyland/config.toml` (all fields are optional and default to the values below):

```toml
[fuzzel]
binary = "fuzzel"
anchor_margin_x = 10
anchor_margin_y = 10

[tooltip]
# Empty strings inherit Waybar's foreground color and use Pango alpha dimming.
text = ""
subtext = ""
muted = ""
separator = ""

[battery]
warning_threshold = 25
critical_threshold = 10

[backlight]
debounce_ms = 120
poll_interval_secs = 30
i2c_settle_ms = 40
internal_prefixes = ["eDP-", "LVDS-", "DSI-"]

[power]
confirm = ["logout", "reboot", "shutdown"]
# layout_path = "~/.config/wlogout/layout"

[screenshot]
save_dir = "~/Pictures/Screenshots"
slurp_bg = "#00000080"
slurp_border = "#ffffffcc"
slurp_selection = "#ffffff22"
slurp_box = "#000000bb"
slurp_font = "monospace"

[media]
max_track_len = 44

[icons]
# Any Nerd Font icon used across menus and status modules can be overridden here.
```

See [examples/catppuccin-mocha.toml](examples/catppuccin-mocha.toml) for a drop-in Catppuccin Mocha preset.

## Waybar, Hyprland & Hyprlock Integration

### Waybar

See [examples/waybar-config.jsonc](examples/waybar-config.jsonc). Both `custom/battery` and `custom/external-backlight` stream live JSON updates from the daemon without polling intervals or `RTMIN` signals:

```jsonc
"custom/battery": {
  "exec": "fuzzyland battery status",
  "return-type": "json",
  "on-click": "fuzzyland battery menu"
},
"custom/external-backlight": {
  "exec": "fuzzyland backlight status",
  "return-type": "json",
  "on-scroll-up": "fuzzyland backlight adjust +5",
  "on-scroll-down": "fuzzyland backlight adjust -5",
  "on-click": "fuzzyland backlight refresh"
}
```

### Hyprland Keybinds

```ini
bind = SUPER, V, exec, fuzzyland clipboard
bind = SUPER SHIFT, V, exec, fuzzyland clipboard --clear
bind = , Print, exec, fuzzyland screenshot
bind = SHIFT, Print, exec, fuzzyland screenshot region
bind = SUPER, Print, exec, fuzzyland screenshot active-window --save
```

### Hyprlock Media Widgets

Use `fuzzyland media` inside `hyprlock.conf` labels and click handlers:

```ini
text = cmd[update:1000] fuzzyland media track
text = cmd[update:1000] fuzzyland media previous
text = cmd[update:1000] fuzzyland media toggle
text = cmd[update:1000] fuzzyland media next
```

Control actions (`fuzzyland media play-pause`, `prev`, `skip`) can be bound to click handlers or media keys.
