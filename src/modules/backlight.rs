use crate::config::{BacklightConfig, Config};
use crate::core::hyprland::HyprEventStream;
use crate::core::waybar::{TooltipFormatter, WaybarOutput};
use crate::daemon::AppContext;
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

const I2C_SLAVE: libc::Ioctl = 0x0703;
const DDC_I2C_ADDR: u8 = 0x37;
const DDC_WRITE_ADDR: u8 = DDC_I2C_ADDR << 1; // 0x6E
const DDC_HOST_SRC_ADDR: u8 = 0x51;
const DDC_HOST_DEST_ADDR: u8 = 0x50;
const DDC_OPCODE_GET_VCP: u8 = 0x01;
const DDC_OPCODE_VCP_REPLY: u8 = 0x02;
const DDC_OPCODE_SET_VCP: u8 = 0x03;
const VCP_LUMINANCE: u8 = 0x10;
const DDC_REPLY_READ_LEN: usize = 12;
const DDC_MAX_RETRIES: usize = 3;

const RECENT_ADJUST_GRACE: Duration = Duration::from_secs(3);
const INCOMPATIBLE_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const HOTPLUG_RETRY_ATTEMPTS: usize = 3;
const HOTPLUG_RETRY_DELAY: Duration = Duration::from_millis(1500);
const HYPR_RECONNECT_INITIAL: Duration = Duration::from_secs(5);
const HYPR_RECONNECT_MAX: Duration = Duration::from_secs(30);
const REFRESH_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

const DRM_SYSFS_ROOT: &str = "/sys/class/drm";

type DrmFingerprint = Vec<(String, Option<u64>)>;

enum BacklightCmd {
    Adjust(i32),
    Refresh { reply: Option<oneshot::Sender<()>> },
    Hotplug,
}

pub struct BacklightState {
    tx: watch::Sender<WaybarOutput>,
    cmd_tx: mpsc::UnboundedSender<BacklightCmd>,
    cmd_rx: Mutex<Option<mpsc::UnboundedReceiver<BacklightCmd>>>,
    background_started: AtomicBool,
}

impl BacklightState {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(WaybarOutput::empty());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        Self {
            tx,
            cmd_tx,
            cmd_rx: Mutex::new(Some(cmd_rx)),
            background_started: AtomicBool::new(false),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<WaybarOutput> {
        self.tx.subscribe()
    }

    fn is_running(&self) -> bool {
        self.background_started.load(Ordering::Acquire)
    }

    fn publish(&self, output: WaybarOutput) {
        self.tx.send_if_modified(|current| {
            if *current != output {
                *current = output;
                true
            } else {
                false
            }
        });
    }
}

impl Default for BacklightState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DrmConnector {
    connector: String,
    edid_hash: Option<u64>,
    edid_info: EdidInfo,
    i2c_bus: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct EdidInfo {
    manufacturer: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorState {
    bus: u32,
    connector: String,
    manufacturer: Option<String>,
    model: String,
    current: u16,
    max_val: u16,
    pct: u8,
    target_pct: u8,
    applied_pct: u8,
    fail_count: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VcpReading {
    current: u16,
    max_val: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DdcReplyError {
    Truncated,
    InvalidHeader,
    NullMessage,
    ChecksumMismatch { expected: u8, actual: u8 },
    UnexpectedOpcode(u8),
    UnsupportedVcp,
    ErrorResult(u8),
    VcpMismatch { expected: u8, actual: u8 },
    ZeroMaxValue,
}

impl std::fmt::Display for DdcReplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "DDC/CI reply truncated"),
            Self::InvalidHeader => write!(f, "DDC/CI reply invalid header"),
            Self::NullMessage => write!(f, "DDC/CI null message (monitor busy)"),
            Self::ChecksumMismatch { expected, actual } => {
                write!(
                    f,
                    "DDC/CI checksum mismatch (expected 0x{expected:02x}, got 0x{actual:02x})"
                )
            }
            Self::UnexpectedOpcode(op) => write!(f, "DDC/CI unexpected opcode 0x{op:02x}"),
            Self::UnsupportedVcp => write!(f, "DDC/CI VCP code unsupported"),
            Self::ErrorResult(rc) => write!(f, "DDC/CI reply error code 0x{rc:02x}"),
            Self::VcpMismatch { expected, actual } => {
                write!(
                    f,
                    "DDC/CI VCP mismatch (expected 0x{expected:02x}, got 0x{actual:02x})"
                )
            }
            Self::ZeroMaxValue => write!(f, "DDC/CI max value is zero"),
        }
    }
}

impl std::error::Error for DdcReplyError {}

struct ApplyOutcome {
    bus: u32,
    target_pct: u8,
    raw_val: u16,
    ok: bool,
}

enum PollOutcome {
    FingerprintChanged {
        fingerprint: DrmFingerprint,
    },
    UnchangedNoMonitors,
    NeedsReprobe,
    Readings {
        fingerprint: DrmFingerprint,
        results: Vec<(u32, Option<VcpReading>)>,
    },
}

enum I2cJobResult {
    Probe {
        fingerprint: DrmFingerprint,
        monitors: Vec<MonitorState>,
        replies: Vec<oneshot::Sender<()>>,
    },
    Apply {
        outcomes: Vec<ApplyOutcome>,
    },
    Poll(PollOutcome),
}

struct ControllerState {
    fingerprint: DrmFingerprint,
    monitors: Vec<MonitorState>,
    pending_apply: bool,
    pending_delta_on_probe: i32,
    last_adjust_at: Option<Instant>,
    incompatible_retry_at: Option<Instant>,
    debounce_deadline: Option<Instant>,
    i2c_busy: bool,
    queued_refresh: Option<Vec<oneshot::Sender<()>>>,
    queued_hotplug: bool,
}

impl ControllerState {
    fn new() -> Self {
        Self {
            fingerprint: Vec::new(),
            monitors: Vec::new(),
            pending_apply: false,
            pending_delta_on_probe: 0,
            last_adjust_at: None,
            incompatible_retry_at: None,
            debounce_deadline: None,
            i2c_busy: false,
            queued_refresh: None,
            queued_hotplug: false,
        }
    }

    fn has_unapplied_targets(&self) -> bool {
        self.monitors.iter().any(|m| m.applied_pct != m.target_pct)
    }

    fn in_recent_adjust_grace(&self) -> bool {
        self.last_adjust_at
            .is_some_and(|t| t.elapsed() < RECENT_ADJUST_GRACE)
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn spawn_background(ctx: Arc<AppContext>) {
    let cmd_rx = {
        let mut guard = match ctx.backlight.cmd_rx.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.take()
    };

    let Some(cmd_rx) = cmd_rx else {
        debug!("Backlight background task already started");
        return;
    };

    ctx.backlight
        .background_started
        .store(true, Ordering::Release);

    let hypr_cmd_tx = ctx.backlight.cmd_tx.clone();
    tokio::spawn(async move {
        run_hyprland_watcher(hypr_cmd_tx).await;
    });

    tokio::spawn(async move {
        run_backlight_loop(ctx, cmd_rx).await;
    });
}

pub async fn status_once(ctx: &AppContext) -> Result<WaybarOutput> {
    if ctx.is_daemon {
        return Ok(ctx.backlight.tx.borrow().clone());
    }

    let cfg = ctx.config.backlight.clone();
    let (_, monitors) =
        tokio::task::spawn_blocking(move || probe_compatible_monitors_sync(&cfg))
            .await
            .context("Backlight probe worker panicked")?;

    Ok(render_waybar_output(&monitors, &ctx.config))
}

pub async fn adjust(ctx: &AppContext, delta: i32) -> Result<()> {
    if delta == 0 {
        return Ok(());
    }

    if ctx.is_daemon || ctx.backlight.is_running() {
        ctx.backlight
            .cmd_tx
            .send(BacklightCmd::Adjust(delta))
            .map_err(|_| anyhow!("Backlight background task is not running"))?;
        return Ok(());
    }

    // Standalone one-shot fallback when daemon is not running.
    let cfg = ctx.config.backlight.clone();
    tokio::task::spawn_blocking(move || {
        let (_, monitors) = probe_compatible_monitors_sync(&cfg);
        if monitors.is_empty() {
            return;
        }
        let settle = Duration::from_millis(cfg.i2c_settle_ms);
        for mon in monitors {
            let new_pct = clamp_pct_delta(mon.pct, delta);
            let raw = pct_to_raw(new_pct, mon.max_val);
            if let Err(err) = ddc_set_brightness_with_retries(mon.bus, raw, settle) {
                warn!(
                    "Standalone DDC setvcp failed on i2c-{} ({}): {err}",
                    mon.bus, mon.model
                );
            }
        }
    })
    .await
    .context("Backlight adjust worker panicked")?;

    Ok(())
}

pub async fn refresh(ctx: &AppContext) -> Result<()> {
    if ctx.is_daemon || ctx.backlight.is_running() {
        let (reply_tx, reply_rx) = oneshot::channel();
        ctx.backlight
            .cmd_tx
            .send(BacklightCmd::Refresh {
                reply: Some(reply_tx),
            })
            .map_err(|_| anyhow!("Backlight background task is not running"))?;
        let _ = tokio::time::timeout(REFRESH_WAIT_TIMEOUT, reply_rx).await;
        return Ok(());
    }

    // Standalone refresh probe.
    let cfg = ctx.config.backlight.clone();
    let (_, monitors) = probe_with_hotplug_retry(cfg, true).await;
    let output = render_waybar_output(&monitors, &ctx.config);
    ctx.backlight.publish(output);
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon Background Loop & Hyprland Hotplug Watcher
// ---------------------------------------------------------------------------

async fn run_hyprland_watcher(cmd_tx: mpsc::UnboundedSender<BacklightCmd>) {
    let mut backoff = HYPR_RECONNECT_INITIAL;

    loop {
        match HyprEventStream::connect().await {
            Ok(mut stream) => {
                debug!("Backlight module subscribed to Hyprland socket2");
                backoff = HYPR_RECONNECT_INITIAL;

                loop {
                    match stream.next_event().await {
                        Ok(Some(ev)) => {
                            if ev.event == "monitoradded" || ev.event == "monitorremoved" {
                                debug!("Hyprland event {}>>{}; scheduling backlight probe", ev.event, ev.data);
                                if cmd_tx.send(BacklightCmd::Hotplug).is_err() {
                                    return;
                                }
                            }
                        }
                        Ok(None) => {
                            debug!("Hyprland socket2 closed; will reconnect");
                            break;
                        }
                        Err(err) => {
                            debug!("Hyprland socket2 read error: {err}; will reconnect");
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                debug!("Hyprland socket2 unavailable for backlight hotplug: {err}");
            }
        }

        if cmd_tx.is_closed() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(HYPR_RECONNECT_MAX);
    }
}

async fn run_backlight_loop(ctx: Arc<AppContext>, mut cmd_rx: mpsc::UnboundedReceiver<BacklightCmd>) {
    let (job_tx, mut job_rx) = mpsc::unbounded_channel::<I2cJobResult>();
    let mut state = ControllerState::new();

    // Probe immediately on startup.
    launch_probe_job(&mut state, &ctx.config.backlight, &job_tx, false, Vec::new());

    let poll_secs = ctx.config.backlight.poll_interval_secs.max(1);
    let mut poll_interval = tokio::time::interval(Duration::from_secs(poll_secs));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume immediate first tick since we already launched startup probe.
    poll_interval.tick().await;

    loop {
        let debounce_fut = async {
            if let Some(deadline) = state.debounce_deadline {
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            } else {
                std::future::pending::<()>().await;
            }
        };

        tokio::select! {
            maybe_cmd = cmd_rx.recv() => {
                let Some(cmd) = maybe_cmd else {
                    break;
                };
                handle_command(cmd, &mut state, &ctx, &job_tx);
            }
            maybe_res = job_rx.recv() => {
                let Some(res) = maybe_res else {
                    break;
                };
                handle_job_result(res, &mut state, &ctx, &job_tx);
            }
            _ = debounce_fut => {
                state.debounce_deadline = None;
                if !state.i2c_busy {
                    maybe_launch_apply_job(&mut state, &ctx.config.backlight, &job_tx);
                }
            }
            _ = poll_interval.tick() => {
                handle_periodic_poll(&mut state, &ctx.config.backlight, &job_tx);
            }
        }
    }
}

fn handle_command(
    cmd: BacklightCmd,
    state: &mut ControllerState,
    ctx: &AppContext,
    job_tx: &mpsc::UnboundedSender<I2cJobResult>,
) {
    match cmd {
        BacklightCmd::Adjust(delta) => {
            if state.monitors.is_empty() {
                state.pending_delta_on_probe = state.pending_delta_on_probe.saturating_add(delta);
                if !state.i2c_busy {
                    launch_probe_job(&mut *state, &ctx.config.backlight, job_tx, false, Vec::new());
                }
                return;
            }

            let mut changed = false;
            for mon in &mut state.monitors {
                let new_pct = clamp_pct_delta(mon.pct, delta);
                if new_pct != mon.pct || mon.target_pct != new_pct {
                    changed = true;
                }
                mon.pct = new_pct;
                mon.target_pct = new_pct;
            }

            state.pending_apply = true;
            state.last_adjust_at = Some(Instant::now());
            state.debounce_deadline =
                Some(Instant::now() + Duration::from_millis(ctx.config.backlight.debounce_ms));

            if changed {
                ctx.backlight
                    .publish(render_waybar_output(&state.monitors, &ctx.config));
            }
        }
        BacklightCmd::Refresh { reply } => {
            if state.i2c_busy {
                let list = state.queued_refresh.get_or_insert_with(Vec::new);
                if let Some(tx) = reply {
                    list.push(tx);
                }
            } else {
                let replies = reply.into_iter().collect();
                launch_probe_job(state, &ctx.config.backlight, job_tx, true, replies);
            }
        }
        BacklightCmd::Hotplug => {
            if state.i2c_busy {
                state.queued_hotplug = true;
            } else {
                launch_probe_job(state, &ctx.config.backlight, job_tx, true, Vec::new());
            }
        }
    }
}

fn handle_job_result(
    result: I2cJobResult,
    state: &mut ControllerState,
    ctx: &AppContext,
    job_tx: &mpsc::UnboundedSender<I2cJobResult>,
) {
    state.i2c_busy = false;

    match result {
        I2cJobResult::Probe {
            fingerprint,
            mut monitors,
            replies,
        } => {
            let pending_delta = std::mem::take(&mut state.pending_delta_on_probe);
            if pending_delta != 0 && !monitors.is_empty() {
                for mon in &mut monitors {
                    let new_pct = clamp_pct_delta(mon.pct, pending_delta);
                    mon.pct = new_pct;
                    mon.target_pct = new_pct;
                }
                state.pending_apply = true;
                state.last_adjust_at = Some(Instant::now());
                state.debounce_deadline = Some(
                    Instant::now() + Duration::from_millis(ctx.config.backlight.debounce_ms),
                );
            } else {
                state.pending_apply = false;
                state.debounce_deadline = None;
            }

            if !fingerprint.is_empty() && monitors.is_empty() {
                state.incompatible_retry_at = Some(Instant::now() + INCOMPATIBLE_RETRY_INTERVAL);
            } else {
                state.incompatible_retry_at = None;
            }

            info!(
                "Backlight probe complete: {} external DDC/CI monitor(s) found",
                monitors.len()
            );
            state.fingerprint = fingerprint;
            state.monitors = monitors;
            ctx.backlight
                .publish(render_waybar_output(&state.monitors, &ctx.config));

            for tx in replies {
                let _ = tx.send(());
            }
        }
        I2cJobResult::Apply { outcomes } => {
            for outcome in outcomes {
                if let Some(mon) = state.monitors.iter_mut().find(|m| m.bus == outcome.bus) {
                    mon.applied_pct = outcome.target_pct;
                    if outcome.ok {
                        mon.current = outcome.raw_val;
                        mon.fail_count = 0;
                    } else {
                        mon.fail_count = mon.fail_count.saturating_add(1);
                    }
                }
            }
            state.last_adjust_at = Some(Instant::now());

            if !state.has_unapplied_targets() {
                state.pending_apply = false;
            }
        }
        I2cJobResult::Poll(outcome) => match outcome {
            PollOutcome::FingerprintChanged { fingerprint } => {
                if fingerprint.is_empty() {
                    state.fingerprint.clear();
                    state.monitors.clear();
                    state.pending_apply = false;
                    state.debounce_deadline = None;
                    state.incompatible_retry_at = None;
                    ctx.backlight.publish(WaybarOutput::empty());
                } else {
                    launch_probe_job(state, &ctx.config.backlight, job_tx, true, Vec::new());
                    return;
                }
            }
            PollOutcome::UnchangedNoMonitors => {}
            PollOutcome::NeedsReprobe => {
                launch_probe_job(state, &ctx.config.backlight, job_tx, false, Vec::new());
                return;
            }
            PollOutcome::Readings {
                fingerprint,
                results,
            } => {
                // Never clobber an adjustment that started while getvcp was in flight.
                if state.pending_apply
                    || state.in_recent_adjust_grace()
                    || state.fingerprint != fingerprint
                {
                    debug!("Discarding idle OSD poll result due to concurrent adjustment/change");
                } else {
                    let mut need_redetect = false;
                    for (bus, reading) in results {
                        if let Some(mon) = state.monitors.iter_mut().find(|m| m.bus == bus) {
                            if let Some(r) = reading {
                                let pct = raw_to_pct(r.current, r.max_val);
                                mon.current = r.current;
                                mon.max_val = r.max_val;
                                mon.pct = pct;
                                mon.target_pct = pct;
                                mon.applied_pct = pct;
                                mon.fail_count = 0;
                            } else {
                                mon.fail_count = mon.fail_count.saturating_add(1);
                                if mon.fail_count >= 2 {
                                    need_redetect = true;
                                }
                            }
                        }
                    }

                    if need_redetect {
                        debug!("Monitor failed consecutive DDC polls; triggering re-probe");
                        launch_probe_job(state, &ctx.config.backlight, job_tx, false, Vec::new());
                        return;
                    }

                    ctx.backlight
                        .publish(render_waybar_output(&state.monitors, &ctx.config));
                }
            }
        },
    }

    // Drain any follow-up work queued while the I2C bus was busy.
    if let Some(replies) = state.queued_refresh.take() {
        state.queued_hotplug = false;
        launch_probe_job(state, &ctx.config.backlight, job_tx, true, replies);
    } else if state.queued_hotplug {
        state.queued_hotplug = false;
        launch_probe_job(state, &ctx.config.backlight, job_tx, true, Vec::new());
    } else if state.has_unapplied_targets()
        && state
            .debounce_deadline
            .is_none_or(|deadline| Instant::now() >= deadline)
    {
        state.debounce_deadline = None;
        maybe_launch_apply_job(state, &ctx.config.backlight, job_tx);
    }
}

fn handle_periodic_poll(
    state: &mut ControllerState,
    cfg: &BacklightConfig,
    job_tx: &mpsc::UnboundedSender<I2cJobResult>,
) {
    if state.i2c_busy || state.pending_apply || state.in_recent_adjust_grace() {
        return;
    }

    state.i2c_busy = true;
    let cfg = cfg.clone();
    let expected_fp = state.fingerprint.clone();
    let buses: Vec<u32> = state.monitors.iter().map(|m| m.bus).collect();
    let can_retry_incompatible = state
        .incompatible_retry_at
        .is_none_or(|deadline| Instant::now() >= deadline);
    let tx = job_tx.clone();

    tokio::spawn(async move {
        let outcome = tokio::task::spawn_blocking(move || {
            let current_fp = get_external_drm_fingerprint(&cfg.internal_prefixes);
            if current_fp != expected_fp {
                return PollOutcome::FingerprintChanged {
                    fingerprint: current_fp,
                };
            }

            if current_fp.is_empty() {
                return PollOutcome::UnchangedNoMonitors;
            }

            if buses.is_empty() {
                return if can_retry_incompatible {
                    PollOutcome::NeedsReprobe
                } else {
                    PollOutcome::UnchangedNoMonitors
                };
            }

            let settle = Duration::from_millis(cfg.i2c_settle_ms);
            let mut results = Vec::with_capacity(buses.len());
            for bus in buses {
                let reading = ddc_get_brightness_with_retries(bus, settle).ok();
                results.push((bus, reading));
            }
            PollOutcome::Readings {
                fingerprint: current_fp,
                results,
            }
        })
        .await
        .unwrap_or(PollOutcome::UnchangedNoMonitors);

        let _ = tx.send(I2cJobResult::Poll(outcome));
    });
}

fn launch_probe_job(
    state: &mut ControllerState,
    cfg: &BacklightConfig,
    job_tx: &mpsc::UnboundedSender<I2cJobResult>,
    with_hotplug_retry: bool,
    replies: Vec<oneshot::Sender<()>>,
) {
    state.i2c_busy = true;
    let cfg = cfg.clone();
    let tx = job_tx.clone();

    tokio::spawn(async move {
        let (fingerprint, monitors) = probe_with_hotplug_retry(cfg, with_hotplug_retry).await;
        let _ = tx.send(I2cJobResult::Probe {
            fingerprint,
            monitors,
            replies,
        });
    });
}

fn maybe_launch_apply_job(
    state: &mut ControllerState,
    cfg: &BacklightConfig,
    job_tx: &mpsc::UnboundedSender<I2cJobResult>,
) {
    let tasks: Vec<(u32, u8, u16)> = state
        .monitors
        .iter()
        .filter(|m| m.applied_pct != m.target_pct)
        .map(|m| (m.bus, m.target_pct, m.max_val.max(1)))
        .collect();

    if tasks.is_empty() {
        state.pending_apply = false;
        return;
    }

    state.i2c_busy = true;
    let settle = Duration::from_millis(cfg.i2c_settle_ms);
    let tx = job_tx.clone();

    tokio::spawn(async move {
        let outcomes = tokio::task::spawn_blocking(move || {
            let mut results = Vec::with_capacity(tasks.len());
            for (bus, target_pct, max_val) in tasks {
                let raw_val = pct_to_raw(target_pct, max_val);
                let ok = match ddc_set_brightness_with_retries(bus, raw_val, settle) {
                    Ok(()) => true,
                    Err(err) => {
                        debug!("DDC setvcp 0x10 failed on i2c-{bus}: {err}");
                        false
                    }
                };
                results.push(ApplyOutcome {
                    bus,
                    target_pct,
                    raw_val,
                    ok,
                });
            }
            results
        })
        .await
        .unwrap_or_default();

        let _ = tx.send(I2cJobResult::Apply { outcomes });
    });
}

async fn probe_with_hotplug_retry(
    cfg: BacklightConfig,
    with_retry: bool,
) -> (DrmFingerprint, Vec<MonitorState>) {
    let max_attempts = if with_retry { HOTPLUG_RETRY_ATTEMPTS } else { 1 };
    let mut last_fp = Vec::new();
    let mut found = Vec::new();

    for attempt in 0..max_attempts {
        if attempt > 0 {
            tokio::time::sleep(HOTPLUG_RETRY_DELAY).await;
        }

        let cfg_clone = cfg.clone();
        let (fp, monitors) = tokio::task::spawn_blocking(move || {
            probe_compatible_monitors_sync(&cfg_clone)
        })
        .await
        .unwrap_or_default();

        last_fp = fp;
        found = monitors;

        if last_fp.is_empty() || !found.is_empty() {
            break;
        }
    }

    (last_fp, found)
}

// ---------------------------------------------------------------------------
// DRM Sysfs Discovery & EDID Parsing
// ---------------------------------------------------------------------------

fn split_drm_connector(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("card")?;
    let digit_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digit_len == 0 {
        return None;
    }
    let after_digits = &rest[digit_len..];
    let short = after_digits.strip_prefix('-')?;
    if short.is_empty() {
        None
    } else {
        Some(short)
    }
}

fn short_connector(drm_connector: &str) -> &str {
    split_drm_connector(drm_connector).unwrap_or(drm_connector)
}

fn is_internal_connector(drm_connector: &str, internal_prefixes: &[String]) -> bool {
    let short = short_connector(drm_connector);
    internal_prefixes
        .iter()
        .any(|prefix| !prefix.is_empty() && short.starts_with(prefix.as_str()))
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn parse_edid_manufacturer(edid: &[u8]) -> Option<String> {
    if edid.len() < 10 {
        return None;
    }
    let raw = u16::from_be_bytes([edid[8], edid[9]]);
    let c1 = ((raw >> 10) & 0x1F) as u8;
    let c2 = ((raw >> 5) & 0x1F) as u8;
    let c3 = (raw & 0x1F) as u8;

    if (1..=26).contains(&c1) && (1..=26).contains(&c2) && (1..=26).contains(&c3) {
        let mut s = String::with_capacity(3);
        s.push((b'A' + c1 - 1) as char);
        s.push((b'A' + c2 - 1) as char);
        s.push((b'A' + c3 - 1) as char);
        Some(s)
    } else {
        None
    }
}

fn parse_edid_model(edid: &[u8]) -> Option<String> {
    // Standard E-EDID base block has 4 18-byte descriptor blocks at offsets 54, 72, 90, 108.
    if edid.len() < 126 {
        return None;
    }
    for offset in [54usize, 72, 90, 108] {
        let block = &edid[offset..offset + 18];
        if block[0] == 0x00 && block[1] == 0x00 && block[2] == 0x00 && block[3] == 0xFC {
            let text_bytes = &block[5..18];
            let end = text_bytes
                .iter()
                .position(|&b| b == 0x0A || b == 0x00)
                .unwrap_or(text_bytes.len());
            let raw = String::from_utf8_lossy(&text_bytes[..end]);
            let cleaned: String = raw
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            let trimmed = cleaned.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn parse_edid_info(edid: &[u8]) -> EdidInfo {
    const EDID_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if edid.len() < 128 || edid[..8] != EDID_HEADER {
        return EdidInfo::default();
    }
    EdidInfo {
        manufacturer: parse_edid_manufacturer(edid),
        model: parse_edid_model(edid),
    }
}

fn parse_i2c_bus_name(name: &str) -> Option<u32> {
    name.strip_prefix("i2c-")?.parse::<u32>().ok()
}

fn find_i2c_bus_in_dir(dir: &Path) -> Option<u32> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut buses: Vec<u32> = Vec::new();
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if let Some(bus) = parse_i2c_bus_name(name) {
                buses.push(bus);
            }
        }
    }
    buses.sort_unstable();
    buses.into_iter().next()
}

fn resolve_connector_i2c_bus(connector_dir: &Path) -> Option<u32> {
    // 1. Check `<connector>/ddc` symlink (resolves to `.../i2c-<N>`).
    let ddc_path = connector_dir.join("ddc");
    if let Ok(resolved) = std::fs::canonicalize(&ddc_path) {
        if let Some(bus) = resolved
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(parse_i2c_bus_name)
        {
            return Some(bus);
        }
        if let Some(bus) = find_i2c_bus_in_dir(&resolved.join("i2c-dev")) {
            return Some(bus);
        }
    } else if let Ok(link_target) = std::fs::read_link(&ddc_path) {
        if let Some(bus) = link_target
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(parse_i2c_bus_name)
        {
            return Some(bus);
        }
    }

    // 2. Check `<connector>/i2c-<N>` directory directly.
    if let Some(bus) = find_i2c_bus_in_dir(connector_dir) {
        return Some(bus);
    }

    // 3. Check `<connector>/drm_dp_aux*/i2c-<N>` (DisplayPort AUX channels).
    if let Ok(entries) = std::fs::read_dir(connector_dir) {
        let mut aux_dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("drm_dp_aux"))
            })
            .map(|e| e.path())
            .collect();
        aux_dirs.sort();
        for aux_dir in aux_dirs {
            if let Some(bus) = find_i2c_bus_in_dir(&aux_dir) {
                return Some(bus);
            }
        }
    }

    None
}

fn discover_external_drm_connectors_at(
    drm_root: &Path,
    internal_prefixes: &[String],
) -> Vec<DrmConnector> {
    let Ok(entries) = std::fs::read_dir(drm_root) else {
        return Vec::new();
    };

    let mut connectors = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if split_drm_connector(&name).is_none() {
            continue;
        }
        if is_internal_connector(&name, internal_prefixes) {
            continue;
        }

        let path = entry.path();
        let status = match std::fs::read_to_string(path.join("status")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if status.trim() != "connected" {
            continue;
        }

        let edid_bytes = std::fs::read(path.join("edid")).unwrap_or_default();
        let (edid_hash, edid_info) = if edid_bytes.is_empty() {
            (None, EdidInfo::default())
        } else {
            (Some(fnv1a_64(&edid_bytes)), parse_edid_info(&edid_bytes))
        };

        let i2c_bus = resolve_connector_i2c_bus(&path);
        connectors.push(DrmConnector {
            connector: name,
            edid_hash,
            edid_info,
            i2c_bus,
        });
    }

    connectors.sort_by(|a, b| a.connector.cmp(&b.connector));
    connectors
}

fn fingerprint_from_connectors(connectors: &[DrmConnector]) -> DrmFingerprint {
    connectors
        .iter()
        .map(|c| (c.connector.clone(), c.edid_hash))
        .collect()
}

fn get_external_drm_fingerprint(internal_prefixes: &[String]) -> DrmFingerprint {
    let connectors = discover_external_drm_connectors_at(Path::new(DRM_SYSFS_ROOT), internal_prefixes);
    fingerprint_from_connectors(&connectors)
}

fn probe_compatible_monitors_sync(cfg: &BacklightConfig) -> (DrmFingerprint, Vec<MonitorState>) {
    let connectors =
        discover_external_drm_connectors_at(Path::new(DRM_SYSFS_ROOT), &cfg.internal_prefixes);
    let fingerprint = fingerprint_from_connectors(&connectors);
    if connectors.is_empty() {
        return (fingerprint, Vec::new());
    }

    let settle = Duration::from_millis(cfg.i2c_settle_ms);
    let mut monitors = Vec::new();
    let mut seen_buses = HashSet::new();

    for conn in connectors {
        let Some(bus) = conn.i2c_bus else {
            debug!(
                "Connector {} has no associated DDC I2C bus; skipping",
                conn.connector
            );
            continue;
        };
        if !seen_buses.insert(bus) {
            continue;
        }

        match ddc_get_brightness_with_retries(bus, settle) {
            Ok(reading) => {
                let pct = raw_to_pct(reading.current, reading.max_val);
                let model = conn
                    .edid_info
                    .model
                    .clone()
                    .unwrap_or_else(|| short_connector(&conn.connector).to_string());
                monitors.push(MonitorState {
                    bus,
                    connector: conn.connector,
                    manufacturer: conn.edid_info.manufacturer,
                    model,
                    current: reading.current,
                    max_val: reading.max_val,
                    pct,
                    target_pct: pct,
                    applied_pct: pct,
                    fail_count: 0,
                });
            }
            Err(err) => {
                debug!(
                    "Connector {} (i2c-{bus}) does not respond to DDC/CI VCP 0x10: {err}",
                    conn.connector
                );
            }
        }
    }

    (fingerprint, monitors)
}

// ---------------------------------------------------------------------------
// Native Linux /dev/i2c-* DDC/CI Protocol (VCP 0x10 Luminance)
// ---------------------------------------------------------------------------

fn ddc_checksum(initial: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(initial, |acc, &b| acc ^ b)
}

fn build_get_vcp_packet(vcp: u8) -> [u8; 5] {
    let body = [DDC_HOST_SRC_ADDR, 0x82, DDC_OPCODE_GET_VCP, vcp];
    let chk = ddc_checksum(DDC_WRITE_ADDR, &body);
    [body[0], body[1], body[2], body[3], chk]
}

fn build_set_vcp_packet(vcp: u8, value: u16) -> [u8; 7] {
    let [hi, lo] = value.to_be_bytes();
    let body = [DDC_HOST_SRC_ADDR, 0x84, DDC_OPCODE_SET_VCP, vcp, hi, lo];
    let chk = ddc_checksum(DDC_WRITE_ADDR, &body);
    [body[0], body[1], body[2], body[3], body[4], body[5], chk]
}

fn parse_vcp_reply(buf: &[u8], expected_vcp: u8) -> std::result::Result<VcpReading, DdcReplyError> {
    if buf.len() < 3 {
        return Err(DdcReplyError::Truncated);
    }
    if buf[0] != DDC_WRITE_ADDR || (buf[1] & 0x80) == 0 {
        return Err(DdcReplyError::InvalidHeader);
    }

    let payload_len = (buf[1] & 0x7F) as usize;
    if payload_len == 0 {
        return Err(DdcReplyError::NullMessage);
    }

    let chk_idx = 2 + payload_len;
    if chk_idx >= buf.len() {
        return Err(DdcReplyError::Truncated);
    }

    let expected_chk = ddc_checksum(DDC_HOST_DEST_ADDR, &buf[..chk_idx]);
    if buf[chk_idx] != expected_chk {
        return Err(DdcReplyError::ChecksumMismatch {
            expected: expected_chk,
            actual: buf[chk_idx],
        });
    }

    if payload_len < 8 {
        return Err(DdcReplyError::Truncated);
    }
    if buf[2] != DDC_OPCODE_VCP_REPLY {
        return Err(DdcReplyError::UnexpectedOpcode(buf[2]));
    }
    match buf[3] {
        0x00 => {}
        0x01 => return Err(DdcReplyError::UnsupportedVcp),
        other => return Err(DdcReplyError::ErrorResult(other)),
    }
    if buf[4] != expected_vcp {
        return Err(DdcReplyError::VcpMismatch {
            expected: expected_vcp,
            actual: buf[4],
        });
    }

    let max_val = u16::from_be_bytes([buf[6], buf[7]]);
    let current = u16::from_be_bytes([buf[8], buf[9]]);
    if max_val == 0 {
        return Err(DdcReplyError::ZeroMaxValue);
    }

    Ok(VcpReading { current, max_val })
}

fn open_i2c_ddc(bus: u32) -> Result<File> {
    let path = format!("/dev/i2c-{bus}");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("Failed to open {path}"))?;

    let rc = unsafe { libc::ioctl(file.as_raw_fd(), I2C_SLAVE, libc::c_ulong::from(DDC_I2C_ADDR)) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        return Err(anyhow!(
            "Failed to set I2C_SLAVE 0x{DDC_I2C_ADDR:02x} on {path}: {err}"
        ));
    }

    Ok(file)
}

fn ddc_get_brightness_once(file: &mut File, settle: Duration) -> Result<VcpReading> {
    let req = build_get_vcp_packet(VCP_LUMINANCE);
    let written = file.write(&req).context("I2C write failed")?;
    if written != req.len() {
        return Err(anyhow!(
            "Short I2C write for getvcp: wrote {written}/{} bytes",
            req.len()
        ));
    }

    if !settle.is_zero() {
        std::thread::sleep(settle);
    }

    let mut reply = [0u8; DDC_REPLY_READ_LEN];
    let n = file.read(&mut reply).context("I2C read failed")?;
    parse_vcp_reply(&reply[..n], VCP_LUMINANCE).map_err(Into::into)
}

fn ddc_get_brightness_with_retries(bus: u32, settle: Duration) -> Result<VcpReading> {
    let mut file = open_i2c_ddc(bus)?;
    let mut last_err = None;

    for attempt in 0..DDC_MAX_RETRIES {
        if attempt > 0 && !settle.is_zero() {
            std::thread::sleep(settle);
        }
        match ddc_get_brightness_once(&mut file, settle) {
            Ok(reading) => return Ok(reading),
            Err(err) => {
                // Do not waste retries when the monitor explicitly reports VCP 0x10 unsupported.
                if err
                    .downcast_ref::<DdcReplyError>()
                    .is_some_and(|e| matches!(e, DdcReplyError::UnsupportedVcp))
                {
                    return Err(err);
                }
                last_err = Some(err);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("DDC/CI getvcp failed on i2c-{bus}")))
}

fn ddc_set_brightness_with_retries(bus: u32, raw_value: u16, settle: Duration) -> Result<()> {
    let mut file = open_i2c_ddc(bus)?;
    let packet = build_set_vcp_packet(VCP_LUMINANCE, raw_value);
    let mut last_err = None;

    for attempt in 0..DDC_MAX_RETRIES {
        if attempt > 0 && !settle.is_zero() {
            std::thread::sleep(settle);
        }
        match file.write(&packet) {
            Ok(n) if n == packet.len() => {
                if !settle.is_zero() {
                    std::thread::sleep(settle);
                }
                return Ok(());
            }
            Ok(n) => {
                last_err = Some(anyhow!(
                    "Short I2C write for setvcp on i2c-{bus}: wrote {n}/{} bytes",
                    packet.len()
                ));
            }
            Err(err) => {
                last_err = Some(anyhow!("I2C write failed on i2c-{bus}: {err}"));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("DDC/CI setvcp failed on i2c-{bus}")))
}

// ---------------------------------------------------------------------------
// Scaling & Waybar Formatting
// ---------------------------------------------------------------------------

fn raw_to_pct(current: u16, max_val: u16) -> u8 {
    if max_val == 0 {
        return 0;
    }
    let max_u32 = u32::from(max_val);
    let pct = (u32::from(current) * 100 + max_u32 / 2) / max_u32;
    pct.min(100) as u8
}

fn pct_to_raw(pct: u8, max_val: u16) -> u16 {
    let clamped = u32::from(pct.min(100));
    let max_u32 = u32::from(max_val);
    ((clamped * max_u32 + 50) / 100).min(max_u32) as u16
}

fn clamp_pct_delta(cur_pct: u8, delta: i32) -> u8 {
    (i32::from(cur_pct).saturating_add(delta)).clamp(0, 100) as u8
}

fn render_waybar_output(monitors: &[MonitorState], config: &Config) -> WaybarOutput {
    let Some(primary) = monitors.first() else {
        return WaybarOutput::empty();
    };

    let pct = primary.pct;
    let text = format!("{} {}%", config.icons.monitor, pct);
    let fmt = TooltipFormatter::new(&config.tooltip);
    let mut lines = Vec::new();

    if monitors.len() == 1 {
        let conn = short_connector(&primary.connector);
        lines.push(fmt.header(&format!("{} · {}%", primary.model, pct)));
        let meta = match (conn.is_empty(), primary.manufacturer.as_deref()) {
            (false, Some(mfg)) if !primary.model.to_ascii_uppercase().contains(mfg) => {
                format!("{conn} · {mfg} · DDC/CI")
            }
            (false, _) => format!("{conn} · DDC/CI"),
            (true, Some(mfg)) if !primary.model.to_ascii_uppercase().contains(mfg) => {
                format!("{mfg} · DDC/CI")
            }
            (true, _) => "DDC/CI".to_string(),
        };
        lines.push(fmt.subtext(&meta));
    } else {
        for (idx, mon) in monitors.iter().enumerate() {
            let conn = short_connector(&mon.connector);
            let label = if !conn.is_empty() && mon.model != conn {
                format!("{} ({conn})", mon.model)
            } else {
                mon.model.clone()
            };
            let entry = format!("{label} · {}%", mon.pct);
            if idx == 0 {
                lines.push(fmt.header(&entry));
            } else {
                lines.push(fmt.line(&entry));
            }
        }
    }

    lines.push(fmt.footer("Scroll: Adjust brightness · Click: Refresh"));

    WaybarOutput {
        text,
        tooltip: Some(lines.join("\n")),
        class: Vec::new(),
        percentage: Some(pct),
    }
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_edid(mfg_raw: u16, monitor_name: Option<&str>) -> [u8; 128] {
        let mut edid = [0u8; 128];
        edid[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        edid[8..10].copy_from_slice(&mfg_raw.to_be_bytes());
        if let Some(name) = monitor_name {
            // Put monitor name descriptor in the 2nd 18-byte descriptor block (offset 72).
            let off = 72;
            edid[off..off + 5].copy_from_slice(&[0x00, 0x00, 0x00, 0xFC, 0x00]);
            let bytes = name.as_bytes();
            let copy_len = bytes.len().min(13);
            edid[off + 5..off + 5 + copy_len].copy_from_slice(&bytes[..copy_len]);
            if copy_len < 13 {
                edid[off + 5 + copy_len] = 0x0A;
                for slot in &mut edid[off + 5 + copy_len + 1..off + 18] {
                    *slot = 0x20;
                }
            }
        }
        edid
    }

    fn encode_pnp(c1: char, c2: char, c3: char) -> u16 {
        let v1 = u16::from((c1 as u8) - b'A' + 1);
        let v2 = u16::from((c2 as u8) - b'A' + 1);
        let v3 = u16::from((c3 as u8) - b'A' + 1);
        (v1 << 10) | (v2 << 5) | v3
    }

    #[test]
    fn ddc_get_vcp_packet_and_checksum() {
        let pkt = build_get_vcp_packet(VCP_LUMINANCE);
        assert_eq!(pkt, [0x51, 0x82, 0x01, 0x10, 0xAC]);
        assert_eq!(ddc_checksum(DDC_WRITE_ADDR, &pkt[..4]), pkt[4]);
    }

    #[test]
    fn ddc_set_vcp_packet_and_checksum() {
        let pkt = build_set_vcp_packet(VCP_LUMINANCE, 50);
        assert_eq!(pkt, [0x51, 0x84, 0x03, 0x10, 0x00, 0x32, 0x9A]);
        assert_eq!(ddc_checksum(DDC_WRITE_ADDR, &pkt[..6]), pkt[6]);

        let pkt_hi = build_set_vcp_packet(VCP_LUMINANCE, 0x01F4);
        assert_eq!(pkt_hi[4], 0x01);
        assert_eq!(pkt_hi[5], 0xF4);
        assert_eq!(ddc_checksum(DDC_WRITE_ADDR, &pkt_hi[..6]), pkt_hi[6]);
    }

    #[test]
    fn ddc_parse_valid_vcp_reply() {
        let mut reply = [
            0x6E, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x4B, 0x00, 0xFF,
        ];
        reply[10] = ddc_checksum(DDC_HOST_DEST_ADDR, &reply[..10]);
        assert_eq!(reply[10], 0x8B);

        let parsed = parse_vcp_reply(&reply, VCP_LUMINANCE).expect("valid reply");
        assert_eq!(
            parsed,
            VcpReading {
                current: 75,
                max_val: 100
            }
        );
    }

    #[test]
    fn ddc_parse_rejects_bad_checksum_and_errors() {
        let mut reply = [
            0x6E, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x4B, 0x00, 0x00,
        ];
        reply[10] = ddc_checksum(DDC_HOST_DEST_ADDR, &reply[..10]) ^ 0x01;
        assert!(matches!(
            parse_vcp_reply(&reply, VCP_LUMINANCE),
            Err(DdcReplyError::ChecksumMismatch { .. })
        ));

        // Null message (monitor busy)
        let null_msg = [0x6E, 0x80, 0xBE];
        assert_eq!(
            parse_vcp_reply(&null_msg, VCP_LUMINANCE),
            Err(DdcReplyError::NullMessage)
        );

        // Unsupported VCP (result_code = 0x01)
        let mut unsup = [
            0x6E, 0x88, 0x02, 0x01, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        unsup[10] = ddc_checksum(DDC_HOST_DEST_ADDR, &unsup[..10]);
        assert_eq!(
            parse_vcp_reply(&unsup, VCP_LUMINANCE),
            Err(DdcReplyError::UnsupportedVcp)
        );

        // Invalid header (all zeros / bus noise)
        assert_eq!(
            parse_vcp_reply(&[0u8; 12], VCP_LUMINANCE),
            Err(DdcReplyError::InvalidHeader)
        );
    }

    #[test]
    fn edid_parses_manufacturer_and_model() {
        let edid = make_test_edid(encode_pnp('D', 'E', 'L'), Some("DELL U2723QE"));
        let info = parse_edid_info(&edid);
        assert_eq!(info.manufacturer.as_deref(), Some("DEL"));
        assert_eq!(info.model.as_deref(), Some("DELL U2723QE"));

        // No 0xFC monitor name descriptor
        let no_name = make_test_edid(encode_pnp('G', 'S', 'M'), None);
        let info2 = parse_edid_info(&no_name);
        assert_eq!(info2.manufacturer.as_deref(), Some("GSM"));
        assert_eq!(info2.model, None);

        // Invalid header returns empty info
        assert_eq!(parse_edid_info(&[0u8; 128]), EdidInfo::default());
    }

    #[test]
    fn connector_prefix_filtering() {
        let defaults = BacklightConfig::default().internal_prefixes;
        assert_eq!(split_drm_connector("card1-DP-2"), Some("DP-2"));
        assert_eq!(split_drm_connector("card0-HDMI-A-1"), Some("HDMI-A-1"));
        assert_eq!(split_drm_connector("card0"), None);
        assert_eq!(split_drm_connector("renderD128"), None);

        assert!(is_internal_connector("card1-eDP-1", &defaults));
        assert!(is_internal_connector("card0-LVDS-1", &defaults));
        assert!(is_internal_connector("card0-DSI-1", &defaults));
        assert!(!is_internal_connector("card1-DP-1", &defaults));
        assert!(!is_internal_connector("card0-HDMI-A-1", &defaults));
    }

    #[test]
    fn scaling_and_clamping_math() {
        assert_eq!(raw_to_pct(0, 100), 0);
        assert_eq!(raw_to_pct(50, 100), 50);
        assert_eq!(raw_to_pct(100, 100), 100);
        assert_eq!(raw_to_pct(128, 255), 50);
        assert_eq!(raw_to_pct(10, 0), 0);

        assert_eq!(pct_to_raw(0, 100), 0);
        assert_eq!(pct_to_raw(50, 100), 50);
        assert_eq!(pct_to_raw(50, 255), 128);
        assert_eq!(pct_to_raw(100, 255), 255);

        assert_eq!(clamp_pct_delta(98, 5), 100);
        assert_eq!(clamp_pct_delta(3, -5), 0);
        assert_eq!(clamp_pct_delta(50, -5), 45);
    }

    #[test]
    fn tooltip_formatting_empty_single_and_multi() {
        let cfg = Config::default();

        // Empty
        let empty = render_waybar_output(&[], &cfg);
        assert_eq!(empty, WaybarOutput::empty());
        assert_eq!(empty.to_json_line(), "{\"text\":\"\"}");

        // Single monitor (theme-agnostic Pango + escaped special chars)
        let single = render_waybar_output(
            &[MonitorState {
                bus: 4,
                connector: "card1-DP-1".to_string(),
                manufacturer: Some("DEL".to_string()),
                model: "DELL U2723QE <HDR>".to_string(),
                current: 65,
                max_val: 100,
                pct: 65,
                target_pct: 65,
                applied_pct: 65,
                fail_count: 0,
            }],
            &cfg,
        );
        assert_eq!(single.text, format!("{} 65%", cfg.icons.monitor));
        assert_eq!(single.percentage, Some(65));
        let tip = single.tooltip.as_deref().unwrap();
        assert!(tip.contains("<b>DELL U2723QE &lt;HDR&gt; · 65%</b>"));
        assert!(tip.contains("<span alpha='75%'>DP-1 · DDC/CI</span>"));
        assert!(tip.contains("<span alpha='55%'>Scroll: Adjust brightness · Click: Refresh</span>"));

        // Multi monitor
        let multi = render_waybar_output(
            &[
                MonitorState {
                    bus: 4,
                    connector: "card1-DP-1".to_string(),
                    manufacturer: Some("DEL".to_string()),
                    model: "U2723QE".to_string(),
                    current: 80,
                    max_val: 100,
                    pct: 80,
                    target_pct: 80,
                    applied_pct: 80,
                    fail_count: 0,
                },
                MonitorState {
                    bus: 5,
                    connector: "card0-HDMI-A-1".to_string(),
                    manufacturer: None,
                    model: "HDMI-A-1".to_string(),
                    current: 40,
                    max_val: 100,
                    pct: 40,
                    target_pct: 40,
                    applied_pct: 40,
                    fail_count: 0,
                },
            ],
            &cfg,
        );
        assert_eq!(multi.text, format!("{} 80%", cfg.icons.monitor));
        assert_eq!(multi.percentage, Some(80));
        let mtip = multi.tooltip.as_deref().unwrap();
        assert!(mtip.contains("<b>U2723QE (DP-1) · 80%</b>"));
        assert!(mtip.contains("\nHDMI-A-1 · 40%\n"));
    }
}
