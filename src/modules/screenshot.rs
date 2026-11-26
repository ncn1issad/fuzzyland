use crate::daemon::AppContext;
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ScreenshotArgs {
    pub mode: String,
    pub save: bool,
    pub dir: Option<PathBuf>,
    pub delay: f64,
}

pub async fn run(_ctx: &AppContext, _args: ScreenshotArgs) -> Result<()> {
    Ok(())
}
