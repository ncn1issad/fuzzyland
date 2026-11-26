use crate::core::waybar::WaybarOutput;
use crate::daemon::AppContext;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::watch;

pub struct BatteryState {
    tx: watch::Sender<WaybarOutput>,
}

impl BatteryState {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(WaybarOutput::empty());
        Self { tx }
    }

    pub fn subscribe(&self) -> watch::Receiver<WaybarOutput> {
        self.tx.subscribe()
    }
}

impl Default for BatteryState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn spawn_background(_ctx: Arc<AppContext>) {}

pub async fn status_once(_ctx: &AppContext) -> Result<WaybarOutput> {
    Ok(WaybarOutput::empty())
}

pub async fn run_menu(_ctx: &AppContext) -> Result<()> {
    Ok(())
}

pub async fn set_profile(_ctx: &AppContext, _name: &str) -> Result<()> {
    Ok(())
}
