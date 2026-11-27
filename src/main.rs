mod config;
mod core;
mod daemon;
mod modules;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use config::Config;
use daemon::ipc::{try_send_request, IpcRequest};
use daemon::AppContext;
use modules::audio::AudioTarget;
use modules::screenshot::ScreenshotArgs;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "fuzzyland",
    version,
    about = "Fast, theme-agnostic shell daemon and CLI for Hyprland, Waybar, and Fuzzel"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the persistent background daemon
    Daemon,
    /// Open Wi-Fi / Ethernet / VPN / Hotspot menu, or toggle Wi-Fi
    Network {
        /// Toggle Wi-Fi without opening the menu
        #[arg(long)]
        toggle_wifi: bool,
    },
    /// Open Bluetooth menu, or toggle adapter power
    Bluetooth {
        #[arg(long)]
        toggle_power: bool,
    },
    /// Battery status and power profile controls
    Battery {
        #[command(subcommand)]
        action: BatteryCommand,
    },
    /// Audio output/input menus, volume, mute, and EasyEffects toggles
    Audio {
        #[command(subcommand)]
        action: AudioCommand,
    },
    /// Backlight controls for external monitors and built-in displays
    Backlight {
        #[command(subcommand)]
        action: BacklightCommand,
    },
    /// Open Session / Power menu
    Power,
    /// Open Clipboard History menu, or confirm-clear
    Clipboard {
        #[arg(long)]
        clear: bool,
    },
    /// Take or choose a screenshot
    Screenshot {
        #[arg(default_value = "menu", value_parser = ["menu", "region", "window", "active-window", "monitor", "all"])]
        mode: String,
        #[arg(short = 's', long)]
        save: bool,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long, default_value_t = 0.0)]
        delay: f64,
    },
    /// Query or control MPRIS media players
    Media {
        #[arg(value_parser = ["track", "previous", "toggle", "next", "play-pause", "prev", "skip"])]
        action: String,
    },
}

#[derive(Debug, Subcommand)]
enum BatteryCommand {
    /// Stream Waybar JSON (or print a single snapshot with --once)
    Status {
        #[arg(long)]
        once: bool,
    },
    /// Open Power Profile menu
    Menu,
    /// Set power profile directly
    Profile { name: String },
}

#[derive(Debug, Subcommand)]
enum AudioCommand {
    /// Open Output & EasyEffects EQ menu, or toggle EQ bypass
    Output {
        #[arg(long, conflicts_with_all = ["toggle_mute", "volume"])]
        toggle_eq: bool,
        #[command(flatten)]
        controls: AudioControls,
    },
    /// Open Input & Mic FX menu, or toggle FX bypass
    Input {
        #[arg(long, conflicts_with_all = ["toggle_mute", "volume"])]
        toggle_fx: bool,
        #[command(flatten)]
        controls: AudioControls,
    },
}

#[derive(Debug, Args)]
struct AudioControls {
    /// Toggle the default device's mute state
    #[arg(long, conflicts_with = "volume")]
    toggle_mute: bool,
    /// Adjust volume by a signed number of percentage points (e.g. +5, -5)
    #[arg(long, value_name = "DELTA", allow_negative_numbers = true)]
    volume: Option<i32>,
}

impl AudioControls {
    fn ipc_request(&self, target: AudioTarget, menu_request: IpcRequest) -> IpcRequest {
        match self.volume {
            Some(delta) => IpcRequest::AudioVolume { target, delta },
            None if self.toggle_mute => IpcRequest::AudioToggleMute { target },
            None => menu_request,
        }
    }
}

#[derive(Debug, Subcommand)]
enum BacklightCommand {
    /// Stream Waybar JSON for DDC/CI external monitors (or single snapshot with --once)
    Status {
        #[arg(long)]
        once: bool,
    },
    /// Adjust external monitor brightness (e.g. +5, -5)
    Adjust {
        #[arg(allow_hyphen_values = true)]
        delta: i32,
    },
    /// Force re-probe of external monitors
    Refresh,
    /// Adjust or temporarily dim a built-in display
    Internal {
        /// Name of a device in /sys/class/backlight
        #[arg(long, global = true)]
        device: Option<String>,
        #[command(subcommand)]
        action: InternalBacklightCommand,
    },
}

#[derive(Debug, Subcommand)]
enum InternalBacklightCommand {
    /// Adjust brightness by signed percentage points (e.g. +5, -5)
    Adjust {
        #[arg(allow_negative_numbers = true)]
        delta: i32,
    },
    /// Temporarily dim the display, saving its previous brightness
    Dim {
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
    },
    /// Restore the brightness saved by dim
    Restore,
}

fn init_tracing(is_daemon: bool) {
    let default_level = if is_daemon { "info" } else { "warn" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let is_daemon = matches!(cli.command, Command::Daemon);
    init_tracing(is_daemon);

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("Failed to start async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("Error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let config = Config::load();

    if matches!(cli.command, Command::Daemon) {
        daemon::run_daemon(config).await?;
        return Ok(ExitCode::SUCCESS);
    }

    let req = command_to_ipc(&cli.command);
    if let Some(code) = try_send_request(&req).await? {
        return Ok(if code == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(code as u8)
        });
    }

    // Transparent standalone fallback when the daemon is not running.
    let ctx = AppContext::new_standalone(config);
    run_standalone(&ctx, cli.command).await?;
    Ok(ExitCode::SUCCESS)
}

fn command_to_ipc(cmd: &Command) -> IpcRequest {
    match cmd {
        Command::Daemon => IpcRequest::Ping,
        Command::Network { toggle_wifi: true } => IpcRequest::NetworkToggleWifi,
        Command::Network { toggle_wifi: false } => IpcRequest::Network,
        Command::Bluetooth { toggle_power } => IpcRequest::Bluetooth {
            toggle_power: *toggle_power,
        },
        Command::Battery { action } => match action {
            BatteryCommand::Status { once } => IpcRequest::BatteryStatus { once: *once },
            BatteryCommand::Menu => IpcRequest::BatteryMenu,
            BatteryCommand::Profile { name } => IpcRequest::BatteryProfile { name: name.clone() },
        },
        Command::Audio { action } => match action {
            AudioCommand::Output {
                toggle_eq,
                controls,
            } => controls.ipc_request(
                AudioTarget::Output,
                IpcRequest::AudioOutput {
                    toggle_eq: *toggle_eq,
                },
            ),
            AudioCommand::Input {
                toggle_fx,
                controls,
            } => controls.ipc_request(
                AudioTarget::Input,
                IpcRequest::AudioInput {
                    toggle_fx: *toggle_fx,
                },
            ),
        },
        Command::Backlight { action } => match action {
            BacklightCommand::Status { once } => IpcRequest::BacklightStatus { once: *once },
            BacklightCommand::Adjust { delta } => IpcRequest::BacklightAdjust { delta: *delta },
            BacklightCommand::Refresh => IpcRequest::BacklightRefresh,
            BacklightCommand::Internal { device, action } => match action {
                InternalBacklightCommand::Adjust { delta } => IpcRequest::InternalBacklightAdjust {
                    delta: *delta,
                    device: device.clone(),
                },
                InternalBacklightCommand::Dim { percent } => IpcRequest::InternalBacklightDim {
                    percent: *percent,
                    device: device.clone(),
                },
                InternalBacklightCommand::Restore => IpcRequest::InternalBacklightRestore {
                    device: device.clone(),
                },
            },
        },
        Command::Power => IpcRequest::Power,
        Command::Clipboard { clear } => IpcRequest::Clipboard { clear: *clear },
        Command::Screenshot {
            mode,
            save,
            dir,
            delay,
        } => IpcRequest::Screenshot {
            mode: mode.clone(),
            save: *save,
            dir: dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
            delay: *delay,
        },
        Command::Media { action } => IpcRequest::Media {
            action: action.clone(),
        },
    }
}

async fn run_standalone(ctx: &std::sync::Arc<AppContext>, cmd: Command) -> Result<()> {
    match cmd {
        Command::Daemon => unreachable!(),
        Command::Network { toggle_wifi: true } => modules::network::toggle_wifi(ctx).await?,
        Command::Network { toggle_wifi: false } => modules::network::run_menu(ctx).await?,
        Command::Bluetooth { toggle_power } => {
            if toggle_power {
                modules::bluetooth::toggle_power(ctx).await?;
            } else {
                modules::bluetooth::run_menu(ctx).await?;
            }
        }
        Command::Battery { action } => match action {
            BatteryCommand::Status { once } => {
                if once {
                    let out = modules::battery::status_once(ctx).await?;
                    println!("{}", out.to_json_line());
                } else {
                    modules::battery::spawn_background(std::sync::Arc::clone(ctx));
                    let mut rx = ctx.battery.subscribe();
                    let mut stdout = std::io::stdout();
                    // Emit initial snapshot, then stream updates.
                    let initial = modules::battery::status_once(ctx).await?;
                    let _ = writeln!(stdout, "{}", initial.to_json_line());
                    let _ = stdout.flush();
                    while rx.changed().await.is_ok() {
                        let line = rx.borrow_and_update().to_json_line();
                        if writeln!(stdout, "{line}").is_err() || stdout.flush().is_err() {
                            break;
                        }
                    }
                }
            }
            BatteryCommand::Menu => modules::battery::run_menu(ctx).await?,
            BatteryCommand::Profile { name } => modules::battery::set_profile(ctx, &name).await?,
        },
        Command::Audio { action } => match action {
            AudioCommand::Output {
                toggle_eq,
                controls,
            } => match controls.volume {
                Some(delta) => {
                    modules::audio::adjust_volume(AudioTarget::Output, delta, &ctx.config.audio)
                        .await?
                }
                None if controls.toggle_mute => {
                    modules::audio::toggle_mute(AudioTarget::Output).await?
                }
                None if toggle_eq => modules::audio::toggle_eq(ctx).await?,
                None => modules::audio::run_output_menu(ctx).await?,
            },
            AudioCommand::Input {
                toggle_fx,
                controls,
            } => match controls.volume {
                Some(delta) => {
                    modules::audio::adjust_volume(AudioTarget::Input, delta, &ctx.config.audio)
                        .await?
                }
                None if controls.toggle_mute => {
                    modules::audio::toggle_mute(AudioTarget::Input).await?
                }
                None if toggle_fx => modules::audio::toggle_mic_fx(ctx).await?,
                None => modules::audio::run_input_menu(ctx).await?,
            },
        },
        Command::Backlight { action } => match action {
            BacklightCommand::Status { once } => {
                if once {
                    let out = modules::backlight::status_once(ctx).await?;
                    println!("{}", out.to_json_line());
                } else {
                    modules::backlight::spawn_background(std::sync::Arc::clone(ctx));
                    let mut rx = ctx.backlight.subscribe();
                    let mut stdout = std::io::stdout();
                    let initial = modules::backlight::status_once(ctx).await?;
                    let _ = writeln!(stdout, "{}", initial.to_json_line());
                    let _ = stdout.flush();
                    while rx.changed().await.is_ok() {
                        let line = rx.borrow_and_update().to_json_line();
                        if writeln!(stdout, "{line}").is_err() || stdout.flush().is_err() {
                            break;
                        }
                    }
                }
            }
            BacklightCommand::Adjust { delta } => modules::backlight::adjust(ctx, delta).await?,
            BacklightCommand::Refresh => modules::backlight::refresh(ctx).await?,
            BacklightCommand::Internal { device, action } => match action {
                InternalBacklightCommand::Adjust { delta } => {
                    modules::internal_backlight::adjust(ctx, delta, device.as_deref()).await?
                }
                InternalBacklightCommand::Dim { percent } => {
                    modules::internal_backlight::dim(ctx, percent, device.as_deref()).await?
                }
                InternalBacklightCommand::Restore => {
                    modules::internal_backlight::restore(ctx, device.as_deref()).await?
                }
            },
        },
        Command::Power => modules::power::run_menu(ctx).await?,
        Command::Clipboard { clear } => {
            if clear {
                modules::clipboard::clear(ctx).await?;
            } else {
                modules::clipboard::run_menu(ctx).await?;
            }
        }
        Command::Screenshot {
            mode,
            save,
            dir,
            delay,
        } => {
            modules::screenshot::run(
                ctx,
                ScreenshotArgs {
                    mode,
                    save,
                    dir,
                    delay,
                },
            )
            .await?;
        }
        Command::Media { action } => {
            if let Some(output) = modules::media::run_action(ctx, &action).await? {
                print!("{output}");
                let _ = std::io::stdout().flush();
            }
        }
    }
    Ok(())
}
