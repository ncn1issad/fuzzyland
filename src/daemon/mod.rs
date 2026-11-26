pub mod ipc;
pub mod menu;

use crate::config::{runtime_dir, Config};
use crate::core::notify::Notifier;
use crate::core::waybar::WaybarOutput;
use crate::daemon::ipc::{lock_path, socket_path, write_response, IpcRequest, IpcResponse};
use crate::daemon::menu::MenuCoordinator;
use crate::modules;
use crate::modules::screenshot::ScreenshotArgs;
use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{watch, OnceCell};
use tracing::{debug, error, info, warn};
use zbus::Connection;

pub struct AppContext {
    pub config: Arc<Config>,
    pub menu: Arc<MenuCoordinator>,
    pub notifier: Arc<Notifier>,
    session_bus: OnceCell<Connection>,
    system_bus: OnceCell<Connection>,
    pub battery: Arc<modules::battery::BatteryState>,
    pub backlight: Arc<modules::backlight::BacklightState>,
    pub bluetooth: Arc<modules::bluetooth::BluetoothState>,
    pub media: Arc<modules::media::MediaState>,
    pub power: Arc<modules::power::PowerState>,
    pub is_daemon: bool,
}

impl AppContext {
    pub fn new_standalone(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config: Arc::new(config),
            menu: MenuCoordinator::new(true),
            notifier: Arc::new(Notifier::new()),
            session_bus: OnceCell::new(),
            system_bus: OnceCell::new(),
            battery: Arc::new(modules::battery::BatteryState::new()),
            backlight: Arc::new(modules::backlight::BacklightState::new()),
            bluetooth: Arc::new(modules::bluetooth::BluetoothState::new()),
            media: Arc::new(modules::media::MediaState::new()),
            power: Arc::new(modules::power::PowerState::new()),
            is_daemon: false,
        })
    }

    pub async fn new_daemon(config: Config) -> Arc<Self> {
        let session_cell = OnceCell::new();
        let notifier = match Connection::session().await {
            Ok(conn) => {
                let _ = session_cell.set(conn.clone());
                Arc::new(Notifier::with_connection(conn))
            }
            Err(err) => {
                warn!("Could not connect to session D-Bus at startup: {err}");
                Arc::new(Notifier::new())
            }
        };

        let system_cell = OnceCell::new();
        match Connection::system().await {
            Ok(conn) => {
                let _ = system_cell.set(conn);
            }
            Err(err) => {
                warn!("Could not connect to system D-Bus at startup: {err}");
            }
        }

        let power = Arc::new(modules::power::PowerState::new());
        power.warm_load(&config.power);

        Arc::new(Self {
            config: Arc::new(config),
            menu: MenuCoordinator::new(false),
            notifier,
            session_bus: session_cell,
            system_bus: system_cell,
            battery: Arc::new(modules::battery::BatteryState::new()),
            backlight: Arc::new(modules::backlight::BacklightState::new()),
            bluetooth: Arc::new(modules::bluetooth::BluetoothState::new()),
            media: Arc::new(modules::media::MediaState::new()),
            power,
            is_daemon: true,
        })
    }

    pub async fn session_bus(&self) -> Result<&Connection> {
        self.session_bus
            .get_or_try_init(|| async {
                Connection::session()
                    .await
                    .context("Failed to connect to session D-Bus")
            })
            .await
    }

    pub async fn system_bus(&self) -> Result<&Connection> {
        self.system_bus
            .get_or_try_init(|| async {
                Connection::system()
                    .await
                    .context("Failed to connect to system D-Bus")
            })
            .await
    }
}

fn acquire_daemon_lock() -> Result<File> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create runtime dir {}", dir.display()))?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));

    let path = lock_path();
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Failed to open lock file {}", path.display()))?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Err(anyhow!("Another fuzzyland daemon instance is already running"));
        }
        return Err(anyhow!("Failed to lock {}: {err}", path.display()));
    }
    Ok(file)
}

pub async fn run_daemon(config: Config) -> Result<()> {
    let _lock_file = acquire_daemon_lock()?;

    let sock_path = socket_path();
    if sock_path.exists() {
        let _ = std::fs::remove_file(&sock_path);
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("Failed to bind IPC socket {}", sock_path.display()))?;
    let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));

    let ctx = AppContext::new_daemon(config).await;

    // Launch warm background caches & watchers.
    modules::battery::spawn_background(Arc::clone(&ctx));
    modules::backlight::spawn_background(Arc::clone(&ctx));
    modules::bluetooth::spawn_background(Arc::clone(&ctx));
    modules::media::spawn_background(Arc::clone(&ctx));

    info!("fuzzyland daemon listening on {}", sock_path.display());

    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _)) => {
                        let client_ctx = Arc::clone(&ctx);
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, client_ctx).await {
                                debug!("IPC client handler ended: {err}");
                            }
                        });
                    }
                    Err(err) => {
                        error!("Failed to accept IPC client: {err}");
                    }
                }
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM; shutting down fuzzyland daemon");
                break;
            }
            _ = sigint.recv() => {
                info!("Received SIGINT; shutting down fuzzyland daemon");
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}

async fn handle_client(stream: UnixStream, ctx: Arc<AppContext>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(());
    }

    let req: IpcRequest = match serde_json::from_str(line.trim_end()) {
        Ok(r) => r,
        Err(err) => {
            write_response(&mut write_half, &IpcResponse::error(format!("Invalid IPC request: {err}"))).await?;
            return Ok(());
        }
    };

    match req {
        IpcRequest::Ping => {
            write_response(&mut write_half, &IpcResponse::ok_line("pong")).await?;
        }
        IpcRequest::BatteryStatus { once: false } => {
            let rx = ctx.battery.subscribe();
            stream_watch(&mut write_half, rx).await?;
        }
        IpcRequest::BacklightStatus { once: false } => {
            let rx = ctx.backlight.subscribe();
            stream_watch(&mut write_half, rx).await?;
        }
        other => {
            let resp = dispatch_request(&ctx, other).await;
            write_response(&mut write_half, &resp).await?;
        }
    }
    Ok(())
}

async fn stream_watch<W: tokio::io::AsyncWriteExt + Unpin>(
    writer: &mut W,
    mut rx: watch::Receiver<WaybarOutput>,
) -> Result<()> {
    let initial = rx.borrow_and_update().to_json_line();
    if write_response(writer, &IpcResponse::StreamLine { line: initial })
        .await
        .is_err()
    {
        return Ok(());
    }

    while rx.changed().await.is_ok() {
        let next = rx.borrow_and_update().to_json_line();
        if write_response(writer, &IpcResponse::StreamLine { line: next })
            .await
            .is_err()
        {
            break;
        }
    }
    Ok(())
}

async fn dispatch_request(ctx: &Arc<AppContext>, req: IpcRequest) -> IpcResponse {
    match req {
        IpcRequest::Ping => IpcResponse::ok_line("pong"),
        IpcRequest::Network => match modules::network::run_menu(ctx).await {
            Ok(()) => IpcResponse::ok_empty(),
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::Bluetooth { toggle_power } => {
            let res = if toggle_power {
                modules::bluetooth::toggle_power(ctx).await
            } else {
                modules::bluetooth::run_menu(ctx).await
            };
            match res {
                Ok(()) => IpcResponse::ok_empty(),
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::BatteryStatus { once: _ } => match modules::battery::status_once(ctx).await {
            Ok(out) => IpcResponse::ok_line(out.to_json_line()),
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::BatteryMenu => match modules::battery::run_menu(ctx).await {
            Ok(()) => IpcResponse::ok_empty(),
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::BatteryProfile { name } => {
            match modules::battery::set_profile(ctx, &name).await {
                Ok(()) => IpcResponse::ok_empty(),
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::AudioOutput { toggle_eq } => {
            let res = if toggle_eq {
                modules::audio::toggle_eq(ctx).await
            } else {
                modules::audio::run_output_menu(ctx).await
            };
            match res {
                Ok(()) => IpcResponse::ok_empty(),
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::AudioInput { toggle_fx } => {
            let res = if toggle_fx {
                modules::audio::toggle_mic_fx(ctx).await
            } else {
                modules::audio::run_input_menu(ctx).await
            };
            match res {
                Ok(()) => IpcResponse::ok_empty(),
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::BacklightStatus { once: _ } => {
            match modules::backlight::status_once(ctx).await {
                Ok(out) => IpcResponse::ok_line(out.to_json_line()),
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::BacklightAdjust { delta } => {
            match modules::backlight::adjust(ctx, delta).await {
                Ok(()) => IpcResponse::ok_empty(),
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::BacklightRefresh => match modules::backlight::refresh(ctx).await {
            Ok(()) => IpcResponse::ok_empty(),
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::Power => match modules::power::run_menu(ctx).await {
            Ok(()) => IpcResponse::ok_empty(),
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::Clipboard { clear } => {
            let res = if clear {
                modules::clipboard::clear(ctx).await
            } else {
                modules::clipboard::run_menu(ctx).await
            };
            match res {
                Ok(()) => IpcResponse::ok_empty(),
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::Screenshot {
            mode,
            save,
            dir,
            delay,
        } => {
            let args = ScreenshotArgs {
                mode,
                save,
                dir: dir.map(PathBuf::from),
                delay,
            };
            match modules::screenshot::run(ctx, args).await {
                Ok(()) => IpcResponse::ok_empty(),
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::Media { action } => match modules::media::run_action(ctx, &action).await {
            Ok(out) => IpcResponse::ok_raw(out),
            Err(err) => IpcResponse::error(err.to_string()),
        },
    }
}
