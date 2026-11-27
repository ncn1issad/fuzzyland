use crate::config::{AudioConfig, IconsConfig};
use crate::core::fuzzel::{self, Anchor, FuzzelOptions};
use crate::core::hyprland;
use crate::core::notify::{Notification, Urgency};
use crate::core::process::spawn_reaped;
use crate::daemon::menu::{MenuKind, MenuSession};
use crate::daemon::AppContext;
use anyhow::{anyhow, Context as _, Result};
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use libpulse_binding::def::PortAvailable;
use libpulse_binding::mainloop::standard::Mainloop;
use libpulse_binding::operation::{Operation, State as OperationState};
use libpulse_binding::time::MicroSeconds;
use libpulse_binding::volume::{ChannelVolumes, Volume};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, warn};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const EASYEFFECTS_SINK_NAME: &str = "easyeffects_sink";
const EASYEFFECTS_SOURCE_NAME: &str = "easyeffects_source";
const EASYEFFECTS_UNIT: &str = "easyeffects.service";
const SEPARATOR: &str = "──────────────────────────────────";

const PULSE_TIMEOUT: Duration = Duration::from_secs(3);
const PULSE_POLL_SLICE_US: u64 = 200_000;
const EE_QUERY_TIMEOUT: Duration = Duration::from_millis(2500);
const EE_ACTION_TIMEOUT: Duration = Duration::from_secs(3);

const SYNC_OUTPUT: &str = "waybar-audio-output";
const SYNC_INPUT: &str = "waybar-audio-input";
const SYNC_EQ: &str = "waybar-audio-eq";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortAvailability {
    Unknown,
    No,
    Yes,
}

impl From<PortAvailable> for PortAvailability {
    fn from(value: PortAvailable) -> Self {
        match value {
            PortAvailable::No => Self::No,
            PortAvailable::Yes => Self::Yes,
            PortAvailable::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PortSnapshot {
    name: String,
    available: PortAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SinkData {
    index: u32,
    name: String,
    description: Option<String>,
    node_name: Option<String>,
    node_description: Option<String>,
    device_description: Option<String>,
    device_bus: Option<String>,
    device_icon_name: Option<String>,
    device_form_factor: Option<String>,
    active_port: Option<String>,
    ports: Vec<PortSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceData {
    index: u32,
    name: String,
    description: Option<String>,
    monitor_of_sink: Option<u32>,
    device_class: Option<String>,
    node_name: Option<String>,
    node_description: Option<String>,
    device_description: Option<String>,
    device_bus: Option<String>,
    device_icon_name: Option<String>,
    device_form_factor: Option<String>,
    active_port: Option<String>,
    ports: Vec<PortSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SinkInputData {
    index: u32,
    sink: u32,
    app_id: Option<String>,
    node_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceOutputData {
    index: u32,
    source: u32,
    app_id: Option<String>,
    node_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioTarget {
    Output,
    Input,
}

#[derive(Debug, Clone, Copy)]
struct DeviceAudioState {
    index: u32,
    volume: ChannelVolumes,
    mute: bool,
}

/// Synchronous PulseAudio / PipeWire-Pulse client meant to run inside `spawn_blocking`.
///
/// Field declaration order matters: `context` must drop before `mainloop`.
struct PulseClient {
    context: Context,
    mainloop: Mainloop,
}

impl Drop for PulseClient {
    fn drop(&mut self) {
        self.context.disconnect();
    }
}

impl PulseClient {
    fn connect() -> Result<Self> {
        let mut mainloop =
            Mainloop::new().ok_or_else(|| anyhow!("Failed to create PulseAudio mainloop"))?;
        let mut context = Context::new(&mainloop, "fuzzyland")
            .ok_or_else(|| anyhow!("Failed to create PulseAudio context"))?;

        context
            .connect(None, ContextFlagSet::NOAUTOSPAWN, None)
            .map_err(|e| anyhow!("Failed to connect to PulseAudio/PipeWire: {e}"))?;

        let deadline = Instant::now() + PULSE_TIMEOUT;
        loop {
            match context.get_state() {
                ContextState::Ready => break,
                ContextState::Failed | ContextState::Terminated => {
                    context.disconnect();
                    return Err(anyhow!(
                        "PulseAudio/PipeWire connection failed (state: {:?})",
                        context.get_state()
                    ));
                }
                _ => {}
            }
            step_mainloop_until(&mut mainloop, deadline)?;
        }

        Ok(Self { context, mainloop })
    }

    fn wait_op<T: ?Sized>(&mut self, mut op: Operation<T>) -> Result<()> {
        let deadline = Instant::now() + PULSE_TIMEOUT;
        while op.get_state() == OperationState::Running {
            match self.context.get_state() {
                ContextState::Failed | ContextState::Terminated => {
                    op.cancel();
                    return Err(anyhow!("PulseAudio/PipeWire context disconnected"));
                }
                _ => {}
            }
            if let Err(err) = step_mainloop_until(&mut self.mainloop, deadline) {
                op.cancel();
                return Err(err);
            }
        }
        if op.get_state() == OperationState::Cancelled {
            return Err(anyhow!("PulseAudio/PipeWire operation cancelled"));
        }
        Ok(())
    }

    fn default_sink_and_source(&mut self) -> Result<(String, String)> {
        let defaults = Rc::new(RefCell::new((String::new(), String::new())));
        let defaults_cb = Rc::clone(&defaults);
        let introspector = self.context.introspect();
        let op = introspector.get_server_info(move |info| {
            let mut slot = defaults_cb.borrow_mut();
            if let Some(sink) = info.default_sink_name.as_deref() {
                slot.0 = sink.to_string();
            }
            if let Some(source) = info.default_source_name.as_deref() {
                slot.1 = source.to_string();
            }
        });
        self.wait_op(op)?;
        let res = defaults.borrow().clone();
        Ok(res)
    }

    fn list_sinks(&mut self) -> Result<Vec<SinkData>> {
        let sinks = Rc::new(RefCell::new(Vec::new()));
        let sinks_cb = Rc::clone(&sinks);
        let introspector = self.context.introspect();
        let op = introspector.get_sink_info_list(move |res| {
            if let ListResult::Item(info) = res {
                let name = info.name.as_deref().unwrap_or_default().to_string();
                let ports = info
                    .ports
                    .iter()
                    .map(|p| PortSnapshot {
                        name: p.name.as_deref().unwrap_or_default().to_string(),
                        available: PortAvailability::from(p.available),
                    })
                    .collect();
                sinks_cb.borrow_mut().push(SinkData {
                    index: info.index,
                    name,
                    description: info.description.as_deref().map(str::to_string),
                    node_name: info.proplist.get_str("node.name"),
                    node_description: info.proplist.get_str("node.description"),
                    device_description: info.proplist.get_str("device.description"),
                    device_bus: info.proplist.get_str("device.bus"),
                    device_icon_name: info.proplist.get_str("device.icon_name"),
                    device_form_factor: info.proplist.get_str("device.form_factor"),
                    active_port: info
                        .active_port
                        .as_ref()
                        .and_then(|p| p.name.as_deref())
                        .map(str::to_string),
                    ports,
                });
            }
        });
        self.wait_op(op)?;
        let out = sinks.borrow().clone();
        Ok(out)
    }

    fn list_sources(&mut self) -> Result<Vec<SourceData>> {
        let sources = Rc::new(RefCell::new(Vec::new()));
        let sources_cb = Rc::clone(&sources);
        let introspector = self.context.introspect();
        let op = introspector.get_source_info_list(move |res| {
            if let ListResult::Item(info) = res {
                let name = info.name.as_deref().unwrap_or_default().to_string();
                let ports = info
                    .ports
                    .iter()
                    .map(|p| PortSnapshot {
                        name: p.name.as_deref().unwrap_or_default().to_string(),
                        available: PortAvailability::from(p.available),
                    })
                    .collect();
                sources_cb.borrow_mut().push(SourceData {
                    index: info.index,
                    name,
                    description: info.description.as_deref().map(str::to_string),
                    monitor_of_sink: info.monitor_of_sink,
                    device_class: info.proplist.get_str("device.class"),
                    node_name: info.proplist.get_str("node.name"),
                    node_description: info.proplist.get_str("node.description"),
                    device_description: info.proplist.get_str("device.description"),
                    device_bus: info.proplist.get_str("device.bus"),
                    device_icon_name: info.proplist.get_str("device.icon_name"),
                    device_form_factor: info.proplist.get_str("device.form_factor"),
                    active_port: info
                        .active_port
                        .as_ref()
                        .and_then(|p| p.name.as_deref())
                        .map(str::to_string),
                    ports,
                });
            }
        });
        self.wait_op(op)?;
        let out = sources.borrow().clone();
        Ok(out)
    }

    fn list_sink_inputs(&mut self) -> Result<Vec<SinkInputData>> {
        let streams = Rc::new(RefCell::new(Vec::new()));
        let streams_cb = Rc::clone(&streams);
        let introspector = self.context.introspect();
        let op = introspector.get_sink_input_info_list(move |res| {
            if let ListResult::Item(info) = res {
                streams_cb.borrow_mut().push(SinkInputData {
                    index: info.index,
                    sink: info.sink,
                    app_id: info.proplist.get_str("application.id"),
                    node_name: info.proplist.get_str("node.name"),
                });
            }
        });
        self.wait_op(op)?;
        let out = streams.borrow().clone();
        Ok(out)
    }

    fn list_source_outputs(&mut self) -> Result<Vec<SourceOutputData>> {
        let streams = Rc::new(RefCell::new(Vec::new()));
        let streams_cb = Rc::clone(&streams);
        let introspector = self.context.introspect();
        let op = introspector.get_source_output_info_list(move |res| {
            if let ListResult::Item(info) = res {
                streams_cb.borrow_mut().push(SourceOutputData {
                    index: info.index,
                    source: info.source,
                    app_id: info.proplist.get_str("application.id"),
                    node_name: info.proplist.get_str("node.name"),
                });
            }
        });
        self.wait_op(op)?;
        let out = streams.borrow().clone();
        Ok(out)
    }

    fn set_default_sink(&mut self, sink_name: &str) -> Result<()> {
        if sink_name.contains('\0') {
            return Err(anyhow!("Invalid sink name containing NUL byte"));
        }
        let ok = Rc::new(Cell::new(false));
        let ok_cb = Rc::clone(&ok);
        let op = self.context.set_default_sink(sink_name, move |success| {
            ok_cb.set(success);
        });
        self.wait_op(op)?;
        if !ok.get() {
            return Err(anyhow!(
                "PulseAudio/PipeWire rejected default sink '{sink_name}'"
            ));
        }
        Ok(())
    }

    fn set_default_source(&mut self, source_name: &str) -> Result<()> {
        if source_name.contains('\0') {
            return Err(anyhow!("Invalid source name containing NUL byte"));
        }
        let ok = Rc::new(Cell::new(false));
        let ok_cb = Rc::clone(&ok);
        let op = self
            .context
            .set_default_source(source_name, move |success| {
                ok_cb.set(success);
            });
        self.wait_op(op)?;
        if !ok.get() {
            return Err(anyhow!(
                "PulseAudio/PipeWire rejected default source '{source_name}'"
            ));
        }
        Ok(())
    }

    fn move_sink_input(&mut self, index: u32, sink_name: &str) -> Result<()> {
        if sink_name.contains('\0') {
            return Err(anyhow!("Invalid sink name containing NUL byte"));
        }
        let mut introspector = self.context.introspect();
        let op = introspector.move_sink_input_by_name(index, sink_name, None);
        self.wait_op(op)
    }

    fn move_source_output(&mut self, index: u32, source_name: &str) -> Result<()> {
        if source_name.contains('\0') {
            return Err(anyhow!("Invalid source name containing NUL byte"));
        }
        let mut introspector = self.context.introspect();
        let op = introspector.move_source_output_by_name(index, source_name, None);
        self.wait_op(op)
    }

    fn ensure_ready(&self) -> Result<()> {
        match self.context.get_state() {
            ContextState::Ready => Ok(()),
            state => Err(anyhow!(
                "PulseAudio/PipeWire context is not ready (state: {state:?})"
            )),
        }
    }

    fn default_audio_state(&mut self, target: AudioTarget) -> Result<DeviceAudioState> {
        let (default_sink, default_source) = self.default_sink_and_source()?;
        match target {
            AudioTarget::Output => {
                if default_sink.is_empty() {
                    return Err(anyhow!("No default audio output device found"));
                }
                self.sink_audio_state(&default_sink)
            }
            AudioTarget::Input => {
                if default_source.is_empty() {
                    return Err(anyhow!("No default audio input device found"));
                }
                self.source_audio_state(&default_source)
            }
        }
    }

    fn sink_audio_state(&mut self, sink_name: &str) -> Result<DeviceAudioState> {
        self.ensure_ready()?;
        if sink_name.contains('\0') {
            return Err(anyhow!("Invalid sink name containing NUL byte"));
        }
        let state = Rc::new(Cell::new(None));
        let failed = Rc::new(Cell::new(false));
        let state_cb = Rc::clone(&state);
        let failed_cb = Rc::clone(&failed);
        let introspector = self.context.introspect();
        let op = introspector.get_sink_info_by_name(sink_name, move |res| match res {
            ListResult::Item(info) => {
                state_cb.set(Some(DeviceAudioState {
                    index: info.index,
                    volume: info.volume,
                    mute: info.mute,
                }));
            }
            ListResult::End => {}
            ListResult::Error => {
                failed_cb.set(true);
            }
        });
        self.wait_op(op)?;
        if failed.get() {
            return Err(anyhow!(
                "PulseAudio/PipeWire failed to query default output '{sink_name}'"
            ));
        }
        state
            .get()
            .ok_or_else(|| anyhow!("Default audio output '{sink_name}' was not found"))
    }

    fn source_audio_state(&mut self, source_name: &str) -> Result<DeviceAudioState> {
        self.ensure_ready()?;
        if source_name.contains('\0') {
            return Err(anyhow!("Invalid source name containing NUL byte"));
        }
        let state = Rc::new(Cell::new(None));
        let failed = Rc::new(Cell::new(false));
        let state_cb = Rc::clone(&state);
        let failed_cb = Rc::clone(&failed);
        let introspector = self.context.introspect();
        let op = introspector.get_source_info_by_name(source_name, move |res| match res {
            ListResult::Item(info) => {
                state_cb.set(Some(DeviceAudioState {
                    index: info.index,
                    volume: info.volume,
                    mute: info.mute,
                }));
            }
            ListResult::End => {}
            ListResult::Error => {
                failed_cb.set(true);
            }
        });
        self.wait_op(op)?;
        if failed.get() {
            return Err(anyhow!(
                "PulseAudio/PipeWire failed to query default input '{source_name}'"
            ));
        }
        state
            .get()
            .ok_or_else(|| anyhow!("Default audio input '{source_name}' was not found"))
    }

    fn set_sink_volume(&mut self, index: u32, volume: &ChannelVolumes) -> Result<()> {
        self.ensure_ready()?;
        if !volume.is_valid() {
            return Err(anyhow!("Refusing to apply invalid output channel volume"));
        }
        let ok = Rc::new(Cell::new(false));
        let ok_cb = Rc::clone(&ok);
        let mut introspector = self.context.introspect();
        let op = introspector.set_sink_volume_by_index(
            index,
            volume,
            Some(Box::new(move |success| {
                ok_cb.set(success);
            })),
        );
        self.wait_op(op)?;
        if !ok.get() {
            return Err(anyhow!(
                "PulseAudio/PipeWire rejected volume update for sink {index}"
            ));
        }
        Ok(())
    }

    fn set_source_volume(&mut self, index: u32, volume: &ChannelVolumes) -> Result<()> {
        self.ensure_ready()?;
        if !volume.is_valid() {
            return Err(anyhow!("Refusing to apply invalid input channel volume"));
        }
        let ok = Rc::new(Cell::new(false));
        let ok_cb = Rc::clone(&ok);
        let mut introspector = self.context.introspect();
        let op = introspector.set_source_volume_by_index(
            index,
            volume,
            Some(Box::new(move |success| {
                ok_cb.set(success);
            })),
        );
        self.wait_op(op)?;
        if !ok.get() {
            return Err(anyhow!(
                "PulseAudio/PipeWire rejected volume update for source {index}"
            ));
        }
        Ok(())
    }

    fn set_sink_mute(&mut self, index: u32, mute: bool) -> Result<()> {
        self.ensure_ready()?;
        let ok = Rc::new(Cell::new(false));
        let ok_cb = Rc::clone(&ok);
        let mut introspector = self.context.introspect();
        let op = introspector.set_sink_mute_by_index(
            index,
            mute,
            Some(Box::new(move |success| {
                ok_cb.set(success);
            })),
        );
        self.wait_op(op)?;
        if !ok.get() {
            return Err(anyhow!(
                "PulseAudio/PipeWire rejected mute update for sink {index}"
            ));
        }
        Ok(())
    }

    fn set_source_mute(&mut self, index: u32, mute: bool) -> Result<()> {
        self.ensure_ready()?;
        let ok = Rc::new(Cell::new(false));
        let ok_cb = Rc::clone(&ok);
        let mut introspector = self.context.introspect();
        let op = introspector.set_source_mute_by_index(
            index,
            mute,
            Some(Box::new(move |success| {
                ok_cb.set(success);
            })),
        );
        self.wait_op(op)?;
        if !ok.get() {
            return Err(anyhow!(
                "PulseAudio/PipeWire rejected mute update for source {index}"
            ));
        }
        Ok(())
    }
}

fn step_mainloop_until(mainloop: &mut Mainloop, deadline: Instant) -> Result<()> {
    let now = Instant::now();
    if now >= deadline {
        return Err(anyhow!("PulseAudio/PipeWire operation timed out"));
    }
    let rem_us = deadline
        .saturating_duration_since(now)
        .as_micros()
        .min(u128::from(PULSE_POLL_SLICE_US)) as u64;
    mainloop
        .prepare(Some(MicroSeconds(rem_us)))
        .map_err(|e| anyhow!("PulseAudio mainloop prepare failed: {e}"))?;
    mainloop
        .poll()
        .map_err(|e| anyhow!("PulseAudio mainloop poll failed: {e}"))?;
    mainloop
        .dispatch()
        .map_err(|e| anyhow!("PulseAudio mainloop dispatch failed: {e}"))?;
    Ok(())
}

async fn query_sinks() -> Result<(String, Vec<SinkData>, Option<u32>)> {
    let (default_sink, raw_sinks) = tokio::task::spawn_blocking(|| -> Result<_> {
        let mut client = PulseClient::connect()?;
        let (default_sink, _) = client.default_sink_and_source()?;
        let sinks = client.list_sinks()?;
        Ok((default_sink, sinks))
    })
    .await??;

    let (filtered, ee_sink_index) = filter_and_sort_sinks(raw_sinks, &default_sink);
    Ok((default_sink, filtered, ee_sink_index))
}

async fn query_sources() -> Result<(String, Vec<SourceData>)> {
    let (default_source, raw_sources) = tokio::task::spawn_blocking(|| -> Result<_> {
        let mut client = PulseClient::connect()?;
        let (_, default_source) = client.default_sink_and_source()?;
        let sources = client.list_sources()?;
        Ok((default_source, sources))
    })
    .await??;

    let filtered = filter_and_sort_sources(raw_sources, &default_source);
    Ok((default_source, filtered))
}

async fn switch_sink(
    ctx: &AppContext,
    sink_name: String,
    label: String,
    ee_sink_index: Option<u32>,
) {
    let target = sink_name.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut client = PulseClient::connect()?;
        client.set_default_sink(&target)?;

        // Move active non-EasyEffects playback streams to the selected sink.
        // Streams routed into easyeffects_sink stay there; EasyEffects follows the default sink.
        if let Ok(streams) = client.list_sink_inputs() {
            for stream in streams {
                if should_move_sink_input(&stream, ee_sink_index) {
                    if let Err(err) = client.move_sink_input(stream.index, &target) {
                        debug!("Failed to move sink-input {}: {err}", stream.index);
                    }
                }
            }
        }
        Ok(())
    })
    .await;

    match res {
        Ok(Ok(())) => {
            notify(ctx, "Audio Output", &label, Urgency::Low, SYNC_OUTPUT).await;
        }
        Ok(Err(err)) => {
            notify(
                ctx,
                "Failed to switch output",
                &err.to_string(),
                Urgency::Critical,
                SYNC_OUTPUT,
            )
            .await;
        }
        Err(err) => {
            notify(
                ctx,
                "Failed to switch output",
                &err.to_string(),
                Urgency::Critical,
                SYNC_OUTPUT,
            )
            .await;
        }
    }
}

async fn switch_source(
    ctx: &AppContext,
    source_name: String,
    label: String,
    valid_source_indices: HashSet<u32>,
) {
    let target = source_name.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut client = PulseClient::connect()?;
        client.set_default_source(&target)?;

        // Move active non-EasyEffects capture streams (skipping monitor/loopback and EasyEffects).
        if let Ok(streams) = client.list_source_outputs() {
            for stream in streams {
                if should_move_source_output(&stream, &valid_source_indices) {
                    if let Err(err) = client.move_source_output(stream.index, &target) {
                        debug!("Failed to move source-output {}: {err}", stream.index);
                    }
                }
            }
        }
        Ok(())
    })
    .await;

    match res {
        Ok(Ok(())) => {
            notify(ctx, "Audio Input", &label, Urgency::Low, SYNC_INPUT).await;
        }
        Ok(Err(err)) => {
            notify(
                ctx,
                "Failed to switch input",
                &err.to_string(),
                Urgency::Critical,
                SYNC_INPUT,
            )
            .await;
        }
        Err(err) => {
            notify(
                ctx,
                "Failed to switch input",
                &err.to_string(),
                Urgency::Critical,
                SYNC_INPUT,
            )
            .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Pure filtering, labeling, sorting, and device-icon helpers
// ---------------------------------------------------------------------------

fn is_easyeffects_sink(sink: &SinkData) -> bool {
    sink.name == EASYEFFECTS_SINK_NAME
        || sink.node_name.as_deref() == Some(EASYEFFECTS_SINK_NAME)
}

fn is_sink_available(sink: &SinkData, default_sink: &str) -> bool {
    if sink.name == default_sink {
        return true;
    }
    if sink.ports.is_empty() {
        return true;
    }
    sink.ports
        .iter()
        .any(|p| p.available != PortAvailability::No)
}

fn sink_label(sink: &SinkData) -> String {
    for candidate in [
        sink.description.as_deref(),
        sink.node_description.as_deref(),
        sink.device_description.as_deref(),
        Some(sink.name.as_str()),
    ] {
        if let Some(text) = candidate.map(str::trim) {
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    "Unknown Output".to_string()
}

fn sink_icon<'a>(sink: &SinkData, icons: &'a IconsConfig) -> &'a str {
    let haystack = build_haystack(&[
        Some(sink.name.as_str()),
        sink.description.as_deref(),
        sink.active_port.as_deref(),
        sink.device_bus.as_deref(),
        sink.device_icon_name.as_deref(),
        sink.device_form_factor.as_deref(),
    ]);

    if haystack.contains("bluez")
        || sink
            .device_bus
            .as_deref()
            .is_some_and(|b| b.eq_ignore_ascii_case("bluetooth"))
    {
        return &icons.bluetooth;
    }
    if haystack.contains("headset")
        || haystack.contains("hands-free")
        || haystack.contains("handsfree")
    {
        return &icons.bt_headset;
    }
    if haystack.contains("headphone") {
        return &icons.bt_headphones;
    }
    if haystack.contains("hdmi") || haystack.contains("displayport") {
        return &icons.monitor;
    }
    &icons.speaker
}

fn filter_and_sort_sinks(sinks: Vec<SinkData>, default_sink: &str) -> (Vec<SinkData>, Option<u32>) {
    let mut ee_sink_index = None;
    let mut filtered = Vec::new();

    for sink in sinks {
        if sink.name.is_empty() {
            continue;
        }
        if is_easyeffects_sink(&sink) {
            ee_sink_index = Some(sink.index);
            continue;
        }
        if is_sink_available(&sink, default_sink) {
            filtered.push(sink);
        }
    }

    filtered.sort_by(|a, b| {
        let a_not_default = a.name != default_sink;
        let b_not_default = b.name != default_sink;
        a_not_default
            .cmp(&b_not_default)
            .then_with(|| sink_label(a).to_lowercase().cmp(&sink_label(b).to_lowercase()))
    });

    (filtered, ee_sink_index)
}

fn should_move_sink_input(stream: &SinkInputData, ee_sink_index: Option<u32>) -> bool {
    if ee_sink_index.is_some_and(|idx| stream.sink == idx) {
        return false;
    }
    if is_easyeffects_stream(stream.app_id.as_deref(), stream.node_name.as_deref()) {
        return false;
    }
    true
}

fn is_monitor_or_virtual_source(source: &SourceData) -> bool {
    source.monitor_of_sink.is_some()
        || source
            .device_class
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case("monitor"))
        || source.name.ends_with(".monitor")
        || source.name == EASYEFFECTS_SOURCE_NAME
        || source.node_name.as_deref() == Some(EASYEFFECTS_SOURCE_NAME)
}

fn is_source_available(source: &SourceData, default_source: &str) -> bool {
    if source.name == default_source {
        return true;
    }
    if source.ports.is_empty() {
        return true;
    }
    source
        .ports
        .iter()
        .any(|p| p.available != PortAvailability::No)
}

fn source_label(source: &SourceData) -> String {
    for candidate in [
        source.description.as_deref(),
        source.node_description.as_deref(),
        source.device_description.as_deref(),
        Some(source.name.as_str()),
    ] {
        if let Some(text) = candidate.map(str::trim) {
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    "Unknown Input".to_string()
}

fn source_icon<'a>(source: &SourceData, icons: &'a IconsConfig) -> &'a str {
    let haystack = build_haystack(&[
        Some(source.name.as_str()),
        source.description.as_deref(),
        source.active_port.as_deref(),
        source.device_bus.as_deref(),
        source.device_icon_name.as_deref(),
        source.device_form_factor.as_deref(),
    ]);

    if haystack.contains("bluez")
        || source
            .device_bus
            .as_deref()
            .is_some_and(|b| b.eq_ignore_ascii_case("bluetooth"))
    {
        return &icons.bluetooth;
    }
    if haystack.contains("headset")
        || haystack.contains("hands-free")
        || haystack.contains("handsfree")
    {
        return &icons.bt_headset;
    }
    if haystack.contains("webcam") || haystack.contains("camera") {
        return &icons.camera;
    }
    &icons.mic
}

fn filter_and_sort_sources(sources: Vec<SourceData>, default_source: &str) -> Vec<SourceData> {
    let mut filtered: Vec<SourceData> = sources
        .into_iter()
        .filter(|s| {
            !s.name.is_empty()
                && !is_monitor_or_virtual_source(s)
                && is_source_available(s, default_source)
        })
        .collect();

    filtered.sort_by(|a, b| {
        let a_not_default = a.name != default_source;
        let b_not_default = b.name != default_source;
        a_not_default.cmp(&b_not_default).then_with(|| {
            source_label(a)
                .to_lowercase()
                .cmp(&source_label(b).to_lowercase())
        })
    });

    filtered
}

fn should_move_source_output(
    stream: &SourceOutputData,
    valid_source_indices: &HashSet<u32>,
) -> bool {
    if !valid_source_indices.contains(&stream.source) {
        return false;
    }
    if is_easyeffects_stream(stream.app_id.as_deref(), stream.node_name.as_deref()) {
        return false;
    }
    true
}

fn is_easyeffects_stream(app_id: Option<&str>, node_name: Option<&str>) -> bool {
    let app_lower = app_id.unwrap_or_default().to_ascii_lowercase();
    let node_lower = node_name.unwrap_or_default().to_ascii_lowercase();
    app_lower.contains("easyeffects") || node_lower.starts_with("ee_")
}

fn build_haystack(parts: &[Option<&str>]) -> String {
    let mut out = String::new();
    for part in parts.iter().flatten() {
        if !part.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&part.to_lowercase());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// EasyEffects integration (optional, only when `easyeffects` is on PATH)
// ---------------------------------------------------------------------------

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            if meta.is_file() && (meta.permissions().mode() & 0o111 != 0) {
                return Some(candidate);
            }
        }
    }
    None
}

fn easyeffects_installed() -> bool {
    find_in_path("easyeffects").is_some()
}

fn process_running_for_current_user(comm_name: &str) -> bool {
    let my_uid = unsafe { libc::geteuid() };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        if !name_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.uid() != my_uid {
            continue;
        }
        if let Ok(comm) = std::fs::read_to_string(path.join("comm")) {
            if comm.trim() == comm_name {
                return true;
            }
        }
    }
    false
}

async fn systemd_unit_is_active(ctx: &AppContext, unit: &str) -> bool {
    let Ok(conn) = ctx.session_bus().await else {
        return false;
    };
    let Ok(reply) = conn
        .call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "GetUnit",
            &(unit,),
        )
        .await
    else {
        return false;
    };
    let Ok(unit_path) = reply.body().deserialize::<OwnedObjectPath>() else {
        return false;
    };
    let Ok(prop_reply) = conn
        .call_method(
            Some("org.freedesktop.systemd1"),
            unit_path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.systemd1.Unit", "ActiveState"),
        )
        .await
    else {
        return false;
    };
    let Ok(value) = prop_reply.body().deserialize::<OwnedValue>() else {
        return false;
    };
    String::try_from(value)
        .map(|s| s == "active")
        .unwrap_or(false)
}

async fn easyeffects_running(ctx: &AppContext) -> bool {
    if process_running_for_current_user("easyeffects") {
        return true;
    }
    systemd_unit_is_active(ctx, EASYEFFECTS_UNIT).await
}

async fn start_easyeffects_service(ctx: &AppContext) -> bool {
    if let Ok(conn) = ctx.session_bus().await {
        let res = conn
            .call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                "StartUnit",
                &(EASYEFFECTS_UNIT, "replace"),
            )
            .await;
        if res.is_ok() {
            return true;
        }
        debug!("systemd1 StartUnit failed for {EASYEFFECTS_UNIT}: {res:?}");
    }

    spawn_reaped(
        Command::new("easyeffects")
            .args(["--service-mode", "--hide-window"])
            .process_group(0),
    )
    .is_ok()
}

fn open_easyeffects_gui() {
    let _ = spawn_reaped(Command::new("easyeffects").process_group(0));
}

async fn run_easyeffects_cmd(args: &[&str], timeout: Duration) -> (bool, String, String) {
    let mut cmd = Command::new("easyeffects");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ),
        Ok(Err(err)) => (false, String::new(), err.to_string()),
        Err(_) => (false, String::new(), "Command timed out".to_string()),
    }
}

fn parse_ee_bool_flag(val: &str) -> Option<bool> {
    match val.trim() {
        "1" => Some(true),
        "2" => Some(false),
        _ => None,
    }
}

async fn get_eq_state(ctx: &AppContext) -> (bool, Option<bool>, String) {
    if !easyeffects_running(ctx).await {
        return (false, None, String::new());
    }

    let (bypass_res, preset_res) = tokio::join!(
        run_easyeffects_cmd(&["-b", "3"], EE_QUERY_TIMEOUT),
        run_easyeffects_cmd(&["-a", "output"], EE_QUERY_TIMEOUT),
    );

    let bypassed = if bypass_res.0 {
        parse_ee_bool_flag(&bypass_res.1)
    } else {
        None
    };
    let active_preset = if preset_res.0 {
        preset_res.1
    } else {
        String::new()
    };

    (true, bypassed, active_preset)
}

async fn get_fx_state(ctx: &AppContext) -> (bool, Option<bool>, Option<bool>, String) {
    if !easyeffects_running(ctx).await {
        return (false, None, None, String::new());
    }

    let (bypass_res, mon_res, preset_res) = tokio::join!(
        run_easyeffects_cmd(&["-b", "3"], EE_QUERY_TIMEOUT),
        run_easyeffects_cmd(&["--microphone-monitoring", "3"], EE_QUERY_TIMEOUT),
        run_easyeffects_cmd(&["-a", "input"], EE_QUERY_TIMEOUT),
    );

    let bypassed = if bypass_res.0 {
        parse_ee_bool_flag(&bypass_res.1)
    } else {
        None
    };
    let monitoring = if mon_res.0 {
        parse_ee_bool_flag(&mon_res.1)
    } else {
        None
    };
    let active_preset = if preset_res.0 {
        preset_res.1
    } else {
        String::new()
    };

    (true, bypassed, monitoring, active_preset)
}

fn easyeffects_preset_dirs(subdir: &str) -> Vec<PathBuf> {
    let mut dirs_list = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs_list.push(home.join(".local/share/easyeffects").join(subdir));
        dirs_list.push(home.join(".config/easyeffects").join(subdir));
    }
    if let Some(data_dir) = dirs::data_local_dir() {
        let p = data_dir.join("easyeffects").join(subdir);
        if !dirs_list.contains(&p) {
            dirs_list.push(p);
        }
    }
    if let Some(cfg_dir) = dirs::config_dir() {
        let p = cfg_dir.join("easyeffects").join(subdir);
        if !dirs_list.contains(&p) {
            dirs_list.push(p);
        }
    }
    dirs_list
}

fn discover_presets_in_dirs(dirs: &[PathBuf]) -> Vec<String> {
    let mut names = HashSet::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_file() && is_json_extension(&path) {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if !stem.is_empty() {
                        names.insert(stem.to_string());
                    }
                }
            }
        }
    }
    let mut sorted: Vec<String> = names.into_iter().collect();
    sorted.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    sorted
}

fn is_json_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

fn list_output_presets() -> Vec<String> {
    discover_presets_in_dirs(&easyeffects_preset_dirs("output"))
}

fn list_input_presets() -> Vec<String> {
    discover_presets_in_dirs(&easyeffects_preset_dirs("input"))
}

async fn set_eq_bypass(ctx: &AppContext, enable_bypass: bool) -> bool {
    let flag = if enable_bypass { "1" } else { "2" };
    let (ok, _, stderr) = run_easyeffects_cmd(&["-b", flag], EE_ACTION_TIMEOUT).await;
    if !ok {
        let msg = if stderr.is_empty() {
            "Failed to change bypass state"
        } else {
            &stderr
        };
        notify(ctx, "Equalizer error", msg, Urgency::Critical, SYNC_EQ).await;
        return false;
    }
    notify(
        ctx,
        "Equalizer",
        if enable_bypass { "Bypassed" } else { "Enabled" },
        Urgency::Low,
        SYNC_EQ,
    )
    .await;
    true
}

async fn set_fx_bypass(ctx: &AppContext, enable_bypass: bool) -> bool {
    let flag = if enable_bypass { "1" } else { "2" };
    let (ok, _, stderr) = run_easyeffects_cmd(&["-b", flag], EE_ACTION_TIMEOUT).await;
    if !ok {
        let msg = if stderr.is_empty() {
            "Failed to change bypass state"
        } else {
            &stderr
        };
        notify(ctx, "Audio Effects error", msg, Urgency::Critical, SYNC_EQ).await;
        return false;
    }
    notify(
        ctx,
        "Audio Effects",
        if enable_bypass { "Bypassed" } else { "Enabled" },
        Urgency::Low,
        SYNC_EQ,
    )
    .await;
    true
}

async fn set_mic_monitoring(ctx: &AppContext, enable: bool) {
    let flag = if enable { "1" } else { "2" };
    let (ok, _, stderr) =
        run_easyeffects_cmd(&["--microphone-monitoring", flag], EE_ACTION_TIMEOUT).await;
    if !ok {
        let msg = if stderr.is_empty() {
            "Failed to toggle monitoring"
        } else {
            &stderr
        };
        notify(
            ctx,
            "Mic Monitoring error",
            msg,
            Urgency::Critical,
            SYNC_INPUT,
        )
        .await;
        return;
    }
    notify(
        ctx,
        "Mic Monitoring",
        if enable { "Enabled" } else { "Disabled" },
        Urgency::Low,
        SYNC_INPUT,
    )
    .await;
}

async fn load_eq_preset(ctx: &AppContext, preset: &str, currently_bypassed: Option<bool>) {
    let (ok, _, stderr) = run_easyeffects_cmd(&["-l", preset], EE_ACTION_TIMEOUT).await;
    if !ok {
        let msg = if stderr.is_empty() { preset } else { &stderr };
        notify(
            ctx,
            "Failed to load preset",
            msg,
            Urgency::Critical,
            SYNC_EQ,
        )
        .await;
        return;
    }
    if currently_bypassed == Some(true) {
        let _ = run_easyeffects_cmd(&["-b", "2"], EE_QUERY_TIMEOUT).await;
    }
    notify(ctx, "Equalizer Preset", preset, Urgency::Low, SYNC_EQ).await;
}

async fn load_input_preset(ctx: &AppContext, preset: &str, currently_bypassed: Option<bool>) {
    let (ok, _, stderr) = run_easyeffects_cmd(&["-l", preset], EE_ACTION_TIMEOUT).await;
    if !ok {
        let msg = if stderr.is_empty() { preset } else { &stderr };
        notify(
            ctx,
            "Failed to load mic preset",
            msg,
            Urgency::Critical,
            SYNC_INPUT,
        )
        .await;
        return;
    }
    if currently_bypassed == Some(true) {
        let _ = run_easyeffects_cmd(&["-b", "2"], EE_QUERY_TIMEOUT).await;
    }
    notify(ctx, "Mic FX Preset", preset, Urgency::Low, SYNC_INPUT).await;
}

async fn notify(ctx: &AppContext, summary: &str, body: &str, urgency: Urgency, sync_id: &str) {
    let mut notif = Notification::new("Audio", summary)
        .urgency(urgency)
        .sync_id(sync_id);
    if !body.is_empty() {
        notif = notif.body(body);
    }
    ctx.notifier.send_best_effort(&notif).await;
}

// ---------------------------------------------------------------------------
// Public API: Output menu & EQ toggle
// ---------------------------------------------------------------------------

enum OutputAction {
    Sink { name: String, label: String },
    EqPresets { active_preset: String, bypassed: Option<bool> },
    EqBypass,
    EqEnable,
    EqStart,
    OpenEq,
    Separator,
    Noop,
}

enum OutputPresetAction {
    Preset(String),
    OpenEq,
    Back,
    Separator,
}

enum OutputScreen {
    Main,
    Presets {
        active_preset: String,
        bypassed: Option<bool>,
    },
}

pub async fn run_output_menu(ctx: &AppContext) -> Result<()> {
    let MenuSession::Acquired(guard) = ctx.menu.begin_session(MenuKind::AudioOutput) else {
        return Ok(());
    };

    let output = hyprland::detect_cursor_output().await;
    let icons = &ctx.config.icons;
    let mut screen = OutputScreen::Main;

    loop {
        match screen {
            OutputScreen::Main => {
                let (default_sink, sinks, ee_sink_index) = match query_sinks().await {
                    Ok(res) => res,
                    Err(err) => {
                        warn!("Failed to query PulseAudio/PipeWire sinks: {err}");
                        (String::new(), Vec::new(), None)
                    }
                };

                let mut entries: Vec<(String, OutputAction)> = Vec::new();
                for sink in &sinks {
                    let label = sink_label(sink);
                    let icon = sink_icon(sink, icons);
                    let mark = if sink.name == default_sink {
                        format!("  {}", icons.check)
                    } else {
                        String::new()
                    };
                    entries.push((
                        format!("{icon}  {label}{mark}"),
                        OutputAction::Sink {
                            name: sink.name.clone(),
                            label,
                        },
                    ));
                }

                if entries.is_empty() {
                    entries.push((
                        "󰖁  No output devices found".to_string(),
                        OutputAction::Noop,
                    ));
                }

                if easyeffects_installed() {
                    entries.push((SEPARATOR.to_string(), OutputAction::Separator));
                    let (running, bypassed, active_preset) = get_eq_state(ctx).await;
                    if running {
                        let preset_label = if active_preset.is_empty() {
                            "None"
                        } else {
                            active_preset.as_str()
                        };
                        entries.push((
                            format!("{}  EQ Preset: {preset_label}...", icons.eq),
                            OutputAction::EqPresets {
                                active_preset,
                                bypassed,
                            },
                        ));
                        if bypassed == Some(true) {
                            entries.push((
                                format!("{}  EQ: Bypassed (Enable)", icons.toggle_on),
                                OutputAction::EqEnable,
                            ));
                        } else {
                            entries.push((
                                format!("{}  EQ: Active (Bypass)", icons.toggle_off),
                                OutputAction::EqBypass,
                            ));
                        }
                        entries.push((
                            format!("{}  Open Equalizer...", icons.settings),
                            OutputAction::OpenEq,
                        ));
                    } else {
                        entries.push((
                            format!("{}  Start Equalizer Service", icons.eq),
                            OutputAction::EqStart,
                        ));
                        entries.push((
                            format!("{}  Open Equalizer...", icons.settings),
                            OutputAction::OpenEq,
                        ));
                    }
                }

                let opts = FuzzelOptions {
                    prompt: format!("{}  ", icons.speaker),
                    placeholder: Some("Select output or EQ...".to_string()),
                    anchor: Anchor::TopRight,
                    width: Some(38),
                    max_lines: Some(14),
                    minimal_lines: true,
                    output: output.clone(),
                    ..Default::default()
                };

                let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await?
                else {
                    break;
                };

                match &entries[idx].1 {
                    OutputAction::Sink { name, label } => {
                        switch_sink(ctx, name.clone(), label.clone(), ee_sink_index).await;
                        break;
                    }
                    OutputAction::EqPresets {
                        active_preset,
                        bypassed,
                    } => {
                        screen = OutputScreen::Presets {
                            active_preset: active_preset.clone(),
                            bypassed: *bypassed,
                        };
                    }
                    OutputAction::EqBypass => {
                        let _ = set_eq_bypass(ctx, true).await;
                        break;
                    }
                    OutputAction::EqEnable => {
                        let _ = set_eq_bypass(ctx, false).await;
                        break;
                    }
                    OutputAction::EqStart => {
                        if start_easyeffects_service(ctx).await {
                            notify(
                                ctx,
                                "Equalizer",
                                "Started EasyEffects service",
                                Urgency::Low,
                                SYNC_EQ,
                            )
                            .await;
                        } else {
                            notify(
                                ctx,
                                "Equalizer",
                                "Failed to start EasyEffects",
                                Urgency::Critical,
                                SYNC_EQ,
                            )
                            .await;
                        }
                        break;
                    }
                    OutputAction::OpenEq => {
                        open_easyeffects_gui();
                        break;
                    }
                    OutputAction::Separator => {
                        screen = OutputScreen::Main;
                    }
                    OutputAction::Noop => break,
                }
            }
            OutputScreen::Presets {
                ref active_preset,
                bypassed,
            } => {
                let presets = list_output_presets();
                let mut entries: Vec<(String, OutputPresetAction)> = Vec::new();

                for preset in &presets {
                    let mark = if preset == active_preset {
                        format!("  {}", icons.check)
                    } else {
                        String::new()
                    };
                    entries.push((
                        format!("{}  {preset}{mark}", icons.eq),
                        OutputPresetAction::Preset(preset.clone()),
                    ));
                }

                if presets.is_empty() {
                    entries.push((
                        format!(
                            "{}  No presets saved — Open Equalizer...",
                            icons.settings
                        ),
                        OutputPresetAction::OpenEq,
                    ));
                } else {
                    entries.push((SEPARATOR.to_string(), OutputPresetAction::Separator));
                    entries.push((
                        format!("{}  Open Equalizer...", icons.settings),
                        OutputPresetAction::OpenEq,
                    ));
                }
                entries.push((
                    format!("{}  Back", icons.back),
                    OutputPresetAction::Back,
                ));

                let opts = FuzzelOptions {
                    prompt: format!("{}  ", icons.eq),
                    placeholder: Some("Select EQ preset...".to_string()),
                    anchor: Anchor::TopRight,
                    width: Some(38),
                    max_lines: Some(12),
                    minimal_lines: true,
                    output: output.clone(),
                    ..Default::default()
                };

                let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await?
                else {
                    break;
                };

                match &entries[idx].1 {
                    OutputPresetAction::Preset(name) => {
                        load_eq_preset(ctx, name, bypassed).await;
                        break;
                    }
                    OutputPresetAction::OpenEq => {
                        open_easyeffects_gui();
                        break;
                    }
                    OutputPresetAction::Back => {
                        screen = OutputScreen::Main;
                    }
                    OutputPresetAction::Separator => {}
                }
            }
        }
    }

    Ok(())
}

pub async fn toggle_eq(ctx: &AppContext) -> Result<()> {
    if !easyeffects_installed() {
        notify(
            ctx,
            "Equalizer",
            "EasyEffects is not installed",
            Urgency::Critical,
            SYNC_EQ,
        )
        .await;
        return Err(anyhow!("EasyEffects is not installed"));
    }

    if !easyeffects_running(ctx).await {
        if start_easyeffects_service(ctx).await {
            notify(
                ctx,
                "Equalizer",
                "Started EasyEffects service",
                Urgency::Low,
                SYNC_EQ,
            )
            .await;
            return Ok(());
        }
        notify(
            ctx,
            "Equalizer",
            "Failed to start EasyEffects",
            Urgency::Critical,
            SYNC_EQ,
        )
        .await;
        return Err(anyhow!("Failed to start EasyEffects"));
    }

    let (_, bypassed, _) = get_eq_state(ctx).await;
    match bypassed {
        None => {
            let (ok, _, stderr) =
                run_easyeffects_cmd(&["--bypass-toggle"], EE_ACTION_TIMEOUT).await;
            if !ok {
                let msg = if stderr.is_empty() {
                    "Failed to toggle EQ"
                } else {
                    &stderr
                };
                notify(ctx, "Equalizer error", msg, Urgency::Critical, SYNC_EQ).await;
                return Err(anyhow!("{msg}"));
            }
            let (_, new_bypassed, _) = get_eq_state(ctx).await;
            let state = if new_bypassed == Some(true) {
                "Bypassed"
            } else {
                "Enabled"
            };
            notify(ctx, "Equalizer", state, Urgency::Low, SYNC_EQ).await;
            Ok(())
        }
        Some(is_bypassed) => {
            if set_eq_bypass(ctx, !is_bypassed).await {
                Ok(())
            } else {
                Err(anyhow!("Failed to change Equalizer bypass state"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API: Input menu & Mic FX toggle
// ---------------------------------------------------------------------------

enum InputAction {
    Source { name: String, label: String },
    FxPresets { active_preset: String, bypassed: Option<bool> },
    MonOn,
    MonOff,
    FxBypass,
    FxEnable,
    FxStart,
    OpenFx,
    Separator,
    Noop,
}

enum InputPresetAction {
    Preset(String),
    OpenFx,
    Back,
    Separator,
}

enum InputScreen {
    Main,
    Presets {
        active_preset: String,
        bypassed: Option<bool>,
    },
}

pub async fn run_input_menu(ctx: &AppContext) -> Result<()> {
    let MenuSession::Acquired(guard) = ctx.menu.begin_session(MenuKind::AudioInput) else {
        return Ok(());
    };

    let output = hyprland::detect_cursor_output().await;
    let icons = &ctx.config.icons;
    let mut screen = InputScreen::Main;

    loop {
        match screen {
            InputScreen::Main => {
                let (default_source, sources) = match query_sources().await {
                    Ok(res) => res,
                    Err(err) => {
                        warn!("Failed to query PulseAudio/PipeWire sources: {err}");
                        (String::new(), Vec::new())
                    }
                };
                let valid_indices: HashSet<u32> = sources.iter().map(|s| s.index).collect();

                let mut entries: Vec<(String, InputAction)> = Vec::new();
                for src in &sources {
                    let label = source_label(src);
                    let icon = source_icon(src, icons);
                    let mark = if src.name == default_source {
                        format!("  {}", icons.check)
                    } else {
                        String::new()
                    };
                    entries.push((
                        format!("{icon}  {label}{mark}"),
                        InputAction::Source {
                            name: src.name.clone(),
                            label,
                        },
                    ));
                }

                if entries.is_empty() {
                    entries.push((
                        "󰍭  No input devices found".to_string(),
                        InputAction::Noop,
                    ));
                }

                if easyeffects_installed() {
                    entries.push((SEPARATOR.to_string(), InputAction::Separator));
                    let (running, bypassed, monitoring, active_preset) = get_fx_state(ctx).await;
                    if running {
                        let preset_label = if active_preset.is_empty() {
                            "None"
                        } else {
                            active_preset.as_str()
                        };
                        entries.push((
                            format!("{}  Mic FX Preset: {preset_label}...", icons.eq),
                            InputAction::FxPresets {
                                active_preset,
                                bypassed,
                            },
                        ));
                        if monitoring == Some(true) {
                            entries.push((
                                format!("{}  Mic Monitor: On (Disable)", icons.bt_headphones),
                                InputAction::MonOff,
                            ));
                        } else {
                            entries.push((
                                format!("{}  Mic Monitor: Off (Enable)", icons.bt_headphones),
                                InputAction::MonOn,
                            ));
                        }
                        if bypassed == Some(true) {
                            entries.push((
                                format!("{}  Effects: Bypassed (Enable)", icons.toggle_on),
                                InputAction::FxEnable,
                            ));
                        } else {
                            entries.push((
                                format!("{}  Effects: Active (Bypass)", icons.toggle_off),
                                InputAction::FxBypass,
                            ));
                        }
                        entries.push((
                            format!("{}  Open EasyEffects...", icons.settings),
                            InputAction::OpenFx,
                        ));
                    } else {
                        entries.push((
                            format!("{}  Start EasyEffects Service", icons.eq),
                            InputAction::FxStart,
                        ));
                        entries.push((
                            format!("{}  Open EasyEffects...", icons.settings),
                            InputAction::OpenFx,
                        ));
                    }
                }

                let opts = FuzzelOptions {
                    prompt: format!("{}  ", icons.mic),
                    placeholder: Some("Select input or Mic FX...".to_string()),
                    anchor: Anchor::TopRight,
                    width: Some(38),
                    max_lines: Some(14),
                    minimal_lines: true,
                    output: output.clone(),
                    ..Default::default()
                };

                let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await?
                else {
                    break;
                };

                match &entries[idx].1 {
                    InputAction::Source { name, label } => {
                        switch_source(ctx, name.clone(), label.clone(), valid_indices).await;
                        break;
                    }
                    InputAction::FxPresets {
                        active_preset,
                        bypassed,
                    } => {
                        screen = InputScreen::Presets {
                            active_preset: active_preset.clone(),
                            bypassed: *bypassed,
                        };
                    }
                    InputAction::MonOn => {
                        set_mic_monitoring(ctx, true).await;
                        break;
                    }
                    InputAction::MonOff => {
                        set_mic_monitoring(ctx, false).await;
                        break;
                    }
                    InputAction::FxBypass => {
                        let _ = set_fx_bypass(ctx, true).await;
                        break;
                    }
                    InputAction::FxEnable => {
                        let _ = set_fx_bypass(ctx, false).await;
                        break;
                    }
                    InputAction::FxStart => {
                        if start_easyeffects_service(ctx).await {
                            notify(
                                ctx,
                                "Audio Effects",
                                "Started EasyEffects service",
                                Urgency::Low,
                                SYNC_EQ,
                            )
                            .await;
                        } else {
                            notify(
                                ctx,
                                "Audio Effects",
                                "Failed to start EasyEffects",
                                Urgency::Critical,
                                SYNC_EQ,
                            )
                            .await;
                        }
                        break;
                    }
                    InputAction::OpenFx => {
                        open_easyeffects_gui();
                        break;
                    }
                    InputAction::Separator => {
                        screen = InputScreen::Main;
                    }
                    InputAction::Noop => break,
                }
            }
            InputScreen::Presets {
                ref active_preset,
                bypassed,
            } => {
                let presets = list_input_presets();
                let mut entries: Vec<(String, InputPresetAction)> = Vec::new();

                for preset in &presets {
                    let mark = if preset == active_preset {
                        format!("  {}", icons.check)
                    } else {
                        String::new()
                    };
                    entries.push((
                        format!("{}  {preset}{mark}", icons.eq),
                        InputPresetAction::Preset(preset.clone()),
                    ));
                }

                if presets.is_empty() {
                    entries.push((
                        format!(
                            "{}  No mic presets saved — Open EasyEffects...",
                            icons.settings
                        ),
                        InputPresetAction::OpenFx,
                    ));
                } else {
                    entries.push((SEPARATOR.to_string(), InputPresetAction::Separator));
                    entries.push((
                        format!("{}  Open EasyEffects...", icons.settings),
                        InputPresetAction::OpenFx,
                    ));
                }
                entries.push((
                    format!("{}  Back", icons.back),
                    InputPresetAction::Back,
                ));

                let opts = FuzzelOptions {
                    prompt: format!("{}  ", icons.eq),
                    placeholder: Some("Select Mic FX preset...".to_string()),
                    anchor: Anchor::TopRight,
                    width: Some(38),
                    max_lines: Some(12),
                    minimal_lines: true,
                    output: output.clone(),
                    ..Default::default()
                };

                let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await?
                else {
                    break;
                };

                match &entries[idx].1 {
                    InputPresetAction::Preset(name) => {
                        load_input_preset(ctx, name, bypassed).await;
                        break;
                    }
                    InputPresetAction::OpenFx => {
                        open_easyeffects_gui();
                        break;
                    }
                    InputPresetAction::Back => {
                        screen = InputScreen::Main;
                    }
                    InputPresetAction::Separator => {}
                }
            }
        }
    }

    Ok(())
}

pub async fn toggle_mic_fx(ctx: &AppContext) -> Result<()> {
    if !easyeffects_installed() {
        notify(
            ctx,
            "Audio Effects",
            "EasyEffects is not installed",
            Urgency::Critical,
            SYNC_EQ,
        )
        .await;
        return Err(anyhow!("EasyEffects is not installed"));
    }

    if !easyeffects_running(ctx).await {
        if start_easyeffects_service(ctx).await {
            notify(
                ctx,
                "Audio Effects",
                "Started EasyEffects service",
                Urgency::Low,
                SYNC_EQ,
            )
            .await;
            return Ok(());
        }
        notify(
            ctx,
            "Audio Effects",
            "Failed to start EasyEffects",
            Urgency::Critical,
            SYNC_EQ,
        )
        .await;
        return Err(anyhow!("Failed to start EasyEffects"));
    }

    let (_, bypassed, _, _) = get_fx_state(ctx).await;
    match bypassed {
        None => {
            let (ok, _, stderr) =
                run_easyeffects_cmd(&["--bypass-toggle"], EE_ACTION_TIMEOUT).await;
            if !ok {
                let msg = if stderr.is_empty() {
                    "Failed to toggle FX"
                } else {
                    &stderr
                };
                notify(ctx, "Audio Effects error", msg, Urgency::Critical, SYNC_EQ).await;
                return Err(anyhow!("{msg}"));
            }
            let (_, new_bypassed, _, _) = get_fx_state(ctx).await;
            let state = if new_bypassed == Some(true) {
                "Bypassed"
            } else {
                "Enabled"
            };
            notify(ctx, "Audio Effects", state, Urgency::Low, SYNC_EQ).await;
            Ok(())
        }
        Some(is_bypassed) => {
            if set_fx_bypass(ctx, !is_bypassed).await {
                Ok(())
            } else {
                Err(anyhow!("Failed to change Audio Effects bypass state"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API: Volume & mute controls
// ---------------------------------------------------------------------------

fn target_lock(target: AudioTarget) -> &'static Mutex<()> {
    static OUTPUT_LOCK: Mutex<()> = Mutex::const_new(());
    static INPUT_LOCK: Mutex<()> = Mutex::const_new(());
    match target {
        AudioTarget::Output => &OUTPUT_LOCK,
        AudioTarget::Input => &INPUT_LOCK,
    }
}

fn percent_to_volume(percent: u16) -> Volume {
    let raw = (u64::from(percent) * u64::from(Volume::NORMAL.0) + 50) / 100;
    Volume(raw.min(u64::from(Volume::MAX.0)) as u32)
}

fn delta_percent_to_raw(delta: i32) -> i64 {
    let scaled = i64::from(delta) * i64::from(Volume::NORMAL.0);
    if scaled >= 0 {
        (scaled + 50) / 100
    } else {
        (scaled - 50) / 100
    }
}

fn compute_adjusted_volume(
    current: &ChannelVolumes,
    delta: i32,
    max_volume: u16,
) -> Result<ChannelVolumes> {
    let channel_count = current.len();
    if channel_count == 0 || channel_count > ChannelVolumes::CHANNELS_MAX {
        return Err(anyhow!(
            "Audio device reported an invalid channel count ({channel_count})"
        ));
    }

    let current_channels = current.get();
    let old_peak = current_channels.iter().map(|v| v.0).max().unwrap_or(0);
    let limit = percent_to_volume(max_volume).0;
    let delta_raw = delta_percent_to_raw(delta);
    let target_peak = (i64::from(old_peak) + delta_raw).clamp(0, i64::from(limit)) as u32;

    let mut next = *current;
    if target_peak == 0 {
        for ch in next.get_mut() {
            *ch = Volume::MUTED;
        }
    } else if old_peak == 0 {
        let target = Volume(target_peak);
        for ch in next.get_mut() {
            *ch = target;
        }
    } else {
        let old_peak_u128 = u128::from(old_peak);
        let target_peak_u128 = u128::from(target_peak);
        for ch in next.get_mut() {
            let scaled = (u128::from(ch.0) * target_peak_u128 + old_peak_u128 / 2) / old_peak_u128;
            ch.0 = scaled.min(target_peak_u128) as u32;
        }
    }

    Ok(next)
}

pub async fn adjust_volume(target: AudioTarget, delta: i32, config: &AudioConfig) -> Result<()> {
    let max_volume = config.max_volume;
    let unmute_on_increase = config.unmute_on_volume_up && delta > 0;
    let _guard = target_lock(target).lock().await;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut client = PulseClient::connect()?;
        let state = client.default_audio_state(target)?;
        let next_volume = compute_adjusted_volume(&state.volume, delta, max_volume)?;
        match target {
            AudioTarget::Output => {
                client.set_sink_volume(state.index, &next_volume)?;
                if unmute_on_increase && state.mute {
                    client
                        .set_sink_mute(state.index, false)
                        .context("Volume adjusted, but the audio output could not be unmuted")?;
                }
                Ok(())
            }
            AudioTarget::Input => client.set_source_volume(state.index, &next_volume),
        }
    })
    .await?
}

pub async fn toggle_mute(target: AudioTarget) -> Result<()> {
    let _guard = target_lock(target).lock().await;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut client = PulseClient::connect()?;
        let state = client.default_audio_state(target)?;
        let next_mute = !state.mute;
        match target {
            AudioTarget::Output => client.set_sink_mute(state.index, next_mute),
            AudioTarget::Input => client.set_source_mute(state.index, next_mute),
        }
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_sink(index: u32, name: &str, desc: Option<&str>) -> SinkData {
        SinkData {
            index,
            name: name.to_string(),
            description: desc.map(str::to_string),
            node_name: None,
            node_description: None,
            device_description: None,
            device_bus: None,
            device_icon_name: None,
            device_form_factor: None,
            active_port: None,
            ports: Vec::new(),
        }
    }

    fn make_source(index: u32, name: &str, desc: Option<&str>) -> SourceData {
        SourceData {
            index,
            name: name.to_string(),
            description: desc.map(str::to_string),
            monitor_of_sink: None,
            device_class: None,
            node_name: None,
            node_description: None,
            device_description: None,
            device_bus: None,
            device_icon_name: None,
            device_form_factor: None,
            active_port: None,
            ports: Vec::new(),
        }
    }

    #[test]
    fn filter_and_sort_sinks_excludes_easyeffects_and_unavailable_but_keeps_default() {
        let mut ee_by_name = make_sink(10, "easyeffects_sink", Some("EasyEffects Sink"));
        ee_by_name.node_name = Some("easyeffects_sink".to_string());

        let mut ee_by_prop = make_sink(11, "custom_ee", Some("EE Alias"));
        ee_by_prop.node_name = Some("easyeffects_sink".to_string());

        let mut unavail_non_default = make_sink(1, "alsa_hdmi_unplugged", Some("HDMI Unplugged"));
        unavail_non_default.ports = vec![PortSnapshot {
            name: "hdmi-output-0".to_string(),
            available: PortAvailability::No,
        }];

        let mut unavail_default = make_sink(2, "alsa_default_unavail", Some("Zed Default"));
        unavail_default.ports = vec![PortSnapshot {
            name: "analog-output".to_string(),
            available: PortAvailability::No,
        }];

        let mut mixed_ports = make_sink(3, "alsa_speakers", Some("Analog Speakers"));
        mixed_ports.ports = vec![
            PortSnapshot {
                name: "headphones".to_string(),
                available: PortAvailability::No,
            },
            PortSnapshot {
                name: "speakers".to_string(),
                available: PortAvailability::Yes,
            },
        ];

        let no_ports = make_sink(4, "bluez_dac", Some("Bluetooth DAC"));

        let (filtered, ee_idx) = filter_and_sort_sinks(
            vec![
                ee_by_name,
                ee_by_prop,
                unavail_non_default,
                unavail_default,
                mixed_ports,
                no_ports,
            ],
            "alsa_default_unavail",
        );

        assert_eq!(ee_idx, Some(11));
        let names: Vec<&str> = filtered.iter().map(|s| s.name.as_str()).collect();
        // Default sink first, then alphabetical by label ("Analog Speakers", "Bluetooth DAC").
        assert_eq!(
            names,
            vec!["alsa_default_unavail", "alsa_speakers", "bluez_dac"]
        );
    }

    #[test]
    fn filter_and_sort_sources_excludes_monitors_and_easyeffects_and_unavailable() {
        let mut monitor_by_field = make_sink_monitor_source(1, "alsa_out_mon", Some(0));
        monitor_by_field.device_class = None;

        let mut monitor_by_class = make_source(2, "virtual_mon", Some("Virtual Mon"));
        monitor_by_class.device_class = Some("monitor".to_string());

        let monitor_by_suffix = make_source(3, "alsa_output.pci.monitor", Some("Monitor"));

        let ee_src = make_source(4, "easyeffects_source", Some("EasyEffects Source"));

        let mut unavail_mic = make_source(5, "alsa_unplugged_mic", Some("Front Mic"));
        unavail_mic.ports = vec![PortSnapshot {
            name: "analog-input-front-mic".to_string(),
            available: PortAvailability::No,
        }];

        let mut usb_mic = make_source(6, "alsa_input.usb_mic", Some("USB Condenser Mic"));
        usb_mic.ports = vec![PortSnapshot {
            name: "analog-input".to_string(),
            available: PortAvailability::Unknown,
        }];

        let default_mic = make_source(7, "alsa_input.internal", Some("Internal Mic"));

        let filtered = filter_and_sort_sources(
            vec![
                monitor_by_field,
                monitor_by_class,
                monitor_by_suffix,
                ee_src,
                unavail_mic,
                usb_mic,
                default_mic,
            ],
            "alsa_input.internal",
        );

        let names: Vec<&str> = filtered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alsa_input.internal", "alsa_input.usb_mic"]);
    }

    fn make_sink_monitor_source(index: u32, name: &str, mon_of: Option<u32>) -> SourceData {
        let mut s = make_source(index, name, Some("Monitor"));
        s.monitor_of_sink = mon_of;
        s
    }

    #[test]
    fn sink_icon_detection_matches_device_types() {
        let icons = IconsConfig::default();

        let mut bt = make_sink(1, "bluez_output.AA_BB", Some("WH-1000XM5"));
        bt.device_bus = Some("bluetooth".to_string());
        assert_eq!(sink_icon(&bt, &icons), icons.bluetooth);

        let mut headset = make_sink(2, "usb_audio", Some("HyperX Cloud Headset"));
        headset.device_form_factor = Some("headset".to_string());
        assert_eq!(sink_icon(&headset, &icons), icons.bt_headset);

        let mut headphones = make_sink(3, "alsa_pci", Some("Built-in Audio"));
        headphones.active_port = Some("analog-output-headphones".to_string());
        assert_eq!(sink_icon(&headphones, &icons), icons.bt_headphones);

        let hdmi = make_sink(4, "alsa_output.hdmi", Some("Navi HDMI/DP Audio"));
        assert_eq!(sink_icon(&hdmi, &icons), icons.monitor);

        let dp = make_sink(5, "alsa_output.dp", Some("DisplayPort Output"));
        assert_eq!(sink_icon(&dp, &icons), icons.monitor);

        let speaker = make_sink(6, "alsa_output.pci", Some("Built-in Speakers"));
        assert_eq!(sink_icon(&speaker, &icons), icons.speaker);
    }

    #[test]
    fn source_icon_detection_matches_device_types() {
        let icons = IconsConfig::default();

        let bt = make_source(1, "bluez_input.AA_BB", Some("AirPods"));
        assert_eq!(source_icon(&bt, &icons), icons.bluetooth);

        let headset = make_source(2, "usb_headset_in", Some("Logitech Hands-Free"));
        assert_eq!(source_icon(&headset, &icons), icons.bt_headset);

        let webcam = make_source(3, "alsa_input.usb_c920", Some("HD Pro Webcam C920"));
        assert_eq!(source_icon(&webcam, &icons), icons.camera);

        let mic = make_source(4, "alsa_input.pci", Some("Built-in Microphone"));
        assert_eq!(source_icon(&mic, &icons), icons.mic);
    }

    #[test]
    fn stream_move_filtering_skips_easyeffects_and_invalid_sources() {
        let regular_in = SinkInputData {
            index: 1,
            sink: 5,
            app_id: Some("org.mozilla.firefox".to_string()),
            node_name: Some("Firefox".to_string()),
        };
        assert!(should_move_sink_input(&regular_in, Some(99)));
        assert!(!should_move_sink_input(&regular_in, Some(5)));

        let ee_in_app = SinkInputData {
            index: 2,
            sink: 5,
            app_id: Some("com.github.wwmm.easyeffects".to_string()),
            node_name: Some("playback".to_string()),
        };
        assert!(!should_move_sink_input(&ee_in_app, Some(99)));

        let ee_in_node = SinkInputData {
            index: 3,
            sink: 5,
            app_id: None,
            node_name: Some("ee_soe_output_level".to_string()),
        };
        assert!(!should_move_sink_input(&ee_in_node, Some(99)));

        let valid_sources: HashSet<u32> = [10, 12].into_iter().collect();
        let valid_out = SourceOutputData {
            index: 1,
            source: 10,
            app_id: Some("discord".to_string()),
            node_name: Some("Discord".to_string()),
        };
        assert!(should_move_source_output(&valid_out, &valid_sources));

        let monitor_out = SourceOutputData {
            index: 2,
            source: 99,
            app_id: Some("obs".to_string()),
            node_name: Some("OBS".to_string()),
        };
        assert!(!should_move_source_output(&monitor_out, &valid_sources));

        let ee_out = SourceOutputData {
            index: 3,
            source: 10,
            app_id: Some("EasyEffects".to_string()),
            node_name: Some("ee_sie_input".to_string()),
        };
        assert!(!should_move_source_output(&ee_out, &valid_sources));
    }

    #[test]
    fn preset_discovery_deduplicates_filters_json_and_sorts_case_insensitively() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "fuzzyland-audio-test-{}-{nanos}",
            std::process::id()
        ));
        let dir_a = base.join("local");
        let dir_b = base.join("config");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        std::fs::write(dir_a.join("zebra.json"), "{}").unwrap();
        std::fs::write(dir_a.join("Alpha.json"), "{}").unwrap();
        std::fs::write(dir_a.join("ignore.txt"), "nope").unwrap();
        std::fs::create_dir_all(dir_a.join("subdir.json")).unwrap();

        std::fs::write(dir_b.join("beta.json"), "{}").unwrap();
        std::fs::write(dir_b.join("Alpha.json"), "{}").unwrap();

        let presets = discover_presets_in_dirs(&[dir_a, dir_b, base.join("missing")]);
        assert_eq!(presets, vec!["Alpha", "beta", "zebra"]);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn label_fallback_prefers_non_empty_candidates() {
        let mut sink = make_sink(1, "fallback_name", Some("   "));
        sink.node_description = Some("Node Desc".to_string());
        assert_eq!(sink_label(&sink), "Node Desc");

        sink.node_description = None;
        sink.device_description = Some("Device Desc".to_string());
        assert_eq!(sink_label(&sink), "Device Desc");

        sink.device_description = None;
        assert_eq!(sink_label(&sink), "fallback_name");

        sink.name.clear();
        assert_eq!(sink_label(&sink), "Unknown Output");
    }

    #[test]
    fn parse_easyeffects_tri_state_flags() {
        assert_eq!(parse_ee_bool_flag("1"), Some(true));
        assert_eq!(parse_ee_bool_flag("2\n"), Some(false));
        assert_eq!(parse_ee_bool_flag("0"), None);
        assert_eq!(parse_ee_bool_flag(""), None);
    }

    fn make_cvolume(values: &[u32]) -> ChannelVolumes {
        let mut cv = ChannelVolumes::default();
        cv.set_len(values.len() as u8);
        for (slot, &val) in cv.get_mut().iter_mut().zip(values) {
            *slot = Volume(val);
        }
        cv
    }

    #[test]
    fn audio_target_serde_uses_kebab_case() {
        assert_eq!(
            serde_json::to_string(&AudioTarget::Output).unwrap(),
            "\"output\""
        );
        assert_eq!(
            serde_json::to_string(&AudioTarget::Input).unwrap(),
            "\"input\""
        );
        assert_eq!(
            serde_json::from_str::<AudioTarget>("\"output\"").unwrap(),
            AudioTarget::Output
        );
        assert_eq!(
            serde_json::from_str::<AudioTarget>("\"input\"").unwrap(),
            AudioTarget::Input
        );
    }

    #[test]
    fn volume_math_applies_relative_steps_and_preserves_channel_balance() {
        let half = Volume::NORMAL.0 / 2;
        let quarter = Volume::NORMAL.0 / 4;
        let unbalanced = make_cvolume(&[half, quarter]);

        let raised = compute_adjusted_volume(&unbalanced, 10, 100).unwrap();
        let expected_left = half + 6554;
        assert_eq!(
            raised.get(),
            &[Volume(expected_left), Volume(expected_left / 2)]
        );

        let restored = compute_adjusted_volume(&raised, -10, 100).unwrap();
        assert_eq!(restored.get(), &[Volume(half), Volume(quarter)]);

        // Multi-channel balance (including a muted channel) scales proportionally.
        let surround = make_cvolume(&[Volume::NORMAL.0, half, quarter, 0]);
        let halved = compute_adjusted_volume(&surround, -50, 100).unwrap();
        assert_eq!(
            halved.get(),
            &[
                Volume(half),
                Volume(quarter),
                Volume(quarter / 2),
                Volume(0)
            ]
        );

        // Raising from all-zero lifts all channels uniformly.
        let zeroed = make_cvolume(&[0, 0]);
        let lifted = compute_adjusted_volume(&zeroed, 5, 100).unwrap();
        assert_eq!(lifted.get(), &[Volume(3277), Volume(3277)]);
    }

    #[test]
    fn volume_math_respects_peak_limit_without_distorting_balance() {
        // Peak at 95%, right at 47.5%; +10% hits 100% cap while preserving 2:1 ratio.
        let near_cap = make_cvolume(&[62259, 31130]);
        let capped = compute_adjusted_volume(&near_cap, 10, 100).unwrap();
        assert_eq!(capped.get()[0], Volume::NORMAL);
        assert!((i64::from(capped.get()[1].0) - i64::from(Volume::NORMAL.0 / 2)).abs() <= 1);

        // Configurable boost above 100% (e.g. 150%) is respected.
        let boost_limit = percent_to_volume(150);
        assert_eq!(boost_limit.0, 98304);
        let stereo = make_cvolume(&[Volume::NORMAL.0, Volume::NORMAL.0 / 2]);
        let boosted = compute_adjusted_volume(&stereo, 60, 150).unwrap();
        assert_eq!(boosted.get(), &[Volume(98304), Volume(49152)]);

        // Existing volume above the configured peak limit is clamped back down proportionally.
        let over_limit = make_cvolume(&[98304, 49152]);
        let clamped_up = compute_adjusted_volume(&over_limit, 5, 100).unwrap();
        assert_eq!(
            clamped_up.get(),
            &[Volume::NORMAL, Volume(Volume::NORMAL.0 / 2)]
        );
        let clamped_zero = compute_adjusted_volume(&over_limit, 0, 100).unwrap();
        assert_eq!(
            clamped_zero.get(),
            &[Volume::NORMAL, Volume(Volume::NORMAL.0 / 2)]
        );
        let reduced_below_cap = compute_adjusted_volume(&over_limit, -60, 100).unwrap();
        assert_eq!(reduced_below_cap.get()[0].0, 98304 - 39322);
        assert_eq!(reduced_below_cap.get()[1].0, (98304 - 39322) / 2);

        // Zero max_volume clamps everything cleanly to muted.
        let zero_cap = compute_adjusted_volume(&stereo, 10, 0).unwrap();
        assert_eq!(zero_cap.get(), &[Volume::MUTED, Volume::MUTED]);
    }

    #[test]
    fn volume_math_handles_extreme_signed_deltas_and_clamps_without_overflow() {
        let stereo = make_cvolume(&[Volume::NORMAL.0 / 2, Volume::NORMAL.0 / 4]);

        let max_delta = compute_adjusted_volume(&stereo, i32::MAX, 100).unwrap();
        assert!(max_delta.is_valid());
        assert_eq!(
            max_delta.get(),
            &[Volume::NORMAL, Volume(Volume::NORMAL.0 / 2)]
        );

        let min_delta = compute_adjusted_volume(&stereo, i32::MIN, 100).unwrap();
        assert!(min_delta.is_valid());
        assert_eq!(min_delta.get(), &[Volume::MUTED, Volume::MUTED]);

        let max_limit = compute_adjusted_volume(&stereo, i32::MAX, u16::MAX).unwrap();
        assert!(max_limit.is_valid());
        assert_eq!(max_limit.get()[0], percent_to_volume(u16::MAX));
        assert!(max_limit.get()[0] <= Volume::MAX);

        let min_with_max_limit = compute_adjusted_volume(&stereo, i32::MIN, u16::MAX).unwrap();
        assert!(min_with_max_limit.is_valid());
        assert_eq!(min_with_max_limit.get(), &[Volume::MUTED, Volume::MUTED]);

        // Zero delta within limit is an exact identity.
        let unchanged = compute_adjusted_volume(&stereo, 0, 100).unwrap();
        assert_eq!(unchanged.get(), stereo.get());

        // Decreasing past 0 clamps cleanly to muted.
        let floor = compute_adjusted_volume(&make_cvolume(&[3277, 1638]), -10, 100).unwrap();
        assert_eq!(floor.get(), &[Volume::MUTED, Volume::MUTED]);
    }

    #[test]
    fn volume_math_rejects_empty_channel_volumes() {
        let empty = ChannelVolumes::default();
        assert!(compute_adjusted_volume(&empty, 5, 100).is_err());
    }
}
