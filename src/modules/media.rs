use crate::daemon::AppContext;
use anyhow::Result;
use std::sync::Arc;

#[derive(Default)]
pub struct MediaState;

impl MediaState {
    pub fn new() -> Self {
        Self
    }
}

pub fn spawn_background(_ctx: Arc<AppContext>) {}

pub async fn run_action(_ctx: &AppContext, _action: &str) -> Result<Option<String>> {
    Ok(None)
}
