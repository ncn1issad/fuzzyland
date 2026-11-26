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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pango_escape_handles_all_special_chars() {
        assert_eq!(
            pango_escape(r#"A & B <tag> "quoted" 'single'"#),
            "A &amp; B &lt;tag&gt; &quot;quoted&quot; &apos;single&apos;"
        );
    }

    #[test]
    fn tooltip_formatter_defaults_are_theme_agnostic() {
        let cfg = TooltipConfig::default();
        let fmt = TooltipFormatter::new(&cfg);

        assert_eq!(fmt.header("Battery & Power"), "<b>Battery &amp; Power</b>");
        assert_eq!(fmt.line("Mouse <1>"), "Mouse &lt;1&gt;");
        assert_eq!(
            fmt.subtext("Balanced"),
            "<span alpha='75%'>Balanced</span>"
        );
        assert_eq!(
            fmt.divider(),
            format!("<span alpha='30%'>{TOOLTIP_RULE}</span>")
        );
        assert_eq!(
            fmt.footer("Left: Power profile"),
            "<span alpha='55%'>Left: Power profile</span>"
        );
    }

    #[test]
    fn tooltip_formatter_applies_custom_colors_when_configured() {
        let cfg = TooltipConfig {
            text: "#cdd6f4".to_string(),
            subtext: "#a6adc8".to_string(),
            muted: "#7f849c".to_string(),
            separator: "#45475a".to_string(),
        };
        let fmt = TooltipFormatter::new(&cfg);

        assert_eq!(
            fmt.header("85%"),
            "<b><span color='#cdd6f4'>85%</span></b>"
        );
        assert_eq!(fmt.line("Item"), "<span color='#cdd6f4'>Item</span>");
        assert_eq!(fmt.subtext("Meta"), "<span color='#a6adc8'>Meta</span>");
        assert_eq!(
            fmt.divider(),
            format!("<span color='#45475a'>{TOOLTIP_RULE}</span>")
        );
        assert_eq!(fmt.footer("Hint"), "<span color='#7f849c'>Hint</span>");
    }

    #[test]
    fn waybar_output_omits_empty_optional_fields() {
        let empty = WaybarOutput::empty();
        assert_eq!(empty.to_json_line(), r#"{"text":""}"#);

        let full = WaybarOutput {
            text: "80%".to_string(),
            tooltip: Some("tip".to_string()),
            class: vec!["charging".to_string()],
            percentage: Some(80),
        };
        let parsed: serde_json::Value = serde_json::from_str(&full.to_json_line()).unwrap();
        assert_eq!(parsed["text"], "80%");
        assert_eq!(parsed["tooltip"], "tip");
        assert_eq!(parsed["class"], serde_json::json!(["charging"]));
        assert_eq!(parsed["percentage"], 80);
    }
}
