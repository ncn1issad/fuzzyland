use crate::config::runtime_dir;
use crate::daemon::AppContext;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{btree_map::Entry, BTreeMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;
use tracing::{debug, warn};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::Connection;

const SYSFS_BACKLIGHT_ROOT: &str = "/sys/class/backlight";
const STATE_FILE_NAME: &str = "internal-backlight-state.json";
const LOCK_FILE_NAME: &str = "internal-backlight.lock";

/// Standard perceptual display gamma curve: `raw / max = (pct / 100)^gamma`.
/// Gives finer raw steps at low brightness levels and visually uniform steps.
const PERCEPTUAL_GAMMA: f64 = 2.2;
const MIN_NONZERO_RAW: u32 = 1;

const LOGIN1_DEST: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const LOGIN1_MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const LOGIN1_SESSION_IFACE: &str = "org.freedesktop.login1.Session";
const LOGIN1_USER_IFACE: &str = "org.freedesktop.login1.User";
const LOGIN1_SESSION_AUTO_PATH: &str = "/org/freedesktop/login1/session/auto";
const LOGIN1_USER_SELF_PATH: &str = "/org/freedesktop/login1/user/self";
const DBUS_PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

static OP_MUTEX: Mutex<()> = Mutex::const_new(());
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BacklightType {
    Firmware = 0,
    Platform = 1,
    Raw = 2,
    Unknown = 3,
}

impl BacklightType {
    fn from_sysfs(raw: &str) -> Self {
        match raw.trim() {
            "firmware" => Self::Firmware,
            "platform" => Self::Platform,
            "raw" => Self::Raw,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BacklightDevice {
    name: String,
    brightness: u32,
    max_brightness: u32,
    kind: BacklightType,
    disconnected: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct DimState {
    #[serde(default)]
    saved_raw: BTreeMap<String, u32>,
}

#[derive(Debug, Clone)]
struct RuntimePaths {
    dir: PathBuf,
    lock_file: PathBuf,
    state_file: PathBuf,
}

impl RuntimePaths {
    fn default_runtime() -> Self {
        Self::in_dir(runtime_dir())
    }

    fn in_dir(dir: PathBuf) -> Self {
        let lock_file = dir.join(LOCK_FILE_NAME);
        let state_file = dir.join(STATE_FILE_NAME);
        Self {
            dir,
            lock_file,
            state_file,
        }
    }
}

struct FileLockGuard {
    _file: File,
}

trait BrightnessWriter {
    async fn set_brightness(&self, device: &str, raw: u32) -> Result<()>;
}

struct SystemBrightnessWriter<'a> {
    ctx: &'a AppContext,
    sysfs_root: &'a Path,
}

impl BrightnessWriter for SystemBrightnessWriter<'_> {
    async fn set_brightness(&self, device: &str, raw: u32) -> Result<()> {
        set_brightness_hardware(self.ctx, self.sysfs_root, device, raw).await
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub async fn adjust(ctx: &AppContext, delta: i32, device: Option<&str>) -> Result<()> {
    let _in_proc = OP_MUTEX.lock().await;
    let sysfs_root = Path::new(SYSFS_BACKLIGHT_ROOT);
    let paths = RuntimePaths::default_runtime();
    let writer = SystemBrightnessWriter { ctx, sysfs_root };
    adjust_with(sysfs_root, &paths, &writer, delta, device).await
}

pub async fn dim(ctx: &AppContext, percent: u8, device: Option<&str>) -> Result<()> {
    let _in_proc = OP_MUTEX.lock().await;
    let sysfs_root = Path::new(SYSFS_BACKLIGHT_ROOT);
    let paths = RuntimePaths::default_runtime();
    let writer = SystemBrightnessWriter { ctx, sysfs_root };
    dim_with(sysfs_root, &paths, &writer, percent, device).await
}

pub async fn restore(ctx: &AppContext, device: Option<&str>) -> Result<()> {
    let _in_proc = OP_MUTEX.lock().await;
    let sysfs_root = Path::new(SYSFS_BACKLIGHT_ROOT);
    let paths = RuntimePaths::default_runtime();
    let writer = SystemBrightnessWriter { ctx, sysfs_root };
    restore_with(sysfs_root, &paths, &writer, device).await
}

// ---------------------------------------------------------------------------
// Core Operations
// ---------------------------------------------------------------------------

async fn adjust_with<W: BrightnessWriter>(
    sysfs_root: &Path,
    paths: &RuntimePaths,
    writer: &W,
    delta: i32,
    device: Option<&str>,
) -> Result<()> {
    if let Some(name) = device {
        validate_device_name(name)?;
    }
    if delta == 0 {
        return Ok(());
    }

    let _file_lock = acquire_file_lock(paths.dir.clone(), paths.lock_file.clone()).await?;
    let dev = resolve_device_async(sysfs_root.to_path_buf(), device.map(str::to_owned)).await?;
    let target = compute_adjusted_raw(dev.brightness, dev.max_brightness, delta)?;

    if target == dev.brightness {
        return Ok(());
    }

    writer.set_brightness(&dev.name, target).await
}

async fn dim_with<W: BrightnessWriter>(
    sysfs_root: &Path,
    paths: &RuntimePaths,
    writer: &W,
    percent: u8,
    device: Option<&str>,
) -> Result<()> {
    if percent > 100 {
        bail!("Dim percentage must be between 0 and 100 (got {percent})");
    }
    if let Some(name) = device {
        validate_device_name(name)?;
    }

    let _file_lock = acquire_file_lock(paths.dir.clone(), paths.lock_file.clone()).await?;
    let dev = resolve_device_async(sysfs_root.to_path_buf(), device.map(str::to_owned)).await?;

    let Some(target) = compute_dim_raw(dev.brightness, dev.max_brightness, percent)? else {
        debug!(
            "Skipping dim for '{}': current raw {} is already <= target for {}%",
            dev.name, dev.brightness, percent
        );
        return Ok(());
    };

    let mut state = load_state(&paths.state_file)?;
    let saved_now = if let Entry::Vacant(entry) = state.saved_raw.entry(dev.name.clone()) {
        // Save the original brightness before changing it, including across daemon restarts.
        entry.insert(dev.brightness);
        save_state(&paths.dir, &paths.state_file, &state)?;
        true
    } else {
        false
    };

    if let Err(err) = writer.set_brightness(&dev.name, target).await {
        if saved_now {
            state.saved_raw.remove(&dev.name);
            if let Err(cleanup_err) = save_state(&paths.dir, &paths.state_file, &state) {
                warn!("Failed to clear saved brightness after dim failed: {cleanup_err:#}");
            }
        }
        return Err(err);
    }
    Ok(())
}

async fn restore_with<W: BrightnessWriter>(
    sysfs_root: &Path,
    paths: &RuntimePaths,
    writer: &W,
    device: Option<&str>,
) -> Result<()> {
    if let Some(name) = device {
        validate_device_name(name)?;
    }

    let _file_lock = acquire_file_lock(paths.dir.clone(), paths.lock_file.clone()).await?;
    let dev = resolve_device_async(sysfs_root.to_path_buf(), device.map(str::to_owned)).await?;
    let mut state = load_state(&paths.state_file)?;

    let Some(&saved_raw) = state.saved_raw.get(&dev.name) else {
        debug!("No saved brightness to restore for '{}'", dev.name);
        return Ok(());
    };

    let target = saved_raw.min(dev.max_brightness);
    // Preserve state if the hardware write fails so a later restore can retry.
    writer.set_brightness(&dev.name, target).await?;

    state.saved_raw.remove(&dev.name);
    save_state(&paths.dir, &paths.state_file, &state)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Perceptual Brightness Math
// ---------------------------------------------------------------------------

fn min_nonzero_raw(max_brightness: u32) -> u32 {
    MIN_NONZERO_RAW.min(max_brightness)
}

fn raw_to_perceptual_pct(raw: u32, max_brightness: u32) -> f64 {
    if max_brightness == 0 || raw == 0 {
        return 0.0;
    }
    let norm = (f64::from(raw.min(max_brightness)) / f64::from(max_brightness)).clamp(0.0, 1.0);
    100.0 * norm.powf(1.0 / PERCEPTUAL_GAMMA)
}

fn perceptual_pct_to_raw(pct: f64, max_brightness: u32) -> u32 {
    if max_brightness == 0 {
        return 0;
    }
    let norm = (pct.clamp(0.0, 100.0) / 100.0).powf(PERCEPTUAL_GAMMA);
    let raw = (f64::from(max_brightness) * norm).round();
    raw.clamp(0.0, f64::from(max_brightness)) as u32
}

fn compute_adjusted_raw(current_raw: u32, max_brightness: u32, delta: i32) -> Result<u32> {
    if max_brightness == 0 {
        bail!("Backlight max_brightness cannot be zero");
    }

    let current = current_raw.min(max_brightness);
    if delta == 0 {
        return Ok(current);
    }

    // Never brighten on a negative delta when brightness is already 0.
    if delta < 0 && current == 0 {
        return Ok(0);
    }

    let min_raw = min_nonzero_raw(max_brightness);
    let current_pct = raw_to_perceptual_pct(current, max_brightness);
    let target_pct = (current_pct + f64::from(delta)).clamp(0.0, 100.0);
    let mut target = perceptual_pct_to_raw(target_pct, max_brightness).max(min_raw);

    // Guarantee at least one raw step of progress on low-resolution devices
    // when not already at the corresponding bound.
    if delta > 0 && current < max_brightness && target <= current {
        target = current + 1;
    } else if delta < 0 && current > min_raw && target >= current {
        target = current - 1;
    }

    Ok(target.clamp(min_raw, max_brightness))
}

fn compute_dim_raw(current_raw: u32, max_brightness: u32, percent: u8) -> Result<Option<u32>> {
    if max_brightness == 0 {
        bail!("Backlight max_brightness cannot be zero");
    }
    if percent > 100 {
        bail!("Dim percentage must be between 0 and 100 (got {percent})");
    }

    let current = current_raw.min(max_brightness);
    let min_raw = min_nonzero_raw(max_brightness);
    let target =
        perceptual_pct_to_raw(f64::from(percent), max_brightness).clamp(min_raw, max_brightness);

    if current <= target {
        Ok(None)
    } else {
        Ok(Some(target))
    }
}

// ---------------------------------------------------------------------------
// Sysfs Device Discovery & Validation
// ---------------------------------------------------------------------------

fn validate_device_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        bail!("Invalid backlight device name: '{name}'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '@' | '+'))
    {
        bail!("Invalid backlight device name: '{name}'");
    }
    Ok(())
}

fn read_u32_file(path: &Path) -> Result<u32> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    raw.trim()
        .parse::<u32>()
        .with_context(|| format!("Invalid integer in {}: '{}'", path.display(), raw.trim()))
}

fn read_device_from(root: &Path, name: &str) -> Result<BacklightDevice> {
    validate_device_name(name)?;
    let dev_dir = root.join(name);
    if !dev_dir.is_dir() {
        bail!(
            "Backlight device '{}' not found in {}",
            name,
            root.display()
        );
    }

    let max_brightness = read_u32_file(&dev_dir.join("max_brightness"))?;
    if max_brightness == 0 {
        bail!("Backlight device '{}' reported max_brightness of 0", name);
    }
    let brightness = read_u32_file(&dev_dir.join("brightness"))?;

    let kind = std::fs::read_to_string(dev_dir.join("type"))
        .map(|s| BacklightType::from_sysfs(&s))
        .unwrap_or(BacklightType::Unknown);

    let disconnected = std::fs::read_to_string(dev_dir.join("device").join("status"))
        .map(|s| s.trim().eq_ignore_ascii_case("disconnected"))
        .unwrap_or(false);

    Ok(BacklightDevice {
        name: name.to_string(),
        brightness,
        max_brightness,
        kind,
        disconnected,
    })
}

fn resolve_device_from(root: &Path, requested: Option<&str>) -> Result<BacklightDevice> {
    if let Some(name) = requested {
        return read_device_from(root, name);
    }

    let entries = std::fs::read_dir(root)
        .with_context(|| format!("Failed to read backlight directory {}", root.display()))?;

    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        match read_device_from(root, &name) {
            Ok(dev) => devices.push(dev),
            Err(err) => debug!("Skipping backlight entry '{name}': {err:#}"),
        }
    }

    // Deterministic selection following Linux sysfs-class-backlight ABI priority:
    // connected/non-DRM before explicitly disconnected, firmware > platform > raw,
    // higher max_brightness, then lexicographical device name.
    devices.sort_by(|a, b| {
        a.disconnected
            .cmp(&b.disconnected)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| b.max_brightness.cmp(&a.max_brightness))
            .then_with(|| a.name.cmp(&b.name))
    });

    devices.into_iter().next().ok_or_else(|| {
        anyhow!(
            "No usable internal backlight devices found in {}",
            root.display()
        )
    })
}

async fn resolve_device_async(root: PathBuf, requested: Option<String>) -> Result<BacklightDevice> {
    tokio::task::spawn_blocking(move || resolve_device_from(&root, requested.as_deref()))
        .await
        .context("Backlight device probe worker panicked")?
}

// ---------------------------------------------------------------------------
// Hardware Write: login1 Session.SetBrightness + Direct Sysfs Fallback
// ---------------------------------------------------------------------------

async fn set_brightness_hardware(
    ctx: &AppContext,
    sysfs_root: &Path,
    device: &str,
    raw: u32,
) -> Result<()> {
    let login1_err = match ctx.system_bus().await {
        Ok(conn) => match set_brightness_via_login1(conn, device, raw).await {
            Ok(()) => return Ok(()),
            Err(err) => err,
        },
        Err(err) => err,
    };

    debug!(
        "login1 SetBrightness unavailable for '{}': {login1_err:#}; falling back to sysfs write",
        device
    );

    let sysfs_path = sysfs_root.join(device).join("brightness");
    let write_res = tokio::task::spawn_blocking(move || write_sysfs_brightness(&sysfs_path, raw))
        .await
        .context("Sysfs brightness write worker panicked")?;

    match write_res {
        Ok(()) => {
            debug!("Set backlight '{}' to {} via direct sysfs write", device, raw);
            Ok(())
        }
        Err(sysfs_err) => Err(anyhow!(
            "Failed to set backlight '{}' to {}: login1 failed ({login1_err:#}); sysfs fallback failed ({sysfs_err:#})",
            device,
            raw
        )),
    }
}

async fn set_brightness_via_login1(conn: &Connection, device: &str, raw: u32) -> Result<()> {
    let candidates = resolve_login1_session_paths(conn).await;
    let mut last_err: Option<anyhow::Error> = None;

    for path in candidates {
        match conn
            .call_method(
                Some(LOGIN1_DEST),
                path.as_str(),
                Some(LOGIN1_SESSION_IFACE),
                "SetBrightness",
                &("backlight", device, raw),
            )
            .await
        {
            Ok(_) => {
                debug!(
                    "Set backlight '{}' to {} via login1 session {}",
                    device, raw, path
                );
                return Ok(());
            }
            Err(err) => {
                debug!(
                    "login1 SetBrightness failed on session {} for '{}': {}",
                    path, device, err
                );
                last_err = Some(anyhow!("session {path}: {err}"));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no usable login1 session found")))
}

fn parse_login1_display_value(val: OwnedValue) -> Option<String> {
    let (session_id, obj_path) = <(String, OwnedObjectPath)>::try_from(val).ok()?;
    if session_id.is_empty() || obj_path.as_str() == "/" {
        return None;
    }
    Some(obj_path.as_str().to_string())
}

async fn resolve_login1_session_paths(conn: &Connection) -> Vec<String> {
    let mut paths = Vec::new();

    if let Ok(id) = std::env::var("XDG_SESSION_ID") {
        if !id.is_empty() {
            let reply = conn
                .call_method(
                    Some(LOGIN1_DEST),
                    LOGIN1_PATH,
                    Some(LOGIN1_MANAGER_IFACE),
                    "GetSession",
                    &(id,),
                )
                .await;
            if let Ok(path) = reply.and_then(|reply| reply.body().deserialize::<OwnedObjectPath>())
            {
                paths.push(path.to_string());
            }
        }
    }

    paths.push(LOGIN1_SESSION_AUTO_PATH.to_string());

    // systemd --user daemons live outside a login session cgroup, so
    // User.Display resolves the user's primary graphical session.
    let display_reply = conn
        .call_method(
            Some(LOGIN1_DEST),
            LOGIN1_USER_SELF_PATH,
            Some(DBUS_PROPERTIES_IFACE),
            "Get",
            &(LOGIN1_USER_IFACE, "Display"),
        )
        .await;
    if let Some(path) = display_reply
        .ok()
        .and_then(|reply| reply.body().deserialize::<OwnedValue>().ok())
        .and_then(parse_login1_display_value)
        .filter(|path| !paths.contains(path))
    {
        paths.push(path);
    }

    paths
}

fn write_sysfs_brightness(path: &Path, raw: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("Failed to open {} for writing", path.display()))?;
    let payload = format!("{raw}\n");
    file.write_all(payload.as_bytes())
        .with_context(|| format!("Failed to write brightness to {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared Runtime State & File Locking
// ---------------------------------------------------------------------------

async fn acquire_file_lock(dir: PathBuf, lock_path: PathBuf) -> Result<FileLockGuard> {
    tokio::task::spawn_blocking(move || acquire_file_lock_sync(&dir, &lock_path))
        .await
        .context("Backlight lock worker panicked")?
}

fn acquire_file_lock_sync(dir: &Path, lock_path: &Path) -> Result<FileLockGuard> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create runtime dir {}", dir.display()))?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)
        .with_context(|| format!("Failed to open lock file {}", lock_path.display()))?;
    let _ = std::fs::set_permissions(lock_path, std::fs::Permissions::from_mode(0o600));

    loop {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc == 0 {
            return Ok(FileLockGuard { _file: file });
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(anyhow!("Failed to lock {}: {err}", lock_path.display()));
    }
}

fn load_state(path: &Path) -> Result<DimState> {
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<DimState>(&raw) {
            Ok(state) => Ok(state),
            Err(err) => {
                warn!(
                    "Ignoring corrupt internal backlight state at {}: {err}",
                    path.display()
                );
                Ok(DimState::default())
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DimState::default()),
        Err(err) => Err(err).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn save_state(dir: &Path, path: &Path, state: &DimState) -> Result<()> {
    if state.saved_raw.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to remove {}", path.display()));
            }
        }
    }

    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create runtime dir {}", dir.display()))?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

    let bytes =
        serde_json::to_vec(state).context("Failed to serialize internal backlight state")?;
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!(
        ".{STATE_FILE_NAME}.tmp.{}.{seq}",
        std::process::id()
    ));

    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)
            .with_context(|| format!("Failed to create temp state file {}", tmp_path.display()))?;
        file.write_all(&bytes)?;
        file.flush()?;
        drop(file);
        std::fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "Failed to replace {} with {}",
                path.display(),
                tmp_path.display()
            )
        })?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    write_result
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Arc;
    use std::time::Duration;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fuzzyland-internal-backlight-{label}-{}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn create_fake_device(
        root: &Path,
        name: &str,
        brightness: u32,
        max_brightness: u32,
        kind: Option<&str>,
        status: Option<&str>,
    ) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("brightness"), format!("{brightness}\n")).unwrap();
        std::fs::write(dir.join("max_brightness"), format!("{max_brightness}\n")).unwrap();
        if let Some(kind) = kind {
            std::fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
        }
        if let Some(status) = status {
            let dev_sub = dir.join("device");
            std::fs::create_dir_all(&dev_sub).unwrap();
            std::fs::write(dev_sub.join("status"), format!("{status}\n")).unwrap();
        }
    }

    struct FakeWriter {
        sysfs_root: PathBuf,
        fail: AtomicBool,
        writes: std::sync::Mutex<Vec<(String, u32)>>,
    }

    impl FakeWriter {
        fn new(sysfs_root: &Path) -> Self {
            Self {
                sysfs_root: sysfs_root.to_path_buf(),
                fail: AtomicBool::new(false),
                writes: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn set_fail(&self, fail: bool) {
            self.fail.store(fail, Ordering::SeqCst);
        }

        fn recorded_writes(&self) -> Vec<(String, u32)> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl BrightnessWriter for FakeWriter {
        async fn set_brightness(&self, device: &str, raw: u32) -> Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                bail!("simulated hardware write failure");
            }
            let path = self.sysfs_root.join(device).join("brightness");
            std::fs::write(path, format!("{raw}\n"))?;
            self.writes.lock().unwrap().push((device.to_string(), raw));
            Ok(())
        }
    }

    #[test]
    fn perceptual_curve_gives_finer_steps_at_low_levels() {
        let max = 1000;
        let low_raw = perceptual_pct_to_raw(10.0, max);
        let high_raw = perceptual_pct_to_raw(80.0, max);

        let low_next = compute_adjusted_raw(low_raw, max, 5).unwrap();
        let high_next = compute_adjusted_raw(high_raw, max, 5).unwrap();

        let low_step = low_next - low_raw;
        let high_step = high_next - high_raw;

        assert!(low_step > 0);
        assert!(
            low_step < high_step,
            "expected finer raw step at low level ({low_step}) than high level ({high_step})"
        );
        assert_eq!(perceptual_pct_to_raw(0.0, max), 0);
        assert_eq!(perceptual_pct_to_raw(100.0, max), max);
        assert!(perceptual_pct_to_raw(50.0, max) < max / 2);
    }

    #[test]
    fn adjust_clamps_to_nonzero_minimum_and_maximum() {
        let max = 1000;
        assert_eq!(compute_adjusted_raw(50, max, -100).unwrap(), 1);
        assert_eq!(compute_adjusted_raw(1, max, -5).unwrap(), 1);
        assert_eq!(compute_adjusted_raw(50, max, i32::MIN).unwrap(), 1);
        assert_eq!(compute_adjusted_raw(50, max, i32::MAX).unwrap(), max);
        assert_eq!(compute_adjusted_raw(max, max, 5).unwrap(), max);
        // If externally set to 0, negative delta must not brighten.
        assert_eq!(compute_adjusted_raw(0, max, -5).unwrap(), 0);
        // Positive delta from 0 raises to a nonzero level.
        assert!(compute_adjusted_raw(0, max, 5).unwrap() >= 1);
        // Zero delta is a no-op.
        assert_eq!(compute_adjusted_raw(123, max, 0).unwrap(), 123);
        assert!(compute_adjusted_raw(0, 0, 5).is_err());
    }

    #[test]
    fn adjust_makes_strict_progress_on_low_resolution_devices() {
        for max in [15_u32, 100, 255] {
            let mut raw = 1;
            let mut steps_up = 0;
            while raw < max {
                let next = compute_adjusted_raw(raw, max, 1).unwrap();
                assert!(
                    next > raw,
                    "stuck stepping up on max={max} at raw={raw} (next={next})"
                );
                raw = next;
                steps_up += 1;
                assert!(steps_up <= max);
            }

            let mut steps_down = 0;
            while raw > 1 {
                let next = compute_adjusted_raw(raw, max, -1).unwrap();
                assert!(
                    next < raw,
                    "stuck stepping down on max={max} at raw={raw} (next={next})"
                );
                assert!(next >= 1);
                raw = next;
                steps_down += 1;
                assert!(steps_down <= max);
            }
        }
    }

    #[test]
    fn dim_math_only_reduces_and_enforces_nonzero_min_and_bounds() {
        let max = 1000;
        // dim(0) clamps to minimum nonzero raw (1).
        assert_eq!(compute_dim_raw(500, max, 0).unwrap(), Some(1));
        // Already at or below target -> no-op (never brightens).
        assert_eq!(compute_dim_raw(1, max, 0).unwrap(), None);
        assert_eq!(compute_dim_raw(0, max, 10).unwrap(), None);
        assert_eq!(compute_dim_raw(5, max, 20).unwrap(), None);
        assert_eq!(compute_dim_raw(500, max, 100).unwrap(), None);
        // Out of range percentage rejected.
        assert!(compute_dim_raw(500, max, 101).is_err());
    }

    #[test]
    fn validate_device_name_rejects_invalid_and_traversal() {
        for valid in [
            "intel_backlight",
            "amdgpu_bl0",
            "nvidia_wmi_ec_backlight",
            "panel-bl.0",
            "backlight@0",
            "acpi_video0",
        ] {
            assert!(
                validate_device_name(valid).is_ok(),
                "expected '{valid}' to be valid"
            );
        }

        for invalid in [
            "",
            ".",
            "..",
            "../etc",
            "a/b",
            "a\\b",
            "intel backlight",
            "intel\0backlight",
            "intel\nbacklight",
        ] {
            assert!(
                validate_device_name(invalid).is_err(),
                "expected '{invalid}' to be rejected"
            );
        }
    }

    #[test]
    fn resolve_device_is_deterministic_across_type_status_max_and_name() {
        let tmp = TempDir::new("select");
        let root = tmp.path();

        // Firmware beats platform and raw; disconnected is deprioritized.
        create_fake_device(
            root,
            "z_raw_high",
            100,
            10000,
            Some("raw"),
            Some("connected"),
        );
        create_fake_device(
            root,
            "a_fw_disc",
            50,
            100,
            Some("firmware"),
            Some("disconnected"),
        );
        create_fake_device(root, "m_platform", 50, 500, Some("platform"), None);

        let selected = resolve_device_from(root, None).unwrap();
        assert_eq!(selected.name, "m_platform");

        // Adding a connected firmware device takes top priority.
        create_fake_device(
            root,
            "b_fw_conn",
            50,
            100,
            Some("firmware"),
            Some("connected"),
        );
        let selected = resolve_device_from(root, None).unwrap();
        assert_eq!(selected.name, "b_fw_conn");

        // Explicit device overrides ranking.
        let explicit = resolve_device_from(root, Some("z_raw_high")).unwrap();
        assert_eq!(explicit.name, "z_raw_high");

        // Tie-breaking among same type: higher max_brightness, then lexicographical name.
        let tmp2 = TempDir::new("tiebreak");
        create_fake_device(tmp2.path(), "amdgpu_bl1", 100, 255, Some("raw"), None);
        create_fake_device(tmp2.path(), "amdgpu_bl0", 100, 255, Some("raw"), None);
        create_fake_device(tmp2.path(), "ddcci0", 50, 100, Some("raw"), None);
        assert_eq!(
            resolve_device_from(tmp2.path(), None).unwrap().name,
            "amdgpu_bl0"
        );
    }

    #[tokio::test]
    async fn dim_repeated_preserves_original_and_restore_roundtrips() {
        let sysfs = TempDir::new("dim-sysfs");
        let runtime = TempDir::new("dim-rt");
        create_fake_device(
            sysfs.path(),
            "intel_backlight",
            800,
            1000,
            Some("raw"),
            None,
        );

        let paths = RuntimePaths::in_dir(runtime.path().to_path_buf());
        let writer = FakeWriter::new(sysfs.path());

        // First dim to 30% saves original 800.
        dim_with(sysfs.path(), &paths, &writer, 30, None)
            .await
            .unwrap();
        let after_30 = read_device_from(sysfs.path(), "intel_backlight")
            .unwrap()
            .brightness;
        assert!((1..800).contains(&after_30));
        assert_eq!(
            load_state(&paths.state_file)
                .unwrap()
                .saved_raw
                .get("intel_backlight"),
            Some(&800)
        );

        // Second dim to 10% lowers further but keeps original 800.
        dim_with(sysfs.path(), &paths, &writer, 10, None)
            .await
            .unwrap();
        let after_10 = read_device_from(sysfs.path(), "intel_backlight")
            .unwrap()
            .brightness;
        assert!(after_10 < after_30 && after_10 >= 1);
        assert_eq!(
            load_state(&paths.state_file)
                .unwrap()
                .saved_raw
                .get("intel_backlight"),
            Some(&800)
        );

        // Dim to higher 40% while already at 10% must not brighten and keeps original 800.
        let write_count_before = writer.recorded_writes().len();
        dim_with(sysfs.path(), &paths, &writer, 40, None)
            .await
            .unwrap();
        assert_eq!(writer.recorded_writes().len(), write_count_before);
        assert_eq!(
            read_device_from(sysfs.path(), "intel_backlight")
                .unwrap()
                .brightness,
            after_10
        );
        assert_eq!(
            load_state(&paths.state_file)
                .unwrap()
                .saved_raw
                .get("intel_backlight"),
            Some(&800)
        );

        // Restore from a separate RuntimePaths instance (simulating standalone <-> daemon).
        let separate_paths = RuntimePaths::in_dir(runtime.path().to_path_buf());
        restore_with(sysfs.path(), &separate_paths, &writer, None)
            .await
            .unwrap();
        assert_eq!(
            read_device_from(sysfs.path(), "intel_backlight")
                .unwrap()
                .brightness,
            800
        );
        assert!(!paths.state_file.exists());

        // Second restore is a clean no-op.
        let writes_after_restore = writer.recorded_writes().len();
        restore_with(sysfs.path(), &separate_paths, &writer, None)
            .await
            .unwrap();
        assert_eq!(writer.recorded_writes().len(), writes_after_restore);
    }

    #[tokio::test]
    async fn dim_already_below_target_is_noop_without_creating_state() {
        let sysfs = TempDir::new("dim-noop-sysfs");
        let runtime = TempDir::new("dim-noop-rt");
        create_fake_device(sysfs.path(), "amdgpu_bl0", 5, 1000, Some("raw"), None);

        let paths = RuntimePaths::in_dir(runtime.path().to_path_buf());
        let writer = FakeWriter::new(sysfs.path());

        dim_with(sysfs.path(), &paths, &writer, 50, None)
            .await
            .unwrap();
        assert!(writer.recorded_writes().is_empty());
        assert!(!paths.state_file.exists());
        assert_eq!(
            read_device_from(sysfs.path(), "amdgpu_bl0")
                .unwrap()
                .brightness,
            5
        );
    }

    #[tokio::test]
    async fn failed_restore_preserves_state_and_failed_dim_leaves_no_state() {
        let sysfs = TempDir::new("fail-sysfs");
        let runtime = TempDir::new("fail-rt");
        create_fake_device(
            sysfs.path(),
            "intel_backlight",
            750,
            1000,
            Some("raw"),
            None,
        );

        let paths = RuntimePaths::in_dir(runtime.path().to_path_buf());
        let writer = FakeWriter::new(sysfs.path());

        // Failed initial dim must not leave behind bogus saved state.
        writer.set_fail(true);
        assert!(dim_with(sysfs.path(), &paths, &writer, 10, None)
            .await
            .is_err());
        assert!(!paths.state_file.exists());

        // Successful dim saves 750.
        writer.set_fail(false);
        dim_with(sysfs.path(), &paths, &writer, 10, None)
            .await
            .unwrap();
        assert_eq!(
            load_state(&paths.state_file)
                .unwrap()
                .saved_raw
                .get("intel_backlight"),
            Some(&750)
        );

        // Failed restore must preserve saved state.
        writer.set_fail(true);
        assert!(restore_with(sysfs.path(), &paths, &writer, None)
            .await
            .is_err());
        assert_eq!(
            load_state(&paths.state_file)
                .unwrap()
                .saved_raw
                .get("intel_backlight"),
            Some(&750)
        );

        // Subsequent retry succeeds and clears saved state.
        writer.set_fail(false);
        restore_with(sysfs.path(), &paths, &writer, None)
            .await
            .unwrap();
        assert_eq!(
            read_device_from(sysfs.path(), "intel_backlight")
                .unwrap()
                .brightness,
            750
        );
        assert!(!paths.state_file.exists());
    }

    #[tokio::test]
    async fn multi_device_state_is_independent_and_corrupt_state_recovers() {
        let sysfs = TempDir::new("multi-sysfs");
        let runtime = TempDir::new("multi-rt");
        create_fake_device(sysfs.path(), "dev_a", 600, 1000, Some("raw"), None);
        create_fake_device(sysfs.path(), "dev_b", 900, 1000, Some("raw"), None);

        let paths = RuntimePaths::in_dir(runtime.path().to_path_buf());
        let writer = FakeWriter::new(sysfs.path());

        dim_with(sysfs.path(), &paths, &writer, 10, Some("dev_a"))
            .await
            .unwrap();
        dim_with(sysfs.path(), &paths, &writer, 10, Some("dev_b"))
            .await
            .unwrap();

        restore_with(sysfs.path(), &paths, &writer, Some("dev_a"))
            .await
            .unwrap();
        let state = load_state(&paths.state_file).unwrap();
        assert!(!state.saved_raw.contains_key("dev_a"));
        assert_eq!(state.saved_raw.get("dev_b"), Some(&900));

        // Corrupt state JSON recovers cleanly instead of wedging dim/restore.
        std::fs::write(&paths.state_file, "{not valid json").unwrap();
        assert!(load_state(&paths.state_file).unwrap().saved_raw.is_empty());
    }

    #[tokio::test]
    async fn file_lock_serializes_concurrent_state_updates() {
        let sysfs = TempDir::new("lock-sysfs");
        let runtime = TempDir::new("lock-rt");
        create_fake_device(sysfs.path(), "dev_a", 800, 1000, Some("raw"), None);
        create_fake_device(sysfs.path(), "dev_b", 700, 1000, Some("raw"), None);

        let paths = RuntimePaths::in_dir(runtime.path().to_path_buf());

        struct SlowWriter {
            sysfs_root: PathBuf,
            active: Arc<AtomicUsize>,
            max_concurrent: Arc<AtomicUsize>,
        }

        impl BrightnessWriter for SlowWriter {
            async fn set_brightness(&self, device: &str, raw: u32) -> Result<()> {
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_concurrent.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                std::fs::write(
                    self.sysfs_root.join(device).join("brightness"),
                    format!("{raw}\n"),
                )?;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let active = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let w1 = SlowWriter {
            sysfs_root: sysfs.path().to_path_buf(),
            active: Arc::clone(&active),
            max_concurrent: Arc::clone(&max_concurrent),
        };
        let w2 = SlowWriter {
            sysfs_root: sysfs.path().to_path_buf(),
            active: Arc::clone(&active),
            max_concurrent: Arc::clone(&max_concurrent),
        };

        let (r1, r2) = tokio::join!(
            dim_with(sysfs.path(), &paths, &w1, 10, Some("dev_a")),
            dim_with(sysfs.path(), &paths, &w2, 10, Some("dev_b")),
        );
        r1.unwrap();
        r2.unwrap();

        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "concurrent dim operations must be serialized by the runtime lock"
        );

        let state = load_state(&paths.state_file).unwrap();
        assert_eq!(state.saved_raw.get("dev_a"), Some(&800));
        assert_eq!(state.saved_raw.get("dev_b"), Some(&700));
    }

    #[test]
    fn parse_login1_display_and_sysfs_fallback_write() {
        use zbus::zvariant::Value;

        let obj_path = OwnedObjectPath::try_from("/org/freedesktop/login1/session/_32").unwrap();
        let display_val = OwnedValue::try_from(Value::from(("2".to_string(), obj_path))).unwrap();
        assert_eq!(
            parse_login1_display_value(display_val).as_deref(),
            Some("/org/freedesktop/login1/session/_32")
        );

        let root_path = OwnedObjectPath::try_from("/").unwrap();
        let empty_val = OwnedValue::try_from(Value::from((String::new(), root_path))).unwrap();
        assert_eq!(parse_login1_display_value(empty_val), None);

        let tmp = TempDir::new("sysfs-write");
        let existing = tmp.path().join("brightness");
        std::fs::write(&existing, "500\n").unwrap();
        write_sysfs_brightness(&existing, 42).unwrap();
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "42\n");

        // Must not create a missing sysfs attribute file.
        let missing = tmp.path().join("nonexistent");
        assert!(write_sysfs_brightness(&missing, 42).is_err());
        assert!(!missing.exists());
    }
}
