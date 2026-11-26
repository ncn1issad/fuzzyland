use crate::daemon::AppContext;
use anyhow::Result;
use std::sync::Arc;

#[derive(Default)]
pub struct BluetoothState;

impl BluetoothState {
    pub fn new() -> Self {
        Self
    }
}

pub fn spawn_background(_ctx: Arc<AppContext>) {}

pub async fn run_menu(_ctx: &AppContext) -> Result<()> {
    Ok(())
}

pub async fn toggle_power(_ctx: &AppContext) -> Result<()> {
    Ok(())
}
