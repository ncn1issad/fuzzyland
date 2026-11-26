use crate::core::waybar::pango_escape;
use crate::daemon::AppContext;
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, warn};
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

const DBUS_SERVICE: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_IFACE: &str = "org.freedesktop.DBus";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

impl PlaybackStatus {
    fn from_mpris(s: &str) -> Self {
        match s {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            _ => Self::Stopped,
        }
    }

    fn is_active(self) -> bool {
        matches!(self, Self::Playing | Self::Paused)
    }

    fn priority(self) -> u8 {
        match self {
            Self::Playing => 2,
            Self::Paused => 1,
            Self::Stopped => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlayerInfo {
    bus_name: String,
    owner: Option<String>,
    status: PlaybackStatus,
    title: String,
    artist: String,
    last_active: u64,
}

#[derive(Default)]
struct MediaInner {
    initialized: bool,
    seq: u64,
    players: HashMap<String, PlayerInfo>,
    owner_to_names: HashMap<String, Vec<String>>,
}

impl MediaInner {
    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    fn set_owner_mapping(&mut self, bus_name: &str, old_owner: Option<&str>, new_owner: Option<&str>) {
        if let Some(old) = old_owner {
            if !old.is_empty() {
                if let Some(names) = self.owner_to_names.get_mut(old) {
                    names.retain(|n| n != bus_name);
                    if names.is_empty() {
                        self.owner_to_names.remove(old);
                    }
                }
            }
        }
        if let Some(new) = new_owner {
            if !new.is_empty() {
                let entry = self.owner_to_names.entry(new.to_string()).or_default();
                if !entry.iter().any(|n| n == bus_name) {
                    entry.push(bus_name.to_string());
                }
            }
        }
    }

    fn upsert_player(&mut self, mut info: PlayerInfo, bump_activity: bool) {
        let prev_owner = self
            .players
            .get(&info.bus_name)
            .and_then(|p| p.owner.clone());
        let prev_seq = self
            .players
            .get(&info.bus_name)
            .map(|p| p.last_active)
            .unwrap_or(0);

        if bump_activity {
            info.last_active = self.next_seq();
        } else if info.last_active == 0 {
            info.last_active = if prev_seq > 0 {
                prev_seq
            } else {
                self.next_seq()
            };
        }

        let new_owner = info.owner.clone();
        self.set_owner_mapping(
            &info.bus_name,
            prev_owner.as_deref(),
            new_owner.as_deref(),
        );
        self.players.insert(info.bus_name.clone(), info);
    }

    fn register_owner(&mut self, bus_name: &str, old_owner: &str, new_owner: &str) {
        self.set_owner_mapping(bus_name, Some(old_owner), Some(new_owner));
        if let Some(existing) = self.players.get_mut(bus_name) {
            existing.owner = Some(new_owner.to_string());
        } else {
            let seq = self.next_seq();
            self.players.insert(
                bus_name.to_string(),
                PlayerInfo {
                    bus_name: bus_name.to_string(),
                    owner: Some(new_owner.to_string()),
                    status: PlaybackStatus::Stopped,
                    title: String::new(),
                    artist: String::new(),
                    last_active: seq,
                },
            );
        }
    }

    fn remove_player(&mut self, bus_name: &str, old_owner: Option<&str>) {
        let stored_owner = self
            .players
            .remove(bus_name)
            .and_then(|p| p.owner);
        self.set_owner_mapping(bus_name, old_owner, None);
        if let Some(owner) = stored_owner {
            self.set_owner_mapping(bus_name, Some(owner.as_str()), None);
        }
    }

    fn apply_property_changes(
        &mut self,
        sender: &str,
        changed: &HashMap<String, OwnedValue>,
    ) -> Option<Vec<String>> {
        let names = self.owner_to_names.get(sender)?.clone();
        let has_status = changed.contains_key("PlaybackStatus");
        let has_metadata = changed.contains_key("Metadata");

        if !has_status && !has_metadata {
            return Some(names);
        }

        let parsed_status = changed
            .get("PlaybackStatus")
            .and_then(|v| value_as_str(v))
            .map(PlaybackStatus::from_mpris);
        let parsed_metadata = changed.get("Metadata").map(|v| extract_metadata(v));

        for name in &names {
            let seq = self.next_seq();
            if let Some(player) = self.players.get_mut(name) {
                let mut touched = false;
                if let Some(status) = parsed_status {
                    if player.status != status {
                        player.status = status;
                        touched = true;
                    } else if status.is_active() {
                        touched = true;
                    }
                }
                if let Some((ref title, ref artist)) = parsed_metadata {
                    player.title = title.clone();
                    player.artist = artist.clone();
                    touched = true;
                }
                if touched {
                    player.last_active = seq;
                }
            }
        }

        Some(names)
    }

    fn touch_player(&mut self, bus_name: &str) {
        let seq = self.next_seq();
        if let Some(player) = self.players.get_mut(bus_name) {
            player.last_active = seq;
        }
    }

    fn select_best(&self) -> Option<PlayerInfo> {
        select_active_player(self.players.values()).cloned()
    }
}

pub struct MediaState {
    inner: RwLock<MediaInner>,
}

impl Default for MediaState {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaState {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(MediaInner::default()),
        }
    }

    fn is_initialized(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .initialized
    }

    fn set_initialized(&self, initialized: bool) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .initialized = initialized;
    }

    fn select_best(&self) -> Option<PlayerInfo> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .select_best()
    }

    fn replace_all(&self, players: Vec<PlayerInfo>) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let old_seqs: HashMap<String, u64> = guard
            .players
            .iter()
            .map(|(k, v)| (k.clone(), v.last_active))
            .collect();
        guard.players.clear();
        guard.owner_to_names.clear();
        for mut player in players {
            if let Some(&seq) = old_seqs.get(&player.bus_name) {
                player.last_active = seq;
                guard.upsert_player(player, false);
            } else {
                guard.upsert_player(player, true);
            }
        }
        guard.initialized = true;
    }

    fn upsert(&self, info: PlayerInfo, bump_activity: bool) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .upsert_player(info, bump_activity);
    }

    fn register_owner(&self, bus_name: &str, old_owner: &str, new_owner: &str) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .register_owner(bus_name, old_owner, new_owner);
    }

    fn remove(&self, bus_name: &str, old_owner: Option<&str>) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove_player(bus_name, old_owner);
    }

    fn apply_property_changes(
        &self,
        sender: &str,
        changed: &HashMap<String, OwnedValue>,
    ) -> Option<Vec<String>> {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .apply_property_changes(sender, changed)
    }

    fn touch(&self, bus_name: &str) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .touch_player(bus_name);
    }
}

pub fn spawn_background(ctx: Arc<AppContext>) {
    tokio::spawn(async move {
        if let Err(err) = run_watcher(Arc::clone(&ctx)).await {
            ctx.media.set_initialized(false);
            warn!("MPRIS background watcher stopped: {err}");
        }
    });
}

async fn run_watcher(ctx: Arc<AppContext>) -> Result<()> {
    let conn = ctx.session_bus().await?;

    // Subscribe to signals before the initial scan so no transitions are missed.
    let name_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(DBUS_SERVICE)?
        .interface(DBUS_IFACE)?
        .member("NameOwnerChanged")?
        .build();
    let mut name_stream = MessageStream::for_match_rule(name_rule, conn, None).await?;

    let props_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(PROPS_IFACE)?
        .member("PropertiesChanged")?
        .path(MPRIS_PATH)?
        .arg(0, MPRIS_PLAYER_IFACE)?
        .build();
    let mut props_stream = MessageStream::for_match_rule(props_rule, conn, None).await?;

    // Populate initial player table.
    let initial = discover_players(conn).await?;
    ctx.media.replace_all(initial);

    loop {
        tokio::select! {
            maybe_msg = name_stream.next() => {
                let Some(msg_res) = maybe_msg else {
                    break;
                };
                match msg_res {
                    Ok(msg) => {
                        if let Ok((name, old_owner, new_owner)) =
                            msg.body().deserialize::<(String, String, String)>()
                        {
                            if !name.starts_with(MPRIS_PREFIX) {
                                continue;
                            }
                            handle_name_owner_changed(
                                conn,
                                &ctx.media,
                                &name,
                                &old_owner,
                                &new_owner,
                            )
                            .await;
                        }
                    }
                    Err(err) => {
                        debug!("Error receiving NameOwnerChanged signal: {err}");
                    }
                }
            }
            maybe_msg = props_stream.next() => {
                let Some(msg_res) = maybe_msg else {
                    break;
                };
                match msg_res {
                    Ok(msg) => {
                        let sender = msg
                            .header()
                            .sender()
                            .map(|s| s.as_str().to_owned());
                        if let Ok((iface, changed, invalidated)) = msg
                            .body()
                            .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                        {
                            if iface != MPRIS_PLAYER_IFACE {
                                continue;
                            }
                            if let Some(sender) = sender {
                                handle_properties_changed(
                                    conn,
                                    &ctx.media,
                                    &sender,
                                    &changed,
                                    &invalidated,
                                )
                                .await;
                            }
                        }
                    }
                    Err(err) => {
                        debug!("Error receiving PropertiesChanged signal: {err}");
                    }
                }
            }
        }
    }

    ctx.media.set_initialized(false);
    Ok(())
}

async fn handle_name_owner_changed(
    conn: &Connection,
    state: &MediaState,
    name: &str,
    old_owner: &str,
    new_owner: &str,
) {
    if new_owner.is_empty() {
        state.remove(name, Some(old_owner));
        return;
    }

    // Register owner mapping immediately so concurrent PropertiesChanged signals match.
    state.register_owner(name, old_owner, new_owner);

    match fetch_player(conn, name, Some(new_owner.to_string())).await {
        Ok(info) => {
            state.upsert(info, true);
        }
        Err(err) => {
            debug!("Initial MPRIS query for {name} failed (will await signals): {err}");
        }
    }
}

async fn handle_properties_changed(
    conn: &Connection,
    state: &MediaState,
    sender: &str,
    changed: &HashMap<String, OwnedValue>,
    invalidated: &[String],
) {
    let maybe_names = state.apply_property_changes(sender, changed);

    let names = match maybe_names {
        Some(names) => names,
        None => {
            // Unknown sender (race before NameOwnerChanged); resync once.
            if let Ok(players) = discover_players(conn).await {
                state.replace_all(players);
            }
            return;
        }
    };

    let needs_refetch = invalidated
        .iter()
        .any(|k| k == "PlaybackStatus" || k == "Metadata");
    if needs_refetch {
        for name in names {
            if let Ok(info) = fetch_player(conn, &name, Some(sender.to_string())).await {
                state.upsert(info, true);
            }
        }
    }
}

pub async fn run_action(ctx: &AppContext, action: &str) -> Result<Option<String>> {
    match action {
        "track" | "previous" | "toggle" | "next" => {
            let player = resolve_active_player(ctx).await?;
            let Some(player) = player else {
                return Ok(None);
            };
            Ok(format_display_action(ctx, action, &player))
        }
        "play-pause" | "prev" | "skip" => {
            let method = match action {
                "play-pause" => "PlayPause",
                "prev" => "Previous",
                "skip" => "Next",
                _ => unreachable!(),
            };
            let Some(player) = resolve_active_player(ctx).await? else {
                return Ok(None);
            };
            let conn = ctx.session_bus().await?;
            if let Err(err) = call_player_method(conn, &player.bus_name, method).await {
                if ctx.media.is_initialized() {
                    debug!(
                        "MPRIS {method} on cached player {} failed ({err}); retrying via live discovery",
                        player.bus_name
                    );
                    ctx.media.remove(&player.bus_name, player.owner.as_deref());
                    let players = discover_players(conn).await?;
                    let fallback = select_active_player(players.iter()).cloned();
                    ctx.media.replace_all(players);
                    if let Some(fb) = fallback {
                        call_player_method(conn, &fb.bus_name, method).await?;
                        ctx.media.touch(&fb.bus_name);
                    }
                } else {
                    return Err(err);
                }
            } else if ctx.media.is_initialized() {
                ctx.media.touch(&player.bus_name);
            }
            Ok(None)
        }
        _ => Err(anyhow!("Unknown media action: {action}")),
    }
}

async fn resolve_active_player(ctx: &AppContext) -> Result<Option<PlayerInfo>> {
    if ctx.media.is_initialized() {
        return Ok(ctx.media.select_best());
    }
    let conn = ctx.session_bus().await?;
    let players = discover_players(conn).await?;
    Ok(select_active_player(players.iter()).cloned())
}

fn format_display_action(ctx: &AppContext, action: &str, player: &PlayerInfo) -> Option<String> {
    if !player.status.is_active() {
        return None;
    }
    match action {
        "track" => format_track(&player.title, &player.artist, ctx.config.media.max_track_len),
        "previous" => Some(ctx.config.icons.media_prev.clone()),
        "toggle" => match player.status {
            PlaybackStatus::Playing => Some(ctx.config.icons.media_pause.clone()),
            PlaybackStatus::Paused => Some(ctx.config.icons.media_play.clone()),
            PlaybackStatus::Stopped => None,
        },
        "next" => Some(ctx.config.icons.media_next.clone()),
        _ => None,
    }
}

async fn call_player_method(conn: &Connection, bus_name: &str, method: &str) -> Result<()> {
    conn.call_method(
        Some(bus_name),
        MPRIS_PATH,
        Some(MPRIS_PLAYER_IFACE),
        method,
        &(),
    )
    .await?;
    Ok(())
}

async fn discover_players(conn: &Connection) -> Result<Vec<PlayerInfo>> {
    let reply = conn
        .call_method(
            Some(DBUS_SERVICE),
            DBUS_PATH,
            Some(DBUS_IFACE),
            "ListNames",
            &(),
        )
        .await?;
    let names: Vec<String> = reply.body().deserialize()?;

    let mut mpris_names: Vec<String> = names
        .into_iter()
        .filter(|n| n.starts_with(MPRIS_PREFIX))
        .collect();
    mpris_names.sort();

    let mut players = Vec::with_capacity(mpris_names.len());
    for name in mpris_names {
        let owner = get_name_owner(conn, &name).await.ok();
        match fetch_player(conn, &name, owner).await {
            Ok(info) => players.push(info),
            Err(err) => {
                debug!("Skipping unreadable MPRIS player {name}: {err}");
            }
        }
    }

    Ok(players)
}

async fn get_name_owner(conn: &Connection, bus_name: &str) -> Result<String> {
    let reply = conn
        .call_method(
            Some(DBUS_SERVICE),
            DBUS_PATH,
            Some(DBUS_IFACE),
            "GetNameOwner",
            &(bus_name,),
        )
        .await?;
    let owner: String = reply.body().deserialize()?;
    Ok(owner)
}

async fn fetch_player(
    conn: &Connection,
    bus_name: &str,
    owner: Option<String>,
) -> Result<PlayerInfo> {
    match conn
        .call_method(
            Some(bus_name),
            MPRIS_PATH,
            Some(PROPS_IFACE),
            "GetAll",
            &(MPRIS_PLAYER_IFACE,),
        )
        .await
    {
        Ok(reply) => {
            let props: HashMap<String, OwnedValue> = reply.body().deserialize()?;
            let status = props
                .get("PlaybackStatus")
                .and_then(|v| value_as_str(v))
                .map(PlaybackStatus::from_mpris)
                .unwrap_or(PlaybackStatus::Stopped);
            let (title, artist) = props
                .get("Metadata")
                .map(|v| extract_metadata(v))
                .unwrap_or_default();
            Ok(PlayerInfo {
                bus_name: bus_name.to_string(),
                owner,
                status,
                title,
                artist,
                last_active: 0,
            })
        }
        Err(_) => {
            // Fallback for players that fail GetAll but support individual Get calls.
            let status_val = get_property(conn, bus_name, "PlaybackStatus").await?;
            let status = value_as_str(&status_val)
                .map(PlaybackStatus::from_mpris)
                .unwrap_or(PlaybackStatus::Stopped);
            let (title, artist) = match get_property(conn, bus_name, "Metadata").await {
                Ok(meta_val) => extract_metadata(&meta_val),
                Err(_) => (String::new(), String::new()),
            };
            Ok(PlayerInfo {
                bus_name: bus_name.to_string(),
                owner,
                status,
                title,
                artist,
                last_active: 0,
            })
        }
    }
}

async fn get_property(conn: &Connection, bus_name: &str, prop: &str) -> Result<OwnedValue> {
    let reply = conn
        .call_method(
            Some(bus_name),
            MPRIS_PATH,
            Some(PROPS_IFACE),
            "Get",
            &(MPRIS_PLAYER_IFACE, prop),
        )
        .await?;
    let val: OwnedValue = reply.body().deserialize()?;
    Ok(val)
}

fn unwrap_value<'a>(mut val: &'a Value<'a>) -> &'a Value<'a> {
    while let Value::Value(inner) = val {
        val = inner.as_ref();
    }
    val
}

fn value_as_str<'a>(val: &'a Value<'a>) -> Option<&'a str> {
    match unwrap_value(val) {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn extract_metadata(val: &Value<'_>) -> (String, String) {
    let Value::Dict(dict) = unwrap_value(val) else {
        return (String::new(), String::new());
    };

    let mut title = String::new();
    let mut artist = String::new();

    for (k, v) in dict.iter() {
        let Some(key) = value_as_str(k) else {
            continue;
        };
        match key {
            "xesam:title" => {
                if let Some(s) = value_as_str(v) {
                    title = s.to_string();
                }
            }
            "xesam:artist" => {
                artist = extract_artist(v);
            }
            _ => {}
        }
    }

    (title, artist)
}

fn extract_artist(val: &Value<'_>) -> String {
    match unwrap_value(val) {
        Value::Str(s) => s.as_str().to_string(),
        Value::Array(arr) => {
            let parts: Vec<&str> = arr
                .iter()
                .filter_map(value_as_str)
                .filter(|s| !s.is_empty())
                .collect();
            parts.join(", ")
        }
        _ => String::new(),
    }
}

fn select_active_player<'a>(
    players: impl IntoIterator<Item = &'a PlayerInfo>,
) -> Option<&'a PlayerInfo> {
    players
        .into_iter()
        .filter(|p| p.status.is_active())
        .max_by(|a, b| {
            a.status
                .priority()
                .cmp(&b.status.priority())
                .then_with(|| a.last_active.cmp(&b.last_active))
                .then_with(|| b.bus_name.cmp(&a.bus_name))
        })
}

fn format_track(title: &str, artist: &str, max_len: usize) -> Option<String> {
    if max_len == 0 {
        return None;
    }

    let clean_title = title.replace(['\n', '\r'], " ");
    let clean_title = clean_title.trim();
    let clean_artist = artist.replace(['\n', '\r'], " ");
    let clean_artist = clean_artist.trim();

    let cleaned = match (clean_title.is_empty(), clean_artist.is_empty()) {
        (true, true) => return None,
        (false, true) => clean_title.to_string(),
        (true, false) => clean_artist.to_string(),
        (false, false) => format!("{clean_title} — {clean_artist}"),
    };

    let truncated = if cleaned.chars().count() > max_len {
        let mut s: String = cleaned.chars().take(max_len - 1).collect();
        s.push('…');
        s
    } else {
        cleaned
    };

    Some(pango_escape(&truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use zbus::zvariant::{Array, Dict, Type};

    fn make_player(name: &str, status: PlaybackStatus, last_active: u64) -> PlayerInfo {
        PlayerInfo {
            bus_name: name.to_string(),
            owner: None,
            status,
            title: String::new(),
            artist: String::new(),
            last_active,
        }
    }

    #[test]
    fn format_track_title_and_artist() {
        assert_eq!(
            format_track("Bohemian Rhapsody", "Queen", 44).as_deref(),
            Some("Bohemian Rhapsody — Queen")
        );
    }

    #[test]
    fn format_track_title_only_and_artist_only() {
        assert_eq!(format_track("Solo Track", "", 44).as_deref(), Some("Solo Track"));
        assert_eq!(format_track("", "Only Artist", 44).as_deref(), Some("Only Artist"));
        assert_eq!(format_track("", "", 44), None);
        assert_eq!(format_track("\n\r", "", 44), None);
    }

    #[test]
    fn format_track_replaces_newlines_and_carriage_returns() {
        assert_eq!(
            format_track("Line 1\nLine 2", "Artist\rName", 44).as_deref(),
            Some("Line 1 Line 2 — Artist Name")
        );
    }

    #[test]
    fn format_track_utf8_char_safe_truncation() {
        // Exact boundary (44 chars) is not truncated.
        let exact = "a".repeat(44);
        assert_eq!(format_track(&exact, "", 44).as_deref(), Some(exact.as_str()));

        // 45 chars truncates to 43 chars + ellipsis = 44 chars.
        let over = "a".repeat(45);
        let expected = format!("{}…", "a".repeat(43));
        assert_eq!(format_track(&over, "", 44).as_deref(), Some(expected.as_str()));

        // Multi-byte UTF-8 characters (Japanese + emoji) must never split mid-codepoint.
        let multibyte = "東京タワー🎵の夜明け"; // 10 chars
        let truncated = format_track(multibyte, "", 6).unwrap();
        assert_eq!(truncated, "東京タワー…");
        assert_eq!(truncated.chars().count(), 6);

        // Edge cases for very small max_len.
        assert_eq!(format_track("abcdef", "", 1).as_deref(), Some("…"));
        assert_eq!(format_track("abcdef", "", 0), None);
    }

    #[test]
    fn format_track_pango_escapes_after_truncation() {
        let formatted = format_track("Rock & Roll <Live>", "A > B \"C\" 'D'", 44).unwrap();
        assert_eq!(
            formatted,
            "Rock &amp; Roll &lt;Live&gt; — A &gt; B &quot;C&quot; &apos;D&apos;"
        );

        // Truncating before escaping guarantees entities aren't sliced mid-escape.
        let short = format_track("A & B", "", 4).unwrap();
        assert_eq!(short, "A &amp;…");
    }

    #[test]
    fn select_player_prefers_playing_over_paused() {
        let players = [
            make_player("org.mpris.MediaPlayer2.spotify", PlaybackStatus::Paused, 100),
            make_player("org.mpris.MediaPlayer2.mpv", PlaybackStatus::Playing, 1),
        ];
        let selected = select_active_player(players.iter()).unwrap();
        assert_eq!(selected.bus_name, "org.mpris.MediaPlayer2.mpv");
    }

    #[test]
    fn select_player_breaks_ties_by_recency() {
        let playing = [
            make_player("org.mpris.MediaPlayer2.a", PlaybackStatus::Playing, 5),
            make_player("org.mpris.MediaPlayer2.b", PlaybackStatus::Playing, 12),
            make_player("org.mpris.MediaPlayer2.c", PlaybackStatus::Playing, 9),
        ];
        assert_eq!(
            select_active_player(playing.iter()).unwrap().bus_name,
            "org.mpris.MediaPlayer2.b"
        );

        let paused = [
            make_player("org.mpris.MediaPlayer2.x", PlaybackStatus::Paused, 20),
            make_player("org.mpris.MediaPlayer2.y", PlaybackStatus::Paused, 30),
        ];
        assert_eq!(
            select_active_player(paused.iter()).unwrap().bus_name,
            "org.mpris.MediaPlayer2.y"
        );
    }

    #[test]
    fn select_player_ignores_stopped_and_unknown() {
        assert_eq!(PlaybackStatus::from_mpris("Stopped"), PlaybackStatus::Stopped);
        assert_eq!(PlaybackStatus::from_mpris("Invalid"), PlaybackStatus::Stopped);

        let players = [
            make_player("org.mpris.MediaPlayer2.a", PlaybackStatus::Stopped, 50),
            make_player("org.mpris.MediaPlayer2.b", PlaybackStatus::Stopped, 99),
        ];
        assert!(select_active_player(players.iter()).is_none());
    }

    #[test]
    fn media_state_tracks_recency_via_property_updates() {
        let state = MediaState::new();
        state.upsert(
            PlayerInfo {
                bus_name: "org.mpris.MediaPlayer2.first".to_string(),
                owner: Some(":1.10".to_string()),
                status: PlaybackStatus::Paused,
                title: "First".to_string(),
                artist: "A".to_string(),
                last_active: 0,
            },
            true,
        );
        state.upsert(
            PlayerInfo {
                bus_name: "org.mpris.MediaPlayer2.second".to_string(),
                owner: Some(":1.20".to_string()),
                status: PlaybackStatus::Paused,
                title: "Second".to_string(),
                artist: "B".to_string(),
                last_active: 0,
            },
            true,
        );

        // Second was inserted more recently, so it wins while both are Paused.
        assert_eq!(
            state.select_best().unwrap().bus_name,
            "org.mpris.MediaPlayer2.second"
        );

        // A PropertiesChanged update for :1.10 (first) bumps its recency above second.
        let mut changed: HashMap<String, OwnedValue> = HashMap::new();
        changed.insert(
            "PlaybackStatus".to_string(),
            OwnedValue::from(zbus::zvariant::Str::from("Paused")),
        );
        state.apply_property_changes(":1.10", &changed);
        assert_eq!(
            state.select_best().unwrap().bus_name,
            "org.mpris.MediaPlayer2.first"
        );

        // Removing first falls back to second.
        state.remove("org.mpris.MediaPlayer2.first", Some(":1.10"));
        assert_eq!(
            state.select_best().unwrap().bus_name,
            "org.mpris.MediaPlayer2.second"
        );
    }

    #[test]
    fn extract_metadata_supports_artist_array_and_string() {
        let mut dict = Dict::new(<&str>::SIGNATURE, <Value<'_>>::SIGNATURE);
        dict.append(
            Value::from("xesam:title"),
            Value::Value(Box::new(Value::from("My Song"))),
        )
        .unwrap();
        let mut artists = Array::new(<&str>::SIGNATURE);
        artists.append(Value::from("Alice")).unwrap();
        artists.append(Value::from("Bob")).unwrap();
        dict.append(
            Value::from("xesam:artist"),
            Value::Value(Box::new(Value::Array(artists))),
        )
        .unwrap();

        let (title, artist) = extract_metadata(&Value::Dict(dict));
        assert_eq!(title, "My Song");
        assert_eq!(artist, "Alice, Bob");

        // Some players emit xesam:artist as a plain string.
        let mut dict_str = Dict::new(<&str>::SIGNATURE, <Value<'_>>::SIGNATURE);
        dict_str
            .append(
                Value::from("xesam:artist"),
                Value::Value(Box::new(Value::from("Solo String Artist"))),
            )
            .unwrap();
        let (title2, artist2) = extract_metadata(&Value::Dict(dict_str));
        assert_eq!(title2, "");
        assert_eq!(artist2, "Solo String Artist");
    }

    #[test]
    fn display_actions_match_playback_status_and_icons() {
        let ctx = AppContext::new_standalone(Config::default());

        let playing = PlayerInfo {
            bus_name: "org.mpris.MediaPlayer2.mpv".to_string(),
            owner: None,
            status: PlaybackStatus::Playing,
            title: "Song".to_string(),
            artist: "Band".to_string(),
            last_active: 1,
        };
        assert_eq!(
            format_display_action(&ctx, "track", &playing).as_deref(),
            Some("Song — Band")
        );
        assert_eq!(
            format_display_action(&ctx, "previous", &playing).as_deref(),
            Some(ctx.config.icons.media_prev.as_str())
        );
        assert_eq!(
            format_display_action(&ctx, "toggle", &playing).as_deref(),
            Some(ctx.config.icons.media_pause.as_str())
        );
        assert_eq!(
            format_display_action(&ctx, "next", &playing).as_deref(),
            Some(ctx.config.icons.media_next.as_str())
        );

        let paused = PlayerInfo {
            status: PlaybackStatus::Paused,
            ..playing.clone()
        };
        assert_eq!(
            format_display_action(&ctx, "toggle", &paused).as_deref(),
            Some(ctx.config.icons.media_play.as_str())
        );

        let stopped = PlayerInfo {
            status: PlaybackStatus::Stopped,
            ..playing
        };
        assert_eq!(format_display_action(&ctx, "track", &stopped), None);
        assert_eq!(format_display_action(&ctx, "toggle", &stopped), None);
        assert_eq!(format_display_action(&ctx, "previous", &stopped), None);
        assert_eq!(format_display_action(&ctx, "next", &stopped), None);
    }
}
