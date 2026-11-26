use crate::config::FuzzelConfig;
use crate::daemon::menu::MenuGuard;
use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

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

fn build_base_args(cfg: &FuzzelConfig, opts: &FuzzelOptions) -> Vec<String> {
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
    if !opts.prompt.is_empty() {
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

async fn run_fuzzel_raw(
    cfg: &FuzzelConfig,
    guard: &MenuGuard,
    args: &[String],
    stdin_text: Option<&str>,
) -> Result<(i32, String)> {
    if !guard.is_current() {
        return Ok((-1, String::new()));
    }

    let mut cmd = Command::new(&cfg.binary);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn fuzzel binary '{}'", cfg.binary))?;

    let pid = child.id();
    if let Some(pid) = pid {
        if !guard.register_child_pid(pid) {
            let _ = child.kill().await;
            return Ok((-1, String::new()));
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
    Ok((code, stdout))
}

pub async fn select_raw(
    cfg: &FuzzelConfig,
    guard: &MenuGuard,
    lines: &[String],
    opts: &FuzzelOptions,
) -> Result<FuzzelResult> {
    let mut args = build_base_args(cfg, opts);
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
    let (exit_code, stdout) = run_fuzzel_raw(cfg, guard, &args, Some(&input)).await?;
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
    let mut args = build_base_args(cfg, opts);
    args.push("--lines=0".to_string());
    let (exit_code, stdout) = run_fuzzel_raw(cfg, guard, &args, Some("")).await?;
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
