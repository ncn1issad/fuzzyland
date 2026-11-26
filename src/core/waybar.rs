use crate::config::TooltipConfig;
use serde::{Deserialize, Serialize};

pub const TOOLTIP_RULE: &str = "────────────────────";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaybarOutput {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub class: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u8>,
}

impl WaybarOutput {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"text\":\"\"}".to_string())
    }
}

pub fn pango_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

pub struct TooltipFormatter<'a> {
    cfg: &'a TooltipConfig,
}

impl<'a> TooltipFormatter<'a> {
    pub fn new(cfg: &'a TooltipConfig) -> Self {
        Self { cfg }
    }

    pub fn header(&self, text: &str) -> String {
        let escaped = pango_escape(text);
        if self.cfg.text.is_empty() {
            format!("<b>{escaped}</b>")
        } else {
            format!(
                "<b><span color='{}'>{escaped}</span></b>",
                pango_escape(&self.cfg.text)
            )
        }
    }

    pub fn line(&self, text: &str) -> String {
        let escaped = pango_escape(text);
        if self.cfg.text.is_empty() {
            escaped
        } else {
            format!(
                "<span color='{}'>{escaped}</span>",
                pango_escape(&self.cfg.text)
            )
        }
    }

    pub fn subtext(&self, text: &str) -> String {
        let escaped = pango_escape(text);
        if self.cfg.subtext.is_empty() {
            format!("<span alpha='75%'>{escaped}</span>")
        } else {
            format!(
                "<span color='{}'>{escaped}</span>",
                pango_escape(&self.cfg.subtext)
            )
        }
    }

    pub fn divider(&self) -> String {
        if self.cfg.separator.is_empty() {
            format!("<span alpha='30%'>{TOOLTIP_RULE}</span>")
        } else {
            format!(
                "<span color='{}'>{TOOLTIP_RULE}</span>",
                pango_escape(&self.cfg.separator)
            )
        }
    }

    pub fn footer(&self, text: &str) -> String {
        let escaped = pango_escape(text);
        if self.cfg.muted.is_empty() {
            format!("<span alpha='55%'>{escaped}</span>")
        } else {
            format!(
                "<span color='{}'>{escaped}</span>",
                pango_escape(&self.cfg.muted)
            )
        }
    }
}
