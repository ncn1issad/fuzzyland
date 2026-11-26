use crate::config::runtime_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MenuKind {
    Network,
    Bluetooth,
    Battery,
    AudioOutput,
    AudioInput,
    Power,
    Clipboard,
    Screenshot,
    Slurp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StandaloneMenuState {
    kind: MenuKind,
    pid: u32,
}

#[derive(Debug, Clone)]
struct ActiveMenu {
    kind: MenuKind,
    pid: Option<u32>,
    session_id: u64,
}

pub enum MenuSession {
    ToggledOff,
    Acquired(MenuGuard),
}

pub struct MenuCoordinator {
    standalone: bool,
    active: Mutex<Option<ActiveMenu>>,
    next_session_id: AtomicU64,
}

impl MenuCoordinator {
    pub fn new(standalone: bool) -> Arc<Self> {
        Arc::new(Self {
            standalone,
            active: Mutex::new(None),
            next_session_id: AtomicU64::new(1),
        })
    }

    fn standalone_state_path() -> PathBuf {
        runtime_dir().join("active-menu.json")
    }

    pub fn begin_session(self: &Arc<Self>, kind: MenuKind) -> MenuSession {
        if self.standalone {
            if let Some(prev) = Self::read_standalone_state() {
                let killed = terminate_expected_pid(prev.pid);
                let _ = std::fs::remove_file(Self::standalone_state_path());
                if killed && prev.kind == kind {
                    return MenuSession::ToggledOff;
                }
            }
        }

        let mut guard = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = guard.take() {
            let was_same = prev.kind == kind;
            if let Some(pid) = prev.pid {
                let killed = terminate_expected_pid(pid);
                debug!(
                    "MenuCoordinator terminated previous {:?} menu (pid={}, killed={})",
                    prev.kind, pid, killed
                );
                if was_same && killed {
                    return MenuSession::ToggledOff;
                }
            } else if was_same {
                // Same menu is already opening/active in-process; toggle it off.
                return MenuSession::ToggledOff;
            }
        }

        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        *guard = Some(ActiveMenu {
            kind,
            pid: None,
            session_id,
        });

        MenuSession::Acquired(MenuGuard {
            coordinator: Arc::clone(self),
            session_id,
        })
    }

    fn read_standalone_state() -> Option<StandaloneMenuState> {
        let raw = std::fs::read_to_string(Self::standalone_state_path()).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn write_standalone_state(kind: MenuKind, pid: u32) {
        let dir = runtime_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(json) = serde_json::to_string(&StandaloneMenuState { kind, pid }) {
            let _ = std::fs::write(Self::standalone_state_path(), json);
        }
    }

    fn clear_standalone_state_if_pid(pid: u32) {
        if let Some(state) = Self::read_standalone_state() {
            if state.pid == pid {
                let _ = std::fs::remove_file(Self::standalone_state_path());
            }
        }
    }
}

pub struct MenuGuard {
    coordinator: Arc<MenuCoordinator>,
    session_id: u64,
}

impl MenuGuard {
    pub fn is_current(&self) -> bool {
        let guard = self
            .coordinator
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        matches!(&*guard, Some(active) if active.session_id == self.session_id)
    }

    pub fn set_kind(&self, kind: MenuKind) {
        let mut guard = self
            .coordinator
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(active) = guard.as_mut() {
            if active.session_id == self.session_id {
                active.kind = kind;
                if self.coordinator.standalone {
                    if let Some(pid) = active.pid {
                        MenuCoordinator::write_standalone_state(kind, pid);
                    }
                }
            }
        }
    }

    pub fn register_child_pid(&self, pid: u32) -> bool {
        let mut guard = self
            .coordinator
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(active) = guard.as_mut() {
            if active.session_id == self.session_id {
                active.pid = Some(pid);
                if self.coordinator.standalone {
                    MenuCoordinator::write_standalone_state(active.kind, pid);
                }
                return true;
            }
        }
        false
    }

    pub fn clear_child_pid(&self, pid: u32) {
        let mut guard = self
            .coordinator
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(active) = guard.as_mut() {
            if active.session_id == self.session_id && active.pid == Some(pid) {
                active.pid = None;
            }
        }
        if self.coordinator.standalone {
            MenuCoordinator::clear_standalone_state_if_pid(pid);
        }
    }
}

impl Drop for MenuGuard {
    fn drop(&mut self) {
        let mut guard = self
            .coordinator
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(active) = guard.as_ref() {
            if active.session_id == self.session_id {
                if self.coordinator.standalone {
                    if let Some(pid) = active.pid {
                        MenuCoordinator::clear_standalone_state_if_pid(pid);
                    }
                }
                *guard = None;
            }
        }
    }
}

fn terminate_expected_pid(pid: u32) -> bool {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    let Ok(bytes) = std::fs::read(&cmdline_path) else {
        return false;
    };
    let lower = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    if !lower.contains("fuzzel") && !lower.contains("slurp") {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    rc == 0
}
