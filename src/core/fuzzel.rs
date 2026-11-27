use crate::config::FuzzelConfig;
use crate::daemon::menu::MenuGuard;
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

static PROMPT_ONLY_SUPPORT: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Center,
    Top,
    TopRight,
    TopLeft,
    Bottom,
    BottomRight,
    BottomLeft,
}

impl Anchor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Center => "center",
            Self::Top => "top",
            Self::TopRight => "top-right",
            Self::TopLeft => "top-left",
            Self::Bottom => "bottom",
            Self::BottomRight => "bottom-right",
            Self::BottomLeft => "bottom-left",
        }
    }

    pub fn has_horizontal_margin(self) -> bool {
        matches!(
            self,
            Self::TopRight | Self::TopLeft | Self::BottomRight | Self::BottomLeft
        )
    }

    pub fn has_vertical_margin(self) -> bool {
        matches!(
            self,
            Self::Top | Self::TopRight | Self::TopLeft | Self::Bottom | Self::BottomRight | Self::BottomLeft
        )
    }
}

#[derive(Debug, Clone)]
pub struct FuzzelOptions {
    pub prompt: String,
    pub placeholder: Option<String>,
    pub mesg: Option<String>,
    pub anchor: Anchor,
    pub width: Option<u32>,
    pub max_lines: Option<usize>,
    pub minimal_lines: bool,
    pub no_sort: bool,
    pub counter: bool,
    pub password: bool,
    pub select_index: Option<usize>,
    pub output: Option<String>,
    pub x_margin: Option<u32>,
    pub y_margin: Option<u32>,
    pub overrides: Vec<String>,
    pub extra_args: Vec<String>,
}

impl Default for FuzzelOptions {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            placeholder: None,
            mesg: None,
            anchor: Anchor::TopRight,
            width: Some(38),
            max_lines: Some(16),
            minimal_lines: true,
            no_sort: false,
            counter: false,
            password: false,
            select_index: None,
            output: None,
            x_margin: None,
            y_margin: None,
            overrides: Vec::new(),
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzelResult {
    pub exit_code: i32,
    pub index: Option<usize>,
    pub text: String,
}

fn prompt_only_cache() -> &'static Mutex<HashMap<String, bool>> {
    PROMPT_ONLY_SUPPORT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_prompt_only_support(binary: &str) -> Option<bool> {
    prompt_only_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(binary)
        .copied()
}

fn cache_prompt_only_support(binary: &str, supported: bool) {
    prompt_only_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(binary.to_string(), supported);
}

fn help_mentions_prompt_only(stdout: &str, stderr: &str) -> bool {
    stdout.contains("--prompt-only") || stderr.contains("--prompt-only")
}

fn is_unsupported_prompt_only_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("prompt-only")
        && (lower.contains("invalid option")
            || lower.contains("unrecognized option")
            || lower.contains("unknown option"))
}

async fn supports_prompt_only(binary: &str) -> bool {
    if let Some(supported) = cached_prompt_only_support(binary) {
        return supported;
    }

    let mut cmd = Command::new(binary);
    cmd.arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };

    let Ok(Ok(output)) = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await else {
        return false;
    };

    let supported = output.status.success()
        && help_mentions_prompt_only(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        );
    cache_prompt_only_support(binary, supported);
    supported
}

fn build_base_args(cfg: &FuzzelConfig, opts: &FuzzelOptions, prompt_only: bool) -> Vec<String> {
    let mut args = vec!["--dmenu".to_string()];
    args.push(format!("--anchor={}", opts.anchor.as_str()));

    if opts.anchor.has_horizontal_margin() {
        let mx = opts.x_margin.unwrap_or(cfg.anchor_margin_x);
        args.push(format!("--x-margin={mx}"));
    }
    if opts.anchor.has_vertical_margin() {
        let my = opts.y_margin.unwrap_or(cfg.anchor_margin_y);
        args.push(format!("--y-margin={my}"));
    }
    if let Some(width) = opts.width {
        args.push(format!("--width={width}"));
    }
    if prompt_only {
        args.push(format!("--prompt-only={}", opts.prompt));
    } else if !opts.prompt.is_empty() {
        args.push(format!("--prompt={}", opts.prompt));
    }
    if let Some(ref placeholder) = opts.placeholder {
        args.push(format!("--placeholder={placeholder}"));
    }
    if let Some(ref mesg) = opts.mesg {
        args.push(format!("--mesg={mesg}"));
    }
    if opts.no_sort {
        args.push("--no-sort".to_string());
    }
    if opts.counter {
        args.push("--counter".to_string());
    }
    if opts.password {
        args.push("--password".to_string());
    }
    if let Some(ref output) = opts.output {
        if !output.is_empty() {
            args.push(format!("--output={output}"));
        }
    }
    for ov in &opts.overrides {
        args.push(format!("--override={ov}"));
    }
    args.extend(opts.extra_args.iter().cloned());
    args
}

fn build_input_args(cfg: &FuzzelConfig, opts: &FuzzelOptions, prompt_only: bool) -> Vec<String> {
    let mut args = build_base_args(cfg, opts, prompt_only);
    if !prompt_only {
        args.push("--lines=0".to_string());
    }
    args
}

async fn run_fuzzel_raw(
    cfg: &FuzzelConfig,
    guard: &MenuGuard,
    args: &[String],
    stdin_text: Option<&str>,
) -> Result<(i32, String, String)> {
    if !guard.is_current() {
        return Ok((-1, String::new(), String::new()));
    }

    let mut cmd = Command::new(&cfg.binary);
    cmd.args(args)
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn fuzzel binary '{}'", cfg.binary))?;

    let pid = child.id();
    if let Some(pid) = pid {
        if !guard.register_child_pid(pid) {
            let _ = child.kill().await;
            return Ok((-1, String::new(), String::new()));
        }
    }

    if let Some(mut stdin) = child.stdin.take() {
        if let Some(text) = stdin_text {
            let _ = stdin.write_all(text.as_bytes()).await;
        }
        let _ = stdin.shutdown().await;
    }

    let output = child.wait_with_output().await;
    if let Some(pid) = pid {
        guard.clear_child_pid(pid);
    }

    let output = output?;
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((code, stdout, stderr))
}

pub async fn select_raw(
    cfg: &FuzzelConfig,
    guard: &MenuGuard,
    lines: &[String],
    opts: &FuzzelOptions,
) -> Result<FuzzelResult> {
    let mut args = build_base_args(cfg, opts, false);
    args.push("--index".to_string());
    if opts.minimal_lines {
        args.push("--minimal-lines".to_string());
    }
    let line_count = match opts.max_lines {
        Some(max) => lines.len().max(1).min(max),
        None => lines.len().max(1),
    };
    args.push(format!("--lines={line_count}"));
    if let Some(sel) = opts.select_index {
        if sel < lines.len() {
            args.push(format!("--select-index={sel}"));
        }
    }

    let input = lines.join("\n");
    let (exit_code, stdout, _) = run_fuzzel_raw(cfg, guard, &args, Some(&input)).await?;
    let trimmed = stdout.trim();
    let index = if trimmed.is_empty() {
        None
    } else {
        trimmed.parse::<usize>().ok()
    };

    Ok(FuzzelResult {
        exit_code,
        index,
        text: trimmed.to_string(),
    })
}

pub async fn select<T>(
    cfg: &FuzzelConfig,
    guard: &MenuGuard,
    entries: &[(String, T)],
    opts: &FuzzelOptions,
) -> Result<Option<usize>> {
    if entries.is_empty() {
        return Err(anyhow!("Cannot open fuzzel menu with empty entries"));
    }
    let labels: Vec<String> = entries.iter().map(|(l, _)| l.clone()).collect();
    let res = select_raw(cfg, guard, &labels, opts).await?;
    if res.exit_code != 0 {
        return Ok(None);
    }
    Ok(res.index.filter(|&i| i < entries.len()))
}

pub async fn input(
    cfg: &FuzzelConfig,
    guard: &MenuGuard,
    opts: &FuzzelOptions,
) -> Result<Option<String>> {
    let use_prompt_only = supports_prompt_only(&cfg.binary).await;
    let args = build_input_args(cfg, opts, use_prompt_only);
    let stdin = if use_prompt_only { None } else { Some("") };
    let (exit_code, stdout, stderr) = run_fuzzel_raw(cfg, guard, &args, stdin).await?;

    let (exit_code, stdout) = if use_prompt_only
        && exit_code != 0
        && stdout.is_empty()
        && guard.is_current()
        && is_unsupported_prompt_only_error(&stderr)
    {
        cache_prompt_only_support(&cfg.binary, false);
        let fallback_args = build_input_args(cfg, opts, false);
        let (code, out, _) = run_fuzzel_raw(cfg, guard, &fallback_args, Some("")).await?;
        (code, out)
    } else {
        (exit_code, stdout)
    };

    if exit_code != 0 {
        return Ok(None);
    }
    let trimmed = stdout.trim_end_matches(['\r', '\n']).to_string();
    Ok(Some(trimmed))
}

pub async fn password(
    cfg: &FuzzelConfig,
    guard: &MenuGuard,
    opts: &FuzzelOptions,
) -> Result<Option<String>> {
    let mut pw_opts = opts.clone();
    pw_opts.password = true;
    input(cfg, guard, &pw_opts).await
}

pub async fn confirm(
    cfg: &FuzzelConfig,
    guard: &MenuGuard,
    yes_label: &str,
    cancel_label: &str,
    opts: &FuzzelOptions,
) -> Result<bool> {
    let entries = vec![
        (yes_label.to_string(), true),
        (cancel_label.to_string(), false),
    ];
    let mut c_opts = opts.clone();
    c_opts.max_lines = Some(2);
    let idx = select(cfg, guard, &entries, &c_opts).await?;
    Ok(matches!(idx, Some(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::menu::{MenuCoordinator, MenuKind, MenuSession};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(prefix: &str) -> Self {
            let base = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .unwrap_or_else(std::env::temp_dir);
            let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = base.join(format!("{prefix}-{}-{seq}", std::process::id()));
            fs::create_dir_all(&dir).expect("create test temp dir");
            Self(dir)
        }

        fn write_script(&self, name: &str, body: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, body).expect("write test script");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .expect("chmod test script");
            path
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn acquire_guard() -> MenuGuard {
        let menu = MenuCoordinator::new(false);
        match menu.begin_session(MenuKind::Network) {
            MenuSession::Acquired(guard) => guard,
            MenuSession::ToggledOff => panic!("expected fresh menu guard"),
        }
    }

    #[test]
    fn build_input_args_switches_between_prompt_only_and_legacy() {
        let cfg = FuzzelConfig::default();
        let opts = FuzzelOptions {
            prompt: "Hidden SSID: ".to_string(),
            placeholder: Some("Network name".to_string()),
            password: true,
            ..Default::default()
        };

        let modern = build_input_args(&cfg, &opts, true);
        assert!(modern.contains(&"--dmenu".to_string()));
        assert!(modern.contains(&"--prompt-only=Hidden SSID: ".to_string()));
        assert!(modern.contains(&"--password".to_string()));
        assert!(!modern.iter().any(|a| a.starts_with("--prompt=")));
        assert!(!modern.contains(&"--lines=0".to_string()));

        let empty_prompt = build_input_args(
            &cfg,
            &FuzzelOptions {
                prompt: String::new(),
                ..Default::default()
            },
            true,
        );
        assert!(empty_prompt.contains(&"--prompt-only=".to_string()));
        assert!(!empty_prompt.contains(&"--lines=0".to_string()));

        let legacy = build_input_args(&cfg, &opts, false);
        assert!(legacy.contains(&"--dmenu".to_string()));
        assert!(legacy.contains(&"--prompt=Hidden SSID: ".to_string()));
        assert!(legacy.contains(&"--lines=0".to_string()));
        assert!(legacy.contains(&"--password".to_string()));
        assert!(!legacy.iter().any(|a| a.starts_with("--prompt-only=")));
    }

    #[tokio::test]
    async fn prompt_only_preserves_input_password_empty_and_cancel() {
        let dir = TestDir::new("fuzzel-modern");
        let args_log = dir.path().join("args.log");
        let script = dir.write_script(
            "fake-fuzzel-modern.sh",
            &format!(
                "#!/bin/sh\n\
if [ \"$1\" = \"--help\" ]; then\n\
  echo '  --prompt-only=PROMPT  same as --prompt'\n\
  exit 0\n\
fi\n\
printf '%s\\n' \"$*\" >> '{}'\n\
for arg in \"$@\"; do\n\
  case \"$arg\" in\n\
    --placeholder=CANCEL) exit 1 ;;\n\
    --placeholder=EMPTY) printf '\\n'; exit 0 ;;\n\
  esac\n\
done\n\
printf 'my-secret\\n'\n",
                args_log.display()
            ),
        );

        let cfg = FuzzelConfig {
            binary: script.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let guard = acquire_guard();

        let ssid = input(
            &cfg,
            &guard,
            &FuzzelOptions {
                prompt: "Hidden SSID: ".to_string(),
                placeholder: Some("Network name".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(ssid.as_deref(), Some("my-secret"));

        let pw = password(
            &cfg,
            &guard,
            &FuzzelOptions {
                prompt: "Password: ".to_string(),
                placeholder: Some("EMPTY".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(pw.as_deref(), Some(""));

        let cancelled = password(
            &cfg,
            &guard,
            &FuzzelOptions {
                prompt: "Password: ".to_string(),
                placeholder: Some("CANCEL".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(cancelled, None);

        let logged = fs::read_to_string(&args_log).unwrap();
        let lines: Vec<&str> = logged.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("--prompt-only=Hidden SSID: "));
        assert!(!lines[0].contains("--lines=0"));
        assert!(!lines[0].contains("--password"));
        assert!(lines[1].contains("--prompt-only=Password: "));
        assert!(lines[1].contains("--password"));
        assert!(lines[2].contains("--prompt-only=Password: "));
        assert!(lines[2].contains("--password"));
    }

    #[tokio::test]
    async fn legacy_binary_uses_lines_zero_fallback() {
        let dir = TestDir::new("fuzzel-legacy");
        let args_log = dir.path().join("args.log");
        let script = dir.write_script(
            "fake-fuzzel-legacy.sh",
            &format!(
                "#!/bin/sh\n\
if [ \"$1\" = \"--help\" ]; then\n\
  echo '  -p,--prompt=PROMPT  prompt'\n\
  exit 0\n\
fi\n\
printf '%s\\n' \"$*\" >> '{}'\n\
printf 'legacy-ssid\\n'\n",
                args_log.display()
            ),
        );

        let cfg = FuzzelConfig {
            binary: script.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let guard = acquire_guard();

        let res = input(
            &cfg,
            &guard,
            &FuzzelOptions {
                prompt: "Hotspot SSID: ".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(res.as_deref(), Some("legacy-ssid"));

        let logged = fs::read_to_string(&args_log).unwrap();
        assert!(logged.contains("--prompt=Hotspot SSID: "));
        assert!(logged.contains("--lines=0"));
        assert!(!logged.contains("--prompt-only"));
    }

    #[tokio::test]
    async fn runtime_invalid_option_falls_back_and_updates_cache() {
        let dir = TestDir::new("fuzzel-fallback");
        let args_log = dir.path().join("args.log");
        let script = dir.write_script(
            "fake-fuzzel-fallback.sh",
            &format!(
                "#!/bin/sh\n\
if [ \"$1\" = \"--help\" ]; then\n\
  echo '  --prompt-only=PROMPT'\n\
  exit 0\n\
fi\n\
printf '%s\\n' \"$*\" >> '{}'\n\
for arg in \"$@\"; do\n\
  case \"$arg\" in\n\
    --prompt-only=*) echo \"error: $arg: invalid option\" >&2; exit 1 ;;\n\
  esac\n\
done\n\
printf 'recovered\\n'\n",
                args_log.display()
            ),
        );

        let cfg = FuzzelConfig {
            binary: script.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let guard = acquire_guard();

        let first = input(
            &cfg,
            &guard,
            &FuzzelOptions {
                prompt: "SSID: ".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(first.as_deref(), Some("recovered"));

        let second = input(
            &cfg,
            &guard,
            &FuzzelOptions {
                prompt: "SSID: ".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(second.as_deref(), Some("recovered"));

        let logged = fs::read_to_string(&args_log).unwrap();
        let lines: Vec<&str> = logged.lines().collect();
        // First call attempted --prompt-only and fell back; second call went straight to legacy.
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("--prompt-only=SSID: "));
        assert!(lines[1].contains("--prompt=SSID: "));
        assert!(lines[1].contains("--lines=0"));
        assert!(lines[2].contains("--prompt=SSID: "));
        assert!(lines[2].contains("--lines=0"));
    }
}
