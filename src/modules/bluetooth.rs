use crate::config::IconsConfig;
use crate::core::clipboard;
use crate::core::fuzzel::{self, Anchor, FuzzelOptions};
use crate::core::hyprland;
use crate::core::notify::{Notification, Urgency};
use crate::daemon::menu::{MenuKind, MenuSession};
use crate::daemon::AppContext;
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{debug, info, warn};
use zbus::fdo::ObjectManagerProxy;
use zbus::message::Type as MessageType;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

const BLUEZ_DEST: &str = "org.bluez";
const BLUEZ_ROOT_PATH: &str = "/";
const BLUEZ_AGENT_MANAGER_PATH: &str = "/org/bluez";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const BATTERY_IFACE: &str = "org.bluez.Battery1";
const AGENT_MANAGER_IFACE: &str = "org.bluez.AgentManager1";
const DBUS_PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const DBUS_OBJMGR_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
const DBUS_IFACE: &str = "org.freedesktop.DBus";

const AGENT_PATH: &str = "/org/fuzzyland/BluetoothAgent";
const AGENT_CAPABILITY: &str = "NoInputNoOutput";
const NOTIFY_APP: &str = "Bluetooth";
const NOTIFY_SYNC_ID: &str = "fuzzyland-bluetooth";

const RFKILL_DEVICE: &str = "/dev/rfkill";
const RFKILL_TYPE_BLUETOOTH: u8 = 2;
const RFKILL_OP_ADD: u8 = 0;
const RFKILL_OP_DEL: u8 = 1;
const RFKILL_OP_CHANGE: u8 = 2;
const RFKILL_OP_CHANGE_ALL: u8 = 3;

const SCAN_DURATION: Duration = Duration::from_secs(5);
const POWER_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PAIR_TIMEOUT: Duration = Duration::from_secs(20);
const ACTION_TIMEOUT: Duration = Duration::from_secs(8);

// ---------------------------------------------------------------------------
// Native /dev/rfkill ABI (Linux <linux/rfkill.h>)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct RfkillEvent {
    idx: u32,
    type_: u8,
    op: u8,
    soft: u8,
    hard: u8,
}

impl RfkillEvent {
    const SIZE: usize = 8;

    fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        let idx = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
        Some(Self {
            idx,
            type_: buf[4],
            op: buf[5],
            soft: buf[6],
            hard: buf[7],
        })
    }

    fn to_bytes(self) -> [u8; Self::SIZE] {
        let idx = self.idx.to_ne_bytes();
        [idx[0], idx[1], idx[2], idx[3], self.type_, self.op, self.soft, self.hard]
    }
}

fn aggregate_rfkill_bluetooth_events(events: &[RfkillEvent]) -> (bool, bool) {
    let mut devices: BTreeMap<u32, (bool, bool)> = BTreeMap::new();
    for ev in events {
        if ev.type_ != RFKILL_TYPE_BLUETOOTH {
            continue;
        }
        match ev.op {
            RFKILL_OP_ADD | RFKILL_OP_CHANGE => {
                devices.insert(ev.idx, (ev.soft != 0, ev.hard != 0));
            }
            RFKILL_OP_DEL => {
                devices.remove(&ev.idx);
            }
            _ => {}
        }
    }
    if devices.is_empty() {
        return (false, false);
    }
    let soft_blocked = devices.values().all(|(soft, _)| *soft);
    let hard_blocked = devices.values().all(|(_, hard)| *hard);
    (soft_blocked, hard_blocked)
}

/// Read current `(soft_blocked, hard_blocked)` state for Bluetooth via `/dev/rfkill`.
fn rfkill_state() -> (bool, bool) {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(RFKILL_DEVICE)
    {
        Ok(f) => f,
        Err(err) => {
            debug!("Could not open {RFKILL_DEVICE} for reading: {err}");
            return (false, false);
        }
    };

    let mut events = Vec::new();
    // Kernel 5.11+ may emit 9-byte rfkill_event_ext structs; read up to 32 bytes per event.
    let mut buf = [0u8; 32];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(ev) = RfkillEvent::from_bytes(&buf[..n]) {
                    events.push(ev);
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => break,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                debug!("Error reading {RFKILL_DEVICE}: {err}");
                break;
            }
        }
    }

    aggregate_rfkill_bluetooth_events(&events)
}

/// Soft-unblock all Bluetooth rfkill switches via `/dev/rfkill`.
fn rfkill_unblock_bluetooth() -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(RFKILL_DEVICE)
        .map_err(|e| anyhow!("Failed to open {RFKILL_DEVICE} for writing: {e}"))?;

    let ev = RfkillEvent {
        idx: 0,
        type_: RFKILL_TYPE_BLUETOOTH,
        op: RFKILL_OP_CHANGE_ALL,
        soft: 0,
        hard: 0,
    };
    file.write_all(&ev.to_bytes())
        .map_err(|e| anyhow!("Failed to write unblock event to {RFKILL_DEVICE}: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Embedded NoInputNoOutput BlueZ Agent1
// ---------------------------------------------------------------------------

struct NoInputNoOutputAgent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl NoInputNoOutputAgent {
    fn release(&self) {
        debug!("BlueZ Agent1 released");
    }

    fn request_pin_code(&self, device: OwnedObjectPath) -> zbus::fdo::Result<String> {
        debug!("BlueZ Agent1 RequestPinCode rejected for {device}");
        Err(zbus::fdo::Error::Failed(
            "PIN entry is not supported by NoInputNoOutput agent".to_string(),
        ))
    }

    fn display_pin_code(&self, device: OwnedObjectPath, _pincode: String) {
        debug!("BlueZ Agent1 DisplayPinCode for {device}");
    }

    fn request_passkey(&self, device: OwnedObjectPath) -> zbus::fdo::Result<u32> {
        debug!("BlueZ Agent1 RequestPasskey rejected for {device}");
        Err(zbus::fdo::Error::Failed(
            "Passkey entry is not supported by NoInputNoOutput agent".to_string(),
        ))
    }

    fn display_passkey(&self, device: OwnedObjectPath, _passkey: u32, _entered: u16) {
        debug!("BlueZ Agent1 DisplayPasskey for {device}");
    }

    fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> zbus::fdo::Result<()> {
        debug!("BlueZ Agent1 RequestConfirmation auto-accepted for {device} ({passkey:06})");
        Ok(())
    }

    fn request_authorization(&self, device: OwnedObjectPath) -> zbus::fdo::Result<()> {
        debug!("BlueZ Agent1 RequestAuthorization auto-accepted for {device}");
        Ok(())
    }

    fn authorize_service(&self, device: OwnedObjectPath, uuid: String) -> zbus::fdo::Result<()> {
        debug!("BlueZ Agent1 AuthorizeService auto-accepted for {device} ({uuid})");
        Ok(())
    }

    fn cancel(&self) {
        debug!("BlueZ Agent1 pairing cancelled");
    }
}

async fn register_agent(conn: &Connection) -> Result<()> {
    let agent_path = ObjectPath::try_from(AGENT_PATH)?;
    let _ = conn
        .object_server()
        .at(&agent_path, NoInputNoOutputAgent)
        .await;

    let res = conn
        .call_method(
            Some(BLUEZ_DEST),
            BLUEZ_AGENT_MANAGER_PATH,
            Some(AGENT_MANAGER_IFACE),
            "RegisterAgent",
            &(&agent_path, AGENT_CAPABILITY),
        )
        .await;

    match res {
        Ok(_) => {
            let _ = conn
                .call_method(
                    Some(BLUEZ_DEST),
                    BLUEZ_AGENT_MANAGER_PATH,
                    Some(AGENT_MANAGER_IFACE),
                    "RequestDefaultAgent",
                    &(&agent_path,),
                )
                .await;
            Ok(())
        }
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.bluez.Error.AlreadyExists" =>
        {
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

async fn unregister_agent(conn: &Connection) {
    if let Ok(agent_path) = ObjectPath::try_from(AGENT_PATH) {
        let _ = conn
            .call_method(
                Some(BLUEZ_DEST),
                BLUEZ_AGENT_MANAGER_PATH,
                Some(AGENT_MANAGER_IFACE),
                "UnregisterAgent",
                &(&agent_path,),
            )
            .await;
        let _ = conn
            .object_server()
            .remove::<NoInputNoOutputAgent, _>(&agent_path)
            .await;
    }
}

// ---------------------------------------------------------------------------
// State Model & Daemon Cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AdapterInfo {
    path: String,
    address: String,
    name: String,
    alias: String,
    powered: bool,
    discoverable: bool,
    pairable: bool,
    discovering: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DeviceInfo {
    path: String,
    adapter_path: String,
    address: String,
    name: String,
    alias: String,
    icon: String,
    paired: bool,
    bonded: bool,
    trusted: bool,
    blocked: bool,
    connected: bool,
    battery: Option<u8>,
}

#[derive(Debug, Clone, Default)]
struct BluetoothSnapshot {
    synced: bool,
    adapters: BTreeMap<String, AdapterInfo>,
    devices: BTreeMap<String, DeviceInfo>,
}

pub struct BluetoothState {
    snapshot: RwLock<BluetoothSnapshot>,
    agent_registered: AtomicBool,
}

impl BluetoothState {
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(BluetoothSnapshot::default()),
            agent_registered: AtomicBool::new(false),
        }
    }

    fn read_snapshot(&self) -> BluetoothSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn replace_snapshot(&self, snap: BluetoothSnapshot) {
        let mut guard = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        *guard = snap;
    }

    fn clear_snapshot(&self) {
        let mut guard = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        guard.adapters.clear();
        guard.devices.clear();
        guard.synced = false;
    }

    fn apply_interfaces_added(
        &self,
        path: &str,
        interfaces: &HashMap<String, HashMap<String, OwnedValue>>,
    ) {
        let mut guard = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        if let Some(props) = interfaces.get(ADAPTER_IFACE) {
            let entry = guard
                .adapters
                .entry(path.to_string())
                .or_insert_with(|| AdapterInfo {
                    path: path.to_string(),
                    alias: "Bluetooth".to_string(),
                    ..Default::default()
                });
            apply_adapter_props(entry, props);
        }
        if let Some(props) = interfaces.get(DEVICE_IFACE) {
            let entry = guard
                .devices
                .entry(path.to_string())
                .or_insert_with(|| DeviceInfo {
                    path: path.to_string(),
                    ..Default::default()
                });
            apply_device_props(entry, props);
        }
        if let Some(props) = interfaces.get(BATTERY_IFACE) {
            let entry = guard
                .devices
                .entry(path.to_string())
                .or_insert_with(|| DeviceInfo {
                    path: path.to_string(),
                    ..Default::default()
                });
            apply_battery_props(entry, props);
        }
    }

    fn apply_interfaces_removed(&self, path: &str, interfaces: &[String]) {
        let mut guard = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        for iface in interfaces {
            match iface.as_str() {
                ADAPTER_IFACE => {
                    guard.adapters.remove(path);
                }
                DEVICE_IFACE => {
                    guard.devices.remove(path);
                }
                BATTERY_IFACE => {
                    if let Some(dev) = guard.devices.get_mut(path) {
                        dev.battery = None;
                    }
                }
                _ => {}
            }
        }
    }

    fn apply_properties_changed(
        &self,
        path: &str,
        iface: &str,
        changed: &HashMap<String, OwnedValue>,
        invalidated: &[String],
    ) {
        let mut guard = self.snapshot.write().unwrap_or_else(|e| e.into_inner());
        match iface {
            ADAPTER_IFACE => {
                if let Some(adapter) = guard.adapters.get_mut(path) {
                    apply_adapter_props(adapter, changed);
                }
            }
            DEVICE_IFACE => {
                if let Some(dev) = guard.devices.get_mut(path) {
                    apply_device_props(dev, changed);
                }
            }
            BATTERY_IFACE => {
                if let Some(dev) = guard.devices.get_mut(path) {
                    apply_battery_props(dev, changed);
                    if invalidated.iter().any(|k| k == "Percentage") {
                        dev.battery = None;
                    }
                }
            }
            _ => {}
        }
    }
}

impl Default for BluetoothState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// BlueZ Property Decoding & ObjectManager Sync
// ---------------------------------------------------------------------------

fn value_as_bool(v: &OwnedValue) -> Option<bool> {
    match &**v {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn value_as_u8(v: &OwnedValue) -> Option<u8> {
    match &**v {
        Value::U8(n) => Some(*n),
        _ => None,
    }
}

fn value_as_string(v: &OwnedValue) -> Option<String> {
    match &**v {
        Value::Str(s) => Some(s.as_str().to_string()),
        _ => None,
    }
}

fn value_as_object_path(v: &OwnedValue) -> Option<String> {
    match &**v {
        Value::ObjectPath(p) => Some(p.as_str().to_string()),
        _ => None,
    }
}

fn apply_adapter_props(adapter: &mut AdapterInfo, props: &HashMap<String, OwnedValue>) {
    if let Some(v) = props.get("Address").and_then(value_as_string) {
        adapter.address = v;
    }
    if let Some(v) = props.get("Name").and_then(value_as_string) {
        adapter.name = v;
    }
    if let Some(v) = props.get("Alias").and_then(value_as_string) {
        if !v.is_empty() {
            adapter.alias = v;
        }
    }
    if let Some(v) = props.get("Powered").and_then(value_as_bool) {
        adapter.powered = v;
    }
    if let Some(v) = props.get("Discoverable").and_then(value_as_bool) {
        adapter.discoverable = v;
    }
    if let Some(v) = props.get("Pairable").and_then(value_as_bool) {
        adapter.pairable = v;
    }
    if let Some(v) = props.get("Discovering").and_then(value_as_bool) {
        adapter.discovering = v;
    }
}

fn apply_device_props(dev: &mut DeviceInfo, props: &HashMap<String, OwnedValue>) {
    if let Some(v) = props.get("Adapter").and_then(value_as_object_path) {
        dev.adapter_path = v;
    }
    if let Some(v) = props.get("Address").and_then(value_as_string) {
        dev.address = v;
    }
    if let Some(v) = props.get("Name").and_then(value_as_string) {
        dev.name = v;
    }
    if let Some(v) = props.get("Alias").and_then(value_as_string) {
        dev.alias = v;
    }
    if let Some(v) = props.get("Icon").and_then(value_as_string) {
        dev.icon = v;
    }
    if let Some(v) = props.get("Paired").and_then(value_as_bool) {
        dev.paired = v;
    }
    if let Some(v) = props.get("Bonded").and_then(value_as_bool) {
        dev.bonded = v;
    }
    if let Some(v) = props.get("Trusted").and_then(value_as_bool) {
        dev.trusted = v;
    }
    if let Some(v) = props.get("Blocked").and_then(value_as_bool) {
        dev.blocked = v;
    }
    if let Some(v) = props.get("Connected").and_then(value_as_bool) {
        dev.connected = v;
    }
    if dev.alias.is_empty() {
        if !dev.name.is_empty() {
            dev.alias = dev.name.clone();
        } else if !dev.address.is_empty() {
            dev.alias = dev.address.clone();
        }
    }
    if dev.name.is_empty() && !dev.alias.is_empty() {
        dev.name = dev.alias.clone();
    }
}

fn apply_battery_props(dev: &mut DeviceInfo, props: &HashMap<String, OwnedValue>) {
    if let Some(pct) = props.get("Percentage").and_then(value_as_u8) {
        dev.battery = Some(pct);
    }
}

async fn fetch_bluez_snapshot(conn: &Connection) -> Result<BluetoothSnapshot> {
    let proxy = ObjectManagerProxy::builder(conn)
        .destination(BLUEZ_DEST)?
        .path(BLUEZ_ROOT_PATH)?
        .build()
        .await?;

    let managed = match proxy.get_managed_objects().await {
        Ok(objs) => objs,
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("ServiceUnknown") || msg.contains("NameHasNoOwner") {
                debug!("BlueZ service not active on system bus: {err}");
                return Ok(BluetoothSnapshot {
                    synced: true,
                    adapters: BTreeMap::new(),
                    devices: BTreeMap::new(),
                });
            }
            return Err(err.into());
        }
    };

    let mut snap = BluetoothSnapshot {
        synced: true,
        adapters: BTreeMap::new(),
        devices: BTreeMap::new(),
    };

    for (obj_path, ifaces) in managed {
        let path = obj_path.as_str().to_string();
        for (iface_name, props) in ifaces {
            match iface_name.as_str() {
                ADAPTER_IFACE => {
                    let entry = snap
                        .adapters
                        .entry(path.clone())
                        .or_insert_with(|| AdapterInfo {
                            path: path.clone(),
                            alias: "Bluetooth".to_string(),
                            ..Default::default()
                        });
                    apply_adapter_props(entry, &props);
                }
                DEVICE_IFACE => {
                    let entry = snap
                        .devices
                        .entry(path.clone())
                        .or_insert_with(|| DeviceInfo {
                            path: path.clone(),
                            ..Default::default()
                        });
                    apply_device_props(entry, &props);
                }
                BATTERY_IFACE => {
                    let entry = snap
                        .devices
                        .entry(path.clone())
                        .or_insert_with(|| DeviceInfo {
                            path: path.clone(),
                            ..Default::default()
                        });
                    apply_battery_props(entry, &props);
                }
                _ => {}
            }
        }
    }

    // Drop any orphan Battery1 entries without a real Device1 Address.
    snap.devices.retain(|_, dev| !dev.address.is_empty());
    Ok(snap)
}

async fn get_snapshot(ctx: &AppContext, force_refresh: bool) -> Result<BluetoothSnapshot> {
    if ctx.is_daemon && !force_refresh {
        let cached = ctx.bluetooth.read_snapshot();
        if cached.synced {
            return Ok(cached);
        }
    }

    let conn = ctx.system_bus().await?;
    let snap = fetch_bluez_snapshot(conn).await?;
    ctx.bluetooth.replace_snapshot(snap.clone());
    Ok(snap)
}

// ---------------------------------------------------------------------------
// Background Daemon Watcher
// ---------------------------------------------------------------------------

pub fn spawn_background(ctx: Arc<AppContext>) {
    tokio::spawn(async move {
        if let Err(err) = run_background_watcher(ctx).await {
            warn!("Bluetooth background watcher stopped: {err:#}");
        }
    });
}

async fn run_background_watcher(ctx: Arc<AppContext>) -> Result<()> {
    let conn = ctx.system_bus().await?;

    // Subscribe to signals before initial GetManagedObjects so no events are missed.
    let objmgr_rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(BLUEZ_DEST)?
        .interface(DBUS_OBJMGR_IFACE)?
        .build();
    let mut objmgr_stream = MessageStream::for_match_rule(objmgr_rule, conn, None).await?;

    let props_rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(BLUEZ_DEST)?
        .interface(DBUS_PROPS_IFACE)?
        .member("PropertiesChanged")?
        .build();
    let mut props_stream = MessageStream::for_match_rule(props_rule, conn, None).await?;

    let owner_rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(DBUS_IFACE)?
        .interface(DBUS_IFACE)?
        .member("NameOwnerChanged")?
        .arg(0, BLUEZ_DEST)?
        .build();
    let mut owner_stream = MessageStream::for_match_rule(owner_rule, conn, None).await?;

    // Initial sync and Agent1 registration.
    match fetch_bluez_snapshot(conn).await {
        Ok(snap) => {
            debug!(
                "Bluetooth cache initialized ({} adapters, {} devices)",
                snap.adapters.len(),
                snap.devices.len()
            );
            ctx.bluetooth.replace_snapshot(snap);
        }
        Err(err) => {
            warn!("Initial BlueZ GetManagedObjects failed: {err}");
        }
    }

    match register_agent(conn).await {
        Ok(()) => {
            ctx.bluetooth.agent_registered.store(true, Ordering::Relaxed);
            info!("Registered BlueZ NoInputNoOutput agent at {AGENT_PATH}");
        }
        Err(err) => {
            debug!("Could not register BlueZ agent at startup: {err}");
        }
    }

    loop {
        tokio::select! {
            maybe_msg = objmgr_stream.next() => {
                let Some(msg_res) = maybe_msg else { break };
                if let Ok(msg) = msg_res {
                    handle_object_manager_signal(&ctx.bluetooth, &msg);
                }
            }
            maybe_msg = props_stream.next() => {
                let Some(msg_res) = maybe_msg else { break };
                if let Ok(msg) = msg_res {
                    handle_properties_changed_signal(&ctx.bluetooth, &msg);
                }
            }
            maybe_msg = owner_stream.next() => {
                let Some(msg_res) = maybe_msg else { break };
                if let Ok(msg) = msg_res {
                    if let Ok((_name, _old_owner, new_owner)) =
                        msg.body().deserialize::<(String, String, String)>()
                    {
                        if new_owner.is_empty() {
                            debug!("BlueZ left the system bus; clearing Bluetooth cache");
                            ctx.bluetooth.clear_snapshot();
                            ctx.bluetooth.agent_registered.store(false, Ordering::Relaxed);
                        } else {
                            debug!("BlueZ appeared on system bus; re-syncing Bluetooth cache");
                            if let Ok(snap) = fetch_bluez_snapshot(conn).await {
                                ctx.bluetooth.replace_snapshot(snap);
                            }
                            if register_agent(conn).await.is_ok() {
                                ctx.bluetooth.agent_registered.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn handle_object_manager_signal(state: &BluetoothState, msg: &zbus::Message) {
    let header = msg.header();
    let Some(member) = header.member() else {
        return;
    };
    match member.as_str() {
        "InterfacesAdded" => {
            if let Ok((obj_path, interfaces)) = msg
                .body()
                .deserialize::<(OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>)>()
            {
                state.apply_interfaces_added(obj_path.as_str(), &interfaces);
            }
        }
        "InterfacesRemoved" => {
            if let Ok((obj_path, interfaces)) =
                msg.body().deserialize::<(OwnedObjectPath, Vec<String>)>()
            {
                state.apply_interfaces_removed(obj_path.as_str(), &interfaces);
            }
        }
        _ => {}
    }
}

fn handle_properties_changed_signal(state: &BluetoothState, msg: &zbus::Message) {
    let header = msg.header();
    let Some(path) = header.path() else {
        return;
    };
    if let Ok((iface, changed, invalidated)) = msg
        .body()
        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
    {
        state.apply_properties_changed(path.as_str(), &iface, &changed, &invalidated);
    }
}

// ---------------------------------------------------------------------------
// Pure Helpers: Adapter Selection, Filtering, Sorting, Icons, Formatting
// ---------------------------------------------------------------------------

fn select_default_adapter(snap: &BluetoothSnapshot) -> Option<AdapterInfo> {
    snap.adapters
        .values()
        .find(|a| a.powered)
        .or_else(|| snap.adapters.values().next())
        .cloned()
}

/// Returns `true` if `s` looks like a MAC address (`AA:BB:CC:DD:EE:FF` or `AA-BB-CC-DD-EE-FF`).
fn is_mac_address(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 17 {
        return false;
    }
    for (idx, &b) in bytes.iter().enumerate() {
        if idx % 3 == 2 {
            if b != b':' && b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn collect_devices(
    snap: &BluetoothSnapshot,
    adapter_path: &str,
    include_unpaired: bool,
) -> Vec<DeviceInfo> {
    let mut devices: Vec<DeviceInfo> = snap
        .devices
        .values()
        .filter(|dev| {
            if !dev.adapter_path.is_empty() {
                dev.adapter_path == adapter_path
            } else {
                dev.path.starts_with(adapter_path)
            }
        })
        .filter(|dev| {
            if dev.connected || dev.paired {
                true
            } else if include_unpaired {
                let trimmed = dev.alias.trim();
                !trimmed.is_empty() && !is_mac_address(trimmed)
            } else {
                false
            }
        })
        .cloned()
        .collect();

    sort_devices(&mut devices);
    devices
}

fn sort_devices(devices: &mut [DeviceInfo]) {
    devices.sort_by(|a, b| {
        (!a.connected)
            .cmp(&!b.connected)
            .then_with(|| (!a.paired).cmp(&!b.paired))
            .then_with(|| a.alias.to_ascii_lowercase().cmp(&b.alias.to_ascii_lowercase()))
            .then_with(|| a.address.cmp(&b.address))
    });
}

fn resolve_device_icon<'a>(icon_name: &str, alias: &str, icons: &'a IconsConfig) -> &'a str {
    match icon_name.to_ascii_lowercase().as_str() {
        "audio-headset" => return &icons.bt_headset,
        "audio-headphones" => return &icons.bt_headphones,
        "audio-card" | "audio-speakers" | "multimedia-player" => return &icons.bt_speaker,
        "input-keyboard" => return &icons.bt_keyboard,
        "input-mouse" => return &icons.bt_mouse,
        "input-gaming" => return &icons.bt_gamepad,
        "phone" => return &icons.bt_phone,
        "computer" => return &icons.bt_computer,
        "camera-photo" | "camera-video" => return &icons.bt_camera,
        _ => {}
    }

    let lower = alias.to_ascii_lowercase();
    if ["headset", "airpods", "buds", "earbuds", "freebuds"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return &icons.bt_headset;
    }
    if ["headphone", "wh-", "qc", "momentum", "beats", "bose", "sony"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return &icons.bt_headphones;
    }
    if ["speaker", "jbl", "soundlink", "marshall", "sonos"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return &icons.bt_speaker;
    }
    if ["keyboard", "keychron", "hhkb", "nuphy"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return &icons.bt_keyboard;
    }
    if ["mouse", "mx master", "mx anywhere", "trackpad"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return &icons.bt_mouse;
    }
    if ["controller", "dualsense", "xbox", "joy-con", "gamepad"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return &icons.bt_gamepad;
    }

    &icons.bluetooth
}

fn format_device_label(dev: &DeviceInfo, icons: &IconsConfig) -> String {
    let icon = resolve_device_icon(&dev.icon, &dev.alias, icons);
    let bat_str = dev
        .battery
        .map(|pct| format!(" ({pct}%)"))
        .unwrap_or_default();

    if dev.connected {
        format!("{icon}  {}{bat_str}  {}", dev.alias, icons.check)
    } else if !dev.paired {
        format!("{icon}  {} (New)", dev.alias)
    } else {
        format!("{icon}  {}", dev.alias)
    }
}

// ---------------------------------------------------------------------------
// BlueZ D-Bus Operations & Notifications
// ---------------------------------------------------------------------------

async fn notify(ctx: &AppContext, summary: &str, body: &str, urgency: Urgency) {
    let mut n = Notification::new(NOTIFY_APP, summary)
        .urgency(urgency)
        .sync_id(NOTIFY_SYNC_ID);
    if !body.is_empty() {
        n = n.body(body);
    }
    ctx.notifier.send_best_effort(&n).await;
}

fn format_zbus_error(err: &zbus::Error, fallback: &str) -> String {
    match err {
        zbus::Error::MethodError(name, detail, _) => {
            if let Some(msg) = detail {
                let trimmed = msg.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
            name.as_str()
                .strip_prefix("org.bluez.Error.")
                .unwrap_or(name.as_str())
                .to_string()
        }
        other => {
            let text = other.to_string();
            if text.is_empty() {
                fallback.to_string()
            } else {
                text
            }
        }
    }
}

fn is_already_exists_error(err: &zbus::Error) -> bool {
    matches!(
        err,
        zbus::Error::MethodError(name, _, _) if name.as_str() == "org.bluez.Error.AlreadyExists"
    )
}

fn is_in_progress_error(err: &zbus::Error) -> bool {
    matches!(
        err,
        zbus::Error::MethodError(name, _, _) if name.as_str() == "org.bluez.Error.InProgress"
    )
}

async fn set_dbus_property(
    conn: &Connection,
    path: &str,
    iface: &str,
    prop: &str,
    value: Value<'_>,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let body = (iface, prop, value);
    let fut = conn.call_method(
        Some(BLUEZ_DEST),
        path,
        Some(DBUS_PROPS_IFACE),
        "Set",
        &body,
    );
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(format_zbus_error(&err, "D-Bus property update failed")),
        Err(_) => Err("Bluetooth operation timed out".to_string()),
    }
}

async fn call_device_method(
    conn: &Connection,
    device_path: &str,
    method: &str,
    timeout: Duration,
) -> std::result::Result<(), zbus::Error> {
    let fut = conn.call_method(
        Some(BLUEZ_DEST),
        device_path,
        Some(DEVICE_IFACE),
        method,
        &(),
    );
    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res.map(|_| ()),
        Err(_) => Err(zbus::Error::Failure("Operation timed out".to_string())),
    }
}

async fn set_power(ctx: &AppContext, enable: bool) -> std::result::Result<(), String> {
    let conn = ctx
        .system_bus()
        .await
        .map_err(|e| format!("System D-Bus unavailable: {e}"))?;

    if enable {
        let (soft_blocked, hard_blocked) = rfkill_state();
        if hard_blocked {
            return Err("Bluetooth is blocked by a hardware switch.".to_string());
        }
        if soft_blocked {
            if let Err(err) = rfkill_unblock_bluetooth() {
                warn!("Could not unblock Bluetooth via {RFKILL_DEVICE}: {err}");
            }
        }
    }

    let mut adapter = get_snapshot(ctx, false)
        .await
        .ok()
        .and_then(|s| select_default_adapter(&s));

    // After rfkill unblock, BlueZ may take a short moment to expose the HCI adapter.
    if enable && adapter.is_none() {
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(snap) = get_snapshot(ctx, true).await {
                if let Some(found) = select_default_adapter(&snap) {
                    adapter = Some(found);
                    break;
                }
            }
        }
    }

    let Some(adapter) = adapter else {
        return Err("No Bluetooth controller found".to_string());
    };

    set_dbus_property(
        conn,
        &adapter.path,
        ADAPTER_IFACE,
        "Powered",
        Value::Bool(enable),
        POWER_TIMEOUT,
    )
    .await
}

async fn connect_device(ctx: &AppContext, dev: &DeviceInfo) {
    notify(ctx, "Connecting...", &dev.alias, Urgency::Low).await;
    let Ok(conn) = ctx.system_bus().await else {
        notify(
            ctx,
            &format!("Failed to connect to {}", dev.alias),
            "System D-Bus unavailable",
            Urgency::Critical,
        )
        .await;
        return;
    };

    match call_device_method(conn, &dev.path, "Connect", CONNECT_TIMEOUT).await {
        Ok(()) => notify(ctx, "Bluetooth connected", &dev.alias, Urgency::Low).await,
        Err(err) => {
            notify(
                ctx,
                &format!("Failed to connect to {}", dev.alias),
                &format_zbus_error(&err, "Connection failed"),
                Urgency::Critical,
            )
            .await;
        }
    }
}

async fn disconnect_device(ctx: &AppContext, dev: &DeviceInfo) {
    let Ok(conn) = ctx.system_bus().await else {
        notify(
            ctx,
            &format!("Failed to disconnect {}", dev.alias),
            "System D-Bus unavailable",
            Urgency::Critical,
        )
        .await;
        return;
    };

    match call_device_method(conn, &dev.path, "Disconnect", DISCONNECT_TIMEOUT).await {
        Ok(()) => notify(ctx, "Bluetooth disconnected", &dev.alias, Urgency::Low).await,
        Err(err) => {
            notify(
                ctx,
                &format!("Failed to disconnect {}", dev.alias),
                &format_zbus_error(&err, "Disconnect failed"),
                Urgency::Critical,
            )
            .await;
        }
    }
}

async fn pair_and_connect(ctx: &AppContext, dev: &DeviceInfo) {
    notify(ctx, "Pairing...", &dev.alias, Urgency::Low).await;
    let Ok(conn) = ctx.system_bus().await else {
        notify(
            ctx,
            &format!("Failed to pair with {}", dev.alias),
            "System D-Bus unavailable",
            Urgency::Critical,
        )
        .await;
        return;
    };

    let mut temp_agent = false;
    if ctx.is_daemon {
        if !ctx.bluetooth.agent_registered.load(Ordering::Relaxed)
            && register_agent(conn).await.is_ok()
        {
            ctx.bluetooth.agent_registered.store(true, Ordering::Relaxed);
        }
    } else if register_agent(conn).await.is_ok() {
        temp_agent = true;
    }

    let pair_res = call_device_method(conn, &dev.path, "Pair", PAIR_TIMEOUT).await;
    let paired_ok = match &pair_res {
        Ok(()) => true,
        Err(err) if is_already_exists_error(err) => true,
        Err(_) => false,
    };

    if !paired_ok {
        let err_msg = pair_res
            .as_ref()
            .err()
            .map(|e| format_zbus_error(e, "Pairing failed"))
            .unwrap_or_else(|| "Pairing failed".to_string());
        if temp_agent {
            unregister_agent(conn).await;
        }
        notify(
            ctx,
            &format!("Failed to pair with {}", dev.alias),
            &err_msg,
            Urgency::Critical,
        )
        .await;
        return;
    }

    let _ = set_dbus_property(
        conn,
        &dev.path,
        DEVICE_IFACE,
        "Trusted",
        Value::Bool(true),
        ACTION_TIMEOUT,
    )
    .await;

    let conn_res = call_device_method(conn, &dev.path, "Connect", CONNECT_TIMEOUT).await;
    if temp_agent {
        unregister_agent(conn).await;
    }

    match conn_res {
        Ok(()) => notify(ctx, "Paired & connected", &dev.alias, Urgency::Low).await,
        Err(_) => {
            notify(
                ctx,
                &format!("Paired with {}", dev.alias),
                "Could not connect automatically",
                Urgency::Normal,
            )
            .await;
        }
    }
}

async fn remove_device(ctx: &AppContext, adapter_path: &str, dev: &DeviceInfo) {
    let Ok(conn) = ctx.system_bus().await else {
        notify(
            ctx,
            &format!("Failed to forget {}", dev.alias),
            "System D-Bus unavailable",
            Urgency::Critical,
        )
        .await;
        return;
    };

    let Ok(obj_path) = ObjectPath::try_from(dev.path.as_str()) else {
        notify(
            ctx,
            &format!("Failed to forget {}", dev.alias),
            "Invalid device object path",
            Urgency::Critical,
        )
        .await;
        return;
    };

    let target_adapter = if !dev.adapter_path.is_empty() {
        dev.adapter_path.as_str()
    } else {
        adapter_path
    };

    let body = (&obj_path,);
    let fut = conn.call_method(
        Some(BLUEZ_DEST),
        target_adapter,
        Some(ADAPTER_IFACE),
        "RemoveDevice",
        &body,
    );
    match tokio::time::timeout(ACTION_TIMEOUT, fut).await {
        Ok(Ok(_)) => notify(ctx, "Device forgotten", &dev.alias, Urgency::Low).await,
        Ok(Err(err)) => {
            notify(
                ctx,
                &format!("Failed to forget {}", dev.alias),
                &format_zbus_error(&err, "Remove failed"),
                Urgency::Critical,
            )
            .await;
        }
        Err(_) => {
            notify(
                ctx,
                &format!("Failed to forget {}", dev.alias),
                "Remove timed out",
                Urgency::Critical,
            )
            .await;
        }
    }
}

async fn scan_devices(
    ctx: &AppContext,
    adapter_path: &str,
    guard: &crate::daemon::menu::MenuGuard,
) {
    notify(
        ctx,
        "Scanning for Bluetooth devices...",
        "Looking for nearby devices (5s)",
        Urgency::Low,
    )
    .await;

    let Ok(conn) = ctx.system_bus().await else {
        return;
    };

    let started = match conn
        .call_method(
            Some(BLUEZ_DEST),
            adapter_path,
            Some(ADAPTER_IFACE),
            "StartDiscovery",
            &(),
        )
        .await
    {
        Ok(_) => true,
        Err(err) if is_in_progress_error(&err) => false,
        Err(err) => {
            warn!("StartDiscovery failed: {err}");
            false
        }
    };

    // Wait for scan duration while respecting menu cancellation/superseding.
    let step = Duration::from_millis(250);
    let mut elapsed = Duration::ZERO;
    while elapsed < SCAN_DURATION && guard.is_current() {
        let remaining = SCAN_DURATION.saturating_sub(elapsed);
        let sleep_for = remaining.min(step);
        tokio::time::sleep(sleep_for).await;
        elapsed += sleep_for;
    }

    if started {
        let _ = conn
            .call_method(
                Some(BLUEZ_DEST),
                adapter_path,
                Some(ADAPTER_IFACE),
                "StopDiscovery",
                &(),
            )
            .await;
    }

    // Ensure freshly discovered devices are synced into the snapshot.
    let _ = get_snapshot(ctx, true).await;
}

// ---------------------------------------------------------------------------
// Public Entrypoints: toggle_power & run_menu
// ---------------------------------------------------------------------------

pub async fn toggle_power(ctx: &AppContext) -> Result<()> {
    let snap = get_snapshot(ctx, false).await?;
    let ctrl = select_default_adapter(&snap);

    let Some(ctrl) = ctrl else {
        let (soft_blocked, hard_blocked) = rfkill_state();
        if hard_blocked {
            notify(
                ctx,
                "Bluetooth unavailable",
                "Blocked by hardware switch",
                Urgency::Critical,
            )
            .await;
            return Ok(());
        }
        if soft_blocked {
            match set_power(ctx, true).await {
                Ok(()) => notify(ctx, "Bluetooth enabled", "", Urgency::Low).await,
                Err(err) => {
                    notify(ctx, "Failed to enable Bluetooth", &err, Urgency::Critical).await;
                }
            }
            return Ok(());
        }
        notify(
            ctx,
            "Bluetooth unavailable",
            "No Bluetooth controller found",
            Urgency::Critical,
        )
        .await;
        return Ok(());
    };

    let target = !ctrl.powered;
    match set_power(ctx, target).await {
        Ok(()) => {
            notify(
                ctx,
                if target {
                    "Bluetooth enabled"
                } else {
                    "Bluetooth disabled"
                },
                "",
                Urgency::Low,
            )
            .await;
        }
        Err(err) => {
            notify(
                ctx,
                if target {
                    "Failed to enable Bluetooth"
                } else {
                    "Failed to disable Bluetooth"
                },
                &err,
                Urgency::Critical,
            )
            .await;
        }
    }
    Ok(())
}

enum MenuScreen {
    Main { show_discovered: bool },
    ManagePaired { show_discovered: bool },
    DeviceActions { device_path: String, show_discovered: bool },
    DeviceInfo { device_path: String, show_discovered: bool },
}

enum MainAction {
    Noop,
    PowerOn,
    PowerOff,
    Device(DeviceInfo),
    Scan,
    ManagePaired,
    SetDiscoverable(bool),
}

enum ManageAction {
    Device(String),
    Back,
}

enum DeviceAction {
    Connect,
    Disconnect,
    Info,
    SetTrusted(bool),
    Forget,
    Back,
}

enum InfoAction {
    Copy(String, String),
    Back,
}

fn base_fuzzel_opts(
    output: &Option<String>,
    prompt: String,
    placeholder: &str,
    max_lines: usize,
) -> FuzzelOptions {
    FuzzelOptions {
        prompt,
        placeholder: Some(placeholder.to_string()),
        anchor: Anchor::TopRight,
        width: Some(38),
        max_lines: Some(max_lines),
        minimal_lines: true,
        output: output.clone(),
        ..Default::default()
    }
}

pub async fn run_menu(ctx: &AppContext) -> Result<()> {
    let guard = match ctx.menu.begin_session(MenuKind::Bluetooth) {
        MenuSession::ToggledOff => return Ok(()),
        MenuSession::Acquired(guard) => guard,
    };

    let output = hyprland::detect_cursor_output().await;
    let icons = &ctx.config.icons;
    let mut screen = MenuScreen::Main {
        show_discovered: false,
    };

    while guard.is_current() {
        match screen {
            MenuScreen::Main { show_discovered } => {
                let snap = get_snapshot(ctx, false).await?;
                let ctrl = select_default_adapter(&snap);

                let mut entries: Vec<(String, MainAction)> = Vec::new();
                let mut adapter_path = String::new();

                match ctrl {
                    None => {
                        let (soft_blocked, hard_blocked) = rfkill_state();
                        if hard_blocked {
                            entries.push((
                                format!(
                                    "{}  Bluetooth blocked by hardware switch",
                                    icons.bluetooth_off
                                ),
                                MainAction::Noop,
                            ));
                        } else if soft_blocked {
                            entries.push((
                                format!("{}  Enable Bluetooth", icons.bluetooth),
                                MainAction::PowerOn,
                            ));
                        } else {
                            entries.push((
                                format!("{}  No Bluetooth controller found", icons.bluetooth_off),
                                MainAction::Noop,
                            ));
                        }
                    }
                    Some(adapter) if !adapter.powered => {
                        entries.push((
                            format!("{}  Enable Bluetooth", icons.bluetooth),
                            MainAction::PowerOn,
                        ));
                    }
                    Some(adapter) => {
                        adapter_path = adapter.path.clone();
                        let devices = collect_devices(&snap, &adapter.path, show_discovered);
                        let paired_count = devices.iter().filter(|d| d.paired).count();

                        for dev in devices {
                            let label = format_device_label(&dev, icons);
                            entries.push((label, MainAction::Device(dev)));
                        }

                        entries.push((
                            format!("{}  Scan for Devices (5s)", icons.refresh),
                            MainAction::Scan,
                        ));

                        if paired_count > 0 {
                            entries.push((
                                format!(
                                    "{}  Manage Paired Devices ({paired_count})...",
                                    icons.settings
                                ),
                                MainAction::ManagePaired,
                            ));
                        }

                        if adapter.discoverable {
                            entries.push((
                                format!("{}  Discoverable: On", icons.toggle_on),
                                MainAction::SetDiscoverable(false),
                            ));
                        } else {
                            entries.push((
                                format!("{}  Discoverable: Off", icons.toggle_off),
                                MainAction::SetDiscoverable(true),
                            ));
                        }

                        entries.push((
                            format!("{}  Disable Bluetooth", icons.bluetooth_off),
                            MainAction::PowerOff,
                        ));
                    }
                }

                let opts = base_fuzzel_opts(
                    &output,
                    format!("{}  ", icons.bluetooth),
                    "Search devices or actions...",
                    16,
                );
                let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await?
                else {
                    break;
                };

                match &entries[idx].1 {
                    MainAction::Noop => break,
                    MainAction::PowerOn => {
                        match set_power(ctx, true).await {
                            Ok(()) => notify(ctx, "Bluetooth enabled", "", Urgency::Low).await,
                            Err(err) => {
                                notify(ctx, "Failed to enable Bluetooth", &err, Urgency::Critical)
                                    .await;
                            }
                        }
                        break;
                    }
                    MainAction::PowerOff => {
                        match set_power(ctx, false).await {
                            Ok(()) => notify(ctx, "Bluetooth disabled", "", Urgency::Low).await,
                            Err(err) => {
                                notify(ctx, "Failed to disable Bluetooth", &err, Urgency::Critical)
                                    .await;
                            }
                        }
                        break;
                    }
                    MainAction::Device(dev) => {
                        let dev = dev.clone();
                        if dev.connected {
                            screen = MenuScreen::DeviceActions {
                                device_path: dev.path,
                                show_discovered,
                            };
                        } else if dev.paired {
                            connect_device(ctx, &dev).await;
                            break;
                        } else {
                            pair_and_connect(ctx, &dev).await;
                            break;
                        }
                    }
                    MainAction::Scan => {
                        scan_devices(ctx, &adapter_path, &guard).await;
                        screen = MenuScreen::Main {
                            show_discovered: true,
                        };
                    }
                    MainAction::ManagePaired => {
                        screen = MenuScreen::ManagePaired { show_discovered };
                    }
                    MainAction::SetDiscoverable(enable) => {
                        let enable = *enable;
                        match ctx.system_bus().await {
                            Ok(conn) => {
                                match set_dbus_property(
                                    conn,
                                    &adapter_path,
                                    ADAPTER_IFACE,
                                    "Discoverable",
                                    Value::Bool(enable),
                                    ACTION_TIMEOUT,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        if enable {
                                            notify(
                                                ctx,
                                                "Bluetooth discoverable",
                                                "Visible to nearby devices",
                                                Urgency::Low,
                                            )
                                            .await;
                                        } else {
                                            notify(
                                                ctx,
                                                "Bluetooth hidden",
                                                "No longer discoverable",
                                                Urgency::Low,
                                            )
                                            .await;
                                        }
                                    }
                                    Err(err) => {
                                        notify(
                                            ctx,
                                            if enable {
                                                "Failed to enable discoverable"
                                            } else {
                                                "Failed to disable discoverable"
                                            },
                                            &err,
                                            Urgency::Critical,
                                        )
                                        .await;
                                    }
                                }
                            }
                            Err(err) => {
                                notify(
                                    ctx,
                                    if enable {
                                        "Failed to enable discoverable"
                                    } else {
                                        "Failed to disable discoverable"
                                    },
                                    &err.to_string(),
                                    Urgency::Critical,
                                )
                                .await;
                            }
                        }
                        break;
                    }
                }
            }

            MenuScreen::ManagePaired { show_discovered } => {
                let snap = get_snapshot(ctx, false).await?;
                let Some(adapter) = select_default_adapter(&snap) else {
                    screen = MenuScreen::Main { show_discovered };
                    continue;
                };

                let devices = collect_devices(&snap, &adapter.path, false);
                let mut entries: Vec<(String, ManageAction)> = devices
                    .into_iter()
                    .filter(|d| d.paired)
                    .map(|d| {
                        let label = format_device_label(&d, icons);
                        (label, ManageAction::Device(d.path))
                    })
                    .collect();
                entries.push((format!("{}  Back", icons.back), ManageAction::Back));

                let opts = base_fuzzel_opts(
                    &output,
                    format!("{}  ", icons.settings),
                    "Select paired device...",
                    14,
                );
                let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await?
                else {
                    break;
                };

                match &entries[idx].1 {
                    ManageAction::Device(path) => {
                        screen = MenuScreen::DeviceActions {
                            device_path: path.clone(),
                            show_discovered,
                        };
                    }
                    ManageAction::Back => {
                        screen = MenuScreen::Main { show_discovered };
                    }
                }
            }

            MenuScreen::DeviceActions {
                device_path,
                show_discovered,
            } => {
                let snap = get_snapshot(ctx, false).await?;
                let adapter_path = select_default_adapter(&snap)
                    .map(|a| a.path)
                    .unwrap_or_default();
                let Some(dev) = snap.devices.get(&device_path).cloned() else {
                    screen = MenuScreen::Main { show_discovered };
                    continue;
                };

                let icon = resolve_device_icon(&dev.icon, &dev.alias, icons);
                let mut entries: Vec<(String, DeviceAction)> = Vec::new();
                if dev.connected {
                    entries.push((
                        format!("{}  Disconnect", icons.bluetooth_off),
                        DeviceAction::Disconnect,
                    ));
                } else {
                    entries.push((
                        format!("{}  Connect", icons.bluetooth_connected),
                        DeviceAction::Connect,
                    ));
                }
                entries.push((
                    format!("{}  Device Information", icons.info),
                    DeviceAction::Info,
                ));
                if dev.trusted {
                    entries.push((
                        format!("{}  Untrust Device", icons.toggle_off),
                        DeviceAction::SetTrusted(false),
                    ));
                } else {
                    entries.push((
                        format!("{}  Trust Device", icons.toggle_on),
                        DeviceAction::SetTrusted(true),
                    ));
                }
                entries.push((
                    format!("{}  Forget Device", icons.trash),
                    DeviceAction::Forget,
                ));
                entries.push((format!("{}  Back", icons.back), DeviceAction::Back));

                let opts = base_fuzzel_opts(
                    &output,
                    format!("{icon}  {} ", dev.alias),
                    "Choose action...",
                    8,
                );
                let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await?
                else {
                    break;
                };

                match &entries[idx].1 {
                    DeviceAction::Connect => {
                        connect_device(ctx, &dev).await;
                        break;
                    }
                    DeviceAction::Disconnect => {
                        disconnect_device(ctx, &dev).await;
                        break;
                    }
                    DeviceAction::Info => {
                        screen = MenuScreen::DeviceInfo {
                            device_path,
                            show_discovered,
                        };
                    }
                    DeviceAction::SetTrusted(trusted) => {
                        let trusted = *trusted;
                        match ctx.system_bus().await {
                            Ok(conn) => {
                                match set_dbus_property(
                                    conn,
                                    &dev.path,
                                    DEVICE_IFACE,
                                    "Trusted",
                                    Value::Bool(trusted),
                                    ACTION_TIMEOUT,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        notify(
                                            ctx,
                                            if trusted {
                                                "Device trusted"
                                            } else {
                                                "Device untrusted"
                                            },
                                            &dev.alias,
                                            Urgency::Low,
                                        )
                                        .await;
                                    }
                                    Err(err) => {
                                        notify(
                                            ctx,
                                            &format!(
                                                "Failed to {} {}",
                                                if trusted { "trust" } else { "untrust" },
                                                dev.alias
                                            ),
                                            &err,
                                            Urgency::Critical,
                                        )
                                        .await;
                                    }
                                }
                            }
                            Err(err) => {
                                notify(
                                    ctx,
                                    &format!(
                                        "Failed to {} {}",
                                        if trusted { "trust" } else { "untrust" },
                                        dev.alias
                                    ),
                                    &err.to_string(),
                                    Urgency::Critical,
                                )
                                .await;
                            }
                        }
                        break;
                    }
                    DeviceAction::Forget => {
                        let confirm_opts = base_fuzzel_opts(
                            &output,
                            format!("{}  ", icons.trash),
                            "Confirm...",
                            3,
                        );
                        let confirmed = fuzzel::confirm(
                            &ctx.config.fuzzel,
                            &guard,
                            &format!("{}  Yes, forget {}", icons.trash, dev.alias),
                            &format!("{}  Cancel", icons.cancel),
                            &confirm_opts,
                        )
                        .await?;
                        if confirmed {
                            remove_device(ctx, &adapter_path, &dev).await;
                        }
                        break;
                    }
                    DeviceAction::Back => {
                        screen = MenuScreen::Main { show_discovered };
                    }
                }
            }

            MenuScreen::DeviceInfo {
                device_path,
                show_discovered,
            } => {
                let snap = get_snapshot(ctx, true).await?;
                let Some(dev) = snap.devices.get(&device_path).cloned() else {
                    screen = MenuScreen::Main { show_discovered };
                    continue;
                };

                let mut rows: Vec<(String, String)> = vec![
                    ("Alias".to_string(), dev.alias.clone()),
                    ("Address".to_string(), dev.address.clone()),
                    (
                        "Status".to_string(),
                        if dev.connected {
                            "Connected".to_string()
                        } else {
                            "Disconnected".to_string()
                        },
                    ),
                    (
                        "Paired".to_string(),
                        if dev.paired { "Yes" } else { "No" }.to_string(),
                    ),
                    (
                        "Trusted".to_string(),
                        if dev.trusted { "Yes" } else { "No" }.to_string(),
                    ),
                ];
                if !dev.icon.is_empty() {
                    rows.push(("Type".to_string(), dev.icon.clone()));
                }
                if let Some(bat) = dev.battery {
                    rows.push(("Battery".to_string(), format!("{bat}%")));
                }

                let mut entries: Vec<(String, InfoAction)> = rows
                    .into_iter()
                    .map(|(k, v)| {
                        let label = format!("{}  {k}: {v}", icons.copy);
                        (label, InfoAction::Copy(k, v))
                    })
                    .collect();
                entries.push((format!("{}  Back", icons.back), InfoAction::Back));

                let opts = base_fuzzel_opts(
                    &output,
                    format!("{}  ", icons.info),
                    "Select item to copy...",
                    12,
                );
                let Some(idx) = fuzzel::select(&ctx.config.fuzzel, &guard, &entries, &opts).await?
                else {
                    break;
                };

                match &entries[idx].1 {
                    InfoAction::Copy(key, val) => {
                        match clipboard::copy_text(val).await {
                            Ok(()) => {
                                notify(
                                    ctx,
                                    "Copied to clipboard",
                                    &format!("{key}: {val}"),
                                    Urgency::Low,
                                )
                                .await;
                            }
                            Err(err) => {
                                notify(
                                    ctx,
                                    "Failed to copy to clipboard",
                                    &err.to_string(),
                                    Urgency::Critical,
                                )
                                .await;
                            }
                        }
                        break;
                    }
                    InfoAction::Back => {
                        screen = MenuScreen::DeviceActions {
                            device_path,
                            show_discovered,
                        };
                    }
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_device(
        path: &str,
        alias: &str,
        address: &str,
        connected: bool,
        paired: bool,
    ) -> DeviceInfo {
        DeviceInfo {
            path: path.to_string(),
            adapter_path: "/org/bluez/hci0".to_string(),
            address: address.to_string(),
            name: alias.to_string(),
            alias: alias.to_string(),
            icon: String::new(),
            paired,
            bonded: paired,
            trusted: paired,
            blocked: false,
            connected,
            battery: None,
        }
    }

    #[test]
    fn rfkill_event_layout_and_roundtrip() {
        assert_eq!(std::mem::size_of::<RfkillEvent>(), 8);
        assert_eq!(RFKILL_TYPE_BLUETOOTH, 2);
        assert_eq!(RFKILL_OP_CHANGE_ALL, 3);

        let ev = RfkillEvent {
            idx: 0x01020304,
            type_: RFKILL_TYPE_BLUETOOTH,
            op: RFKILL_OP_CHANGE_ALL,
            soft: 1,
            hard: 0,
        };
        let bytes = ev.to_bytes();
        assert_eq!(bytes.len(), 8);
        assert_eq!(bytes[4], RFKILL_TYPE_BLUETOOTH);
        assert_eq!(bytes[5], RFKILL_OP_CHANGE_ALL);
        assert_eq!(bytes[6], 1);
        assert_eq!(bytes[7], 0);

        let decoded = RfkillEvent::from_bytes(&bytes).expect("valid 8-byte event");
        assert_eq!(decoded, ev);

        // Extended 9-byte kernel event still parses cleanly.
        let mut ext = [0u8; 9];
        ext[..8].copy_from_slice(&bytes);
        ext[8] = 1;
        assert_eq!(RfkillEvent::from_bytes(&ext), Some(ev));
        assert_eq!(RfkillEvent::from_bytes(&bytes[..7]), None);
    }

    #[test]
    fn rfkill_event_aggregation() {
        // Empty => not blocked.
        assert_eq!(aggregate_rfkill_bluetooth_events(&[]), (false, false));

        // Non-Bluetooth events are ignored.
        let wlan_blocked = RfkillEvent {
            idx: 0,
            type_: 1, // WLAN
            op: RFKILL_OP_ADD,
            soft: 1,
            hard: 1,
        };
        assert_eq!(
            aggregate_rfkill_bluetooth_events(&[wlan_blocked]),
            (false, false)
        );

        // Single Bluetooth adapter soft-blocked, then unblocked.
        let bt_add_soft = RfkillEvent {
            idx: 1,
            type_: RFKILL_TYPE_BLUETOOTH,
            op: RFKILL_OP_ADD,
            soft: 1,
            hard: 0,
        };
        assert_eq!(
            aggregate_rfkill_bluetooth_events(&[bt_add_soft]),
            (true, false)
        );

        let bt_unblock = RfkillEvent {
            idx: 1,
            type_: RFKILL_TYPE_BLUETOOTH,
            op: RFKILL_OP_CHANGE,
            soft: 0,
            hard: 0,
        };
        assert_eq!(
            aggregate_rfkill_bluetooth_events(&[bt_add_soft, bt_unblock]),
            (false, false)
        );

        // Hard-blocked then deleted => no remaining blocked adapter.
        let bt_hard = RfkillEvent {
            idx: 2,
            type_: RFKILL_TYPE_BLUETOOTH,
            op: RFKILL_OP_ADD,
            soft: 0,
            hard: 1,
        };
        let bt_del = RfkillEvent {
            idx: 2,
            type_: RFKILL_TYPE_BLUETOOTH,
            op: RFKILL_OP_DEL,
            soft: 0,
            hard: 1,
        };
        assert_eq!(aggregate_rfkill_bluetooth_events(&[bt_hard]), (false, true));
        assert_eq!(
            aggregate_rfkill_bluetooth_events(&[bt_hard, bt_del]),
            (false, false)
        );
    }

    #[test]
    fn mac_only_name_detection() {
        assert!(is_mac_address("AA:BB:CC:DD:EE:FF"));
        assert!(is_mac_address("aa-bb-cc-dd-ee-ff"));
        assert!(is_mac_address("01:23:45:67:89:aB"));
        assert!(!is_mac_address(""));
        assert!(!is_mac_address("AA:BB:CC:DD:EE"));
        assert!(!is_mac_address("AA:BB:CC:DD:EE:FF:00"));
        assert!(!is_mac_address("GG:BB:CC:DD:EE:FF"));
        assert!(!is_mac_address("Sony WH-1000XM5"));
        assert!(!is_mac_address("AA_BB_CC_DD_EE_FF"));
    }

    #[test]
    fn device_icon_resolution() {
        let icons = IconsConfig::default();

        // Explicit BlueZ Icon property takes precedence over alias.
        assert_eq!(
            resolve_device_icon("audio-headset", "Some Mouse", &icons),
            icons.bt_headset
        );
        assert_eq!(
            resolve_device_icon("audio-headphones", "Device", &icons),
            icons.bt_headphones
        );
        assert_eq!(
            resolve_device_icon("audio-speakers", "Device", &icons),
            icons.bt_speaker
        );
        assert_eq!(
            resolve_device_icon("input-keyboard", "Device", &icons),
            icons.bt_keyboard
        );
        assert_eq!(
            resolve_device_icon("input-mouse", "Device", &icons),
            icons.bt_mouse
        );
        assert_eq!(
            resolve_device_icon("input-gaming", "Device", &icons),
            icons.bt_gamepad
        );
        assert_eq!(
            resolve_device_icon("phone", "Device", &icons),
            icons.bt_phone
        );
        assert_eq!(
            resolve_device_icon("computer", "Device", &icons),
            icons.bt_computer
        );
        assert_eq!(
            resolve_device_icon("camera-photo", "Device", &icons),
            icons.bt_camera
        );

        // Alias heuristics when Icon is empty or unrecognized.
        assert_eq!(
            resolve_device_icon("", "AirPods Pro", &icons),
            icons.bt_headset
        );
        assert_eq!(
            resolve_device_icon("", "Sony WH-1000XM5", &icons),
            icons.bt_headphones
        );
        assert_eq!(
            resolve_device_icon("", "JBL Flip 6", &icons),
            icons.bt_speaker
        );
        assert_eq!(
            resolve_device_icon("", "Keychron K2", &icons),
            icons.bt_keyboard
        );
        assert_eq!(
            resolve_device_icon("", "Logitech MX Master 3S", &icons),
            icons.bt_mouse
        );
        assert_eq!(
            resolve_device_icon("", "DualSense Wireless Controller", &icons),
            icons.bt_gamepad
        );
        assert_eq!(
            resolve_device_icon("unknown-icon", "Generic Gadget", &icons),
            icons.bluetooth
        );
    }

    #[test]
    fn device_sorting_and_filtering() {
        let mut snap = BluetoothSnapshot {
            synced: true,
            adapters: BTreeMap::new(),
            devices: BTreeMap::new(),
        };

        let devs = [
            sample_device(
                "/org/bluez/hci0/dev_1",
                "zeta",
                "00:00:00:00:00:01",
                false,
                true,
            ),
            sample_device(
                "/org/bluez/hci0/dev_2",
                "Alpha",
                "00:00:00:00:00:02",
                true,
                true,
            ),
            sample_device(
                "/org/bluez/hci0/dev_3",
                "Beta",
                "00:00:00:00:00:03",
                false,
                true,
            ),
            sample_device(
                "/org/bluez/hci0/dev_4",
                "Nearby Speaker",
                "00:00:00:00:00:04",
                false,
                false,
            ),
            // Nameless BLE advertisement whose alias is a MAC address
            sample_device(
                "/org/bluez/hci0/dev_5",
                "AA-BB-CC-DD-EE-FF",
                "AA:BB:CC:DD:EE:FF",
                false,
                false,
            ),
            sample_device(
                "/org/bluez/hci0/dev_6",
                "connected-b",
                "00:00:00:00:00:06",
                true,
                true,
            ),
        ];

        for d in devs {
            snap.devices.insert(d.path.clone(), d);
        }

        // Without unpaired: only connected/paired, sorted (connected desc, paired desc, alias asc).
        let paired_only = collect_devices(&snap, "/org/bluez/hci0", false);
        let names: Vec<&str> = paired_only.iter().map(|d| d.alias.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "connected-b", "Beta", "zeta"]);

        // With unpaired: includes named unpaired device, filters MAC-only BLE ad.
        let with_scan = collect_devices(&snap, "/org/bluez/hci0", true);
        let scan_names: Vec<&str> = with_scan.iter().map(|d| d.alias.as_str()).collect();
        assert_eq!(
            scan_names,
            vec!["Alpha", "connected-b", "Beta", "zeta", "Nearby Speaker"]
        );
    }

    #[test]
    fn device_label_formatting() {
        let icons = IconsConfig::default();

        let mut dev = sample_device(
            "/org/bluez/hci0/dev_1",
            "MX Master 3S",
            "11:22:33:44:55:66",
            true,
            true,
        );
        dev.battery = Some(85);
        assert_eq!(
            format_device_label(&dev, &icons),
            format!("{}  MX Master 3S (85%)  {}", icons.bt_mouse, icons.check)
        );

        dev.battery = None;
        assert_eq!(
            format_device_label(&dev, &icons),
            format!("{}  MX Master 3S  {}", icons.bt_mouse, icons.check)
        );

        dev.connected = false;
        assert_eq!(
            format_device_label(&dev, &icons),
            format!("{}  MX Master 3S", icons.bt_mouse)
        );

        dev.paired = false;
        assert_eq!(
            format_device_label(&dev, &icons),
            format!("{}  MX Master 3S (New)", icons.bt_mouse)
        );
    }
}
