use anyhow::Result;
use std::collections::HashMap;
use tokio::sync::OnceCell;
use tracing::warn;
use zbus::zvariant::Value;
use zbus::Connection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub app_name: String,
    pub summary: String,
    pub body: String,
    pub icon: Option<String>,
    pub urgency: Urgency,
    pub sync_id: Option<String>,
    pub replaces_id: u32,
    pub timeout_ms: i32,
}

impl Notification {
    pub fn new(app_name: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            app_name: app_name.into(),
            summary: summary.into(),
            body: String::new(),
            icon: None,
            urgency: Urgency::Low,
            sync_id: None,
            replaces_id: 0,
            timeout_ms: -1,
        }
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub fn urgency(mut self, urgency: Urgency) -> Self {
        self.urgency = urgency;
        self
    }

    pub fn sync_id(mut self, sync_id: impl Into<String>) -> Self {
        self.sync_id = Some(sync_id.into());
        self
    }

    #[allow(dead_code)]
    pub fn replaces_id(mut self, id: u32) -> Self {
        self.replaces_id = id;
        self
    }

    #[allow(dead_code)]
    pub fn timeout_ms(mut self, timeout_ms: i32) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }
}

pub struct Notifier {
    bus: OnceCell<Connection>,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            bus: OnceCell::new(),
        }
    }

    pub fn with_connection(conn: Connection) -> Self {
        let cell = OnceCell::new();
        let _ = cell.set(conn);
        Self { bus: cell }
    }

    async fn connection(&self) -> Result<&Connection> {
        self.bus
            .get_or_try_init(|| async { Ok(Connection::session().await?) })
            .await
    }

    pub async fn send(&self, notif: &Notification) -> Result<u32> {
        let conn = self.connection().await?;
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("urgency", Value::U8(notif.urgency as u8));
        if let Some(ref sync_id) = notif.sync_id {
            hints.insert(
                "x-canonical-private-synchronous",
                Value::from(sync_id.as_str()),
            );
        }

        let actions: Vec<&str> = Vec::new();
        let icon = notif.icon.as_deref().unwrap_or("");

        let reply = conn
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    notif.app_name.as_str(),
                    notif.replaces_id,
                    icon,
                    notif.summary.as_str(),
                    notif.body.as_str(),
                    actions,
                    hints,
                    notif.timeout_ms,
                ),
            )
            .await?;

        let id: u32 = reply.body().deserialize()?;
        Ok(id)
    }

    pub async fn send_best_effort(&self, notif: &Notification) {
        if let Err(err) = self.send(notif).await {
            warn!("Failed to send notification '{}': {err}", notif.summary);
        }
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_builder_sets_all_fields() {
        let notif = Notification::new("Fuzzyland", "Title")
            .body("Details")
            .icon("/tmp/preview.png")
            .urgency(Urgency::Critical)
            .sync_id("sync-key")
            .replaces_id(42)
            .timeout_ms(5000);

        assert_eq!(notif.app_name, "Fuzzyland");
        assert_eq!(notif.summary, "Title");
        assert_eq!(notif.body, "Details");
        assert_eq!(notif.icon.as_deref(), Some("/tmp/preview.png"));
        assert_eq!(notif.urgency, Urgency::Critical);
        assert_eq!(notif.sync_id.as_deref(), Some("sync-key"));
        assert_eq!(notif.replaces_id, 42);
        assert_eq!(notif.timeout_ms, 5000);
    }
}
