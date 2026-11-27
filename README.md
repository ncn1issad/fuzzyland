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
| **Backlight** | External monitors via DDC/CI, built-in displays via logind and `/sys/class/backlight` | None |
| **Power / Session** | Live-cached `wlogout` layout (`~/.config/wlogout/layout`) | `wlogout` layout actions *(optional)* |
| **Hyprland IPC** | Direct Unix sockets (`.socket.sock` & `.socket2.sock`) | None |
| **Notifications** | `org.freedesktop.Notifications` (`zbus`) | None |
| **Media** | `org.mpris.MediaPlayer2.*` (`zbus`) | None |
| **Clipboard** | `MenuCoordinator` + Hyprland cursor output | `cliphist`, `wl-copy`, `wl-paste` (history collection) |
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

Or start it from `hyprland.lua`:

```lua
hl.on("hyprland.start", function()
  hl.dispatch(hl.dsp.exec_cmd("fuzzyland daemon"))
end)
```

For older Hyprland versions that use `hyprland.conf`:

```ini
exec-once = fuzzyland daemon
```

## CLI Reference

```text
fuzzyland
├── daemon                          Run the persistent background daemon
├── network [--toggle-wifi]         Open Network menu, or toggle Wi-Fi
├── bluetooth [--toggle-power]      Open Bluetooth menu, or toggle adapter power
├── battery
│   ├── status [--once]             Stream Waybar JSON (or single snapshot with --once)
│   ├── menu                        Open Power Profile menu
│   └── profile <name>              Set power profile directly
├── audio
│   ├── output [ACTION]             Open Output & EQ menu, or use --toggle-eq, --toggle-mute, --volume <DELTA>
│   └── input  [ACTION]             Open Input & Mic FX menu, or use --toggle-fx, --toggle-mute, --volume <DELTA>
├── backlight
│   ├── status [--once]             Stream Waybar JSON for DDC/CI external monitors
│   ├── adjust <DELTA>              Adjust external monitor brightness (e.g. +5, -5)
│   ├── refresh                     Force re-probe of external monitors
│   └── internal [--device <NAME>]
│       ├── adjust <DELTA>          Adjust built-in display brightness (e.g. +5, -5)
│       ├── dim [--percent <PCT>]   Dim the display and save its previous brightness (default 10%)
│       └── restore                 Restore the brightness saved by dim
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

[audio]
max_volume = 100 # Percent of normal volume, for both input and output adjustments.
unmute_on_volume_up = false # Set to true to unmute the output when raising its volume.

[battery]
warning_threshold = 25
critical_threshold = 10

[backlight]
debounce_ms = 120
poll_interval_secs = 30
i2c_settle_ms = 40
internal_prefixes = ["eDP-", "LVDS-", "DSI-"]

[clipboard]
watch = false

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

Set `watch = true` under `[clipboard]` and restart `fuzzyland daemon` to collect text and image history automatically. This uses `wl-paste` and `cliphist`. If you enable it, remove any existing `wl-paste --watch cliphist store` autostart entries and stop those watchers to avoid duplicate collection. The daemon needs the Wayland session environment, including `WAYLAND_DISPLAY`.

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

For `hyprland.lua`:

```lua
hl.bind("SUPER + V", hl.dsp.exec_cmd("fuzzyland clipboard"))
hl.bind("SUPER + SHIFT + V", hl.dsp.exec_cmd("fuzzyland clipboard --clear"))
hl.bind("Print", hl.dsp.exec_cmd("fuzzyland screenshot"))
hl.bind("SHIFT + Print", hl.dsp.exec_cmd("fuzzyland screenshot region"))
hl.bind("SUPER + Print", hl.dsp.exec_cmd("fuzzyland screenshot active-window --save"))
hl.bind("XF86AudioRaiseVolume", hl.dsp.exec_cmd("fuzzyland audio output --volume +5"), { repeating = true })
hl.bind("XF86AudioLowerVolume", hl.dsp.exec_cmd("fuzzyland audio output --volume -5"), { repeating = true })
hl.bind("XF86AudioMute", hl.dsp.exec_cmd("fuzzyland audio output --toggle-mute"))
hl.bind("XF86AudioMicMute", hl.dsp.exec_cmd("fuzzyland audio input --toggle-mute"))
hl.bind("XF86MonBrightnessUp", hl.dsp.exec_cmd("fuzzyland backlight internal adjust +5"), { repeating = true })
hl.bind("XF86MonBrightnessDown", hl.dsp.exec_cmd("fuzzyland backlight internal adjust -5"), { repeating = true })
```

For `hyprland.conf`:

```ini
bind = SUPER, V, exec, fuzzyland clipboard
bind = SUPER SHIFT, V, exec, fuzzyland clipboard --clear
bind = , Print, exec, fuzzyland screenshot
bind = SHIFT, Print, exec, fuzzyland screenshot region
bind = SUPER, Print, exec, fuzzyland screenshot active-window --save
binde = , XF86AudioRaiseVolume, exec, fuzzyland audio output --volume +5
binde = , XF86AudioLowerVolume, exec, fuzzyland audio output --volume -5
bind = , XF86AudioMute, exec, fuzzyland audio output --toggle-mute
bind = , XF86AudioMicMute, exec, fuzzyland audio input --toggle-mute
binde = , XF86MonBrightnessUp, exec, fuzzyland backlight internal adjust +5
binde = , XF86MonBrightnessDown, exec, fuzzyland backlight internal adjust -5
```

For idle dimming, put `fuzzyland backlight internal dim` in your Hypridle listener's `on-timeout` and `fuzzyland backlight internal restore` in its `on-resume`. To select a specific backlight, pass the same `--device <NAME>` to both commands.

### Hyprlock Media Widgets

Use `fuzzyland media` inside `hyprlock.conf` labels and click handlers:

```ini
text = cmd[update:1000] fuzzyland media track
text = cmd[update:1000] fuzzyland media previous
text = cmd[update:1000] fuzzyland media toggle
text = cmd[update:1000] fuzzyland media next
```

Control actions (`fuzzyland media play-pause`, `prev`, `skip`) can be bound to click handlers or media keys.
