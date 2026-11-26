use crate::core::waybar::WaybarOutput;
use crate::daemon::AppContext;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::watch;

pub struct BacklightState {
    tx: watch::Sender<WaybarOutput>,
}

impl BacklightState {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(WaybarOutput::empty());
        Self { tx }
    }

    pub fn subscribe(&self) -> watch::Receiver<WaybarOutput> {
        self.tx.subscribe()
    }
}

impl Default for BacklightState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn spawn_background(_ctx: Arc<AppContext>) {}

pub async fn status_once(_ctx: &AppContext) -> Result<WaybarOutput> {
    Ok(WaybarOutput::empty())
}

pub async fn adjust(_ctx: &AppContext, _delta: i32) -> Result<()> {
    Ok(())
}

pub async fn refresh(_ctx: &AppContext) -> Result<()> {
    Ok(())
}
