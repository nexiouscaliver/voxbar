//! KB-301 (v1.6.9): the stall watchdog. The product rule that came out of
//! the v1.6.8 whole-app freeze: NO internal failure may ever leave VoxBar
//! hanging. Bugs are fixed at the root (KB-300 killed the self-deadlock),
//! but the registry and its sibling locks are shared by the quit teardown,
//! every tray resync, and every phone hello, so any future strand must
//! degrade into a visible error plus a self-healing restart instead of a
//! force-quit for the user.
//!
//! Design constraints that shaped it:
//! - The supervisor thread itself NEVER blocks on anything it monitors: the
//!   probes run on throwaway threads joined with a timeout, so a wedged
//!   probe is a signal, not a hang of the watchdog.
//! - The settings read rides inside the probe for the same reason: a wedged
//!   settings store counts as a stall (fail-safe: the watchdog only stays
//!   silent when it can positively read `stall_watchdog_enabled == false`).
//! - Recovery is last-resort and honest: one error log line, one macOS
//!   notification, then a detached relaunch helper and a clean exit(0). A
//!   tray dictation app that quietly restarts itself beats one that needs
//!   a force quit every time something deep wedges.

use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

use crate::companion::CompanionManager;

/// How often the supervisor wakes to probe core state.
const PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// A probe that cannot finish inside this window counts as stalled (the
/// join timeout, not the operation, is the signal).
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Consecutive stalled probes before recovery fires (15s at the defaults).
const STALL_THRESHOLD: u32 = 3;
/// The exit teardown gets this long before the fuse completes the exit.
const EXIT_FUSE: Duration = Duration::from_secs(5);

/// What one probe concluded.
#[derive(Debug, PartialEq, Eq)]
enum ProbeOutcome {
    /// The probe finished; every core lock was acquirable.
    Healthy,
    /// The probe finished but a core lock was held (try_lock refused).
    Stalled(&'static str),
    /// The user turned the watchdog off (read positively inside the probe).
    Disabled,
}

/// Start the supervisor (idempotent: one per process no matter how often
/// the settings panel reloads the app state).
pub fn start(app: tauri::AppHandle) {
    SUPERVISOR_STARTED.call_once(|| {
        std::thread::Builder::new()
            .name("stall-watchdog".into())
            .spawn(move || supervise(app))
            .expect("spawn the stall watchdog");
    });
}

static SUPERVISOR_STARTED: std::sync::Once = std::sync::Once::new();

fn supervise(app: tauri::AppHandle) {
    let mut consecutive_stalls: u32 = 0;
    loop {
        std::thread::sleep(PROBE_INTERVAL);
        match probe(&app) {
            ProbeOutcome::Healthy => consecutive_stalls = 0,
            ProbeOutcome::Disabled => consecutive_stalls = 0,
            ProbeOutcome::Stalled(what) => {
                consecutive_stalls += 1;
                log::error!(
                    "[stall-watchdog] {what} unacquirable (probe {consecutive_stalls}/{STALL_THRESHOLD})"
                );
                if consecutive_stalls >= STALL_THRESHOLD {
                    recover(&app, what);
                }
            }
        }
    }
}

/// One probe on a throwaway thread, joined with a timeout. The supervisor
/// itself never touches a monitored lock or the settings store.
fn probe(app: &tauri::AppHandle) -> ProbeOutcome {
    let app = app.clone();
    let handle = std::thread::spawn(move || {
        if !crate::settings::get_settings(&app).stall_watchdog_enabled {
            return ProbeOutcome::Disabled;
        }
        // The companion registry: the lock the v1.6.8 freeze stranded.
        if let Some(manager) = app.try_state::<Arc<CompanionManager>>() {
            if manager.registry_probe_timed_out() {
                return ProbeOutcome::Stalled("the companion registry");
            }
            if manager.lane_sessions_probe_timed_out() {
                return ProbeOutcome::Stalled("the lane session map");
            }
        }
        ProbeOutcome::Healthy
    });
    match handle.join_timeout(PROBE_TIMEOUT) {
        Ok(outcome) => outcome,
        Err(_did_not_finish) => ProbeOutcome::Stalled("a core subsystem probe"),
    }
}

/// JoinHandle::join with a timeout, without pulling a dependency: park on
/// a channel instead.
trait JoinTimeout<T> {
    fn join_timeout(self, timeout: Duration) -> Result<T, ()>;
}

impl<T: Send + 'static> JoinTimeout<T> for std::thread::JoinHandle<T> {
    fn join_timeout(self, timeout: Duration) -> Result<T, ()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(self.join());
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => result.map_err(|_| ()),
            Err(_) => Err(()),
        }
    }
}

/// Arm the quit fuse: the exit teardown on the main thread is best effort,
/// so it gets EXIT_FUSE seconds before the process is completed anyway. A
/// quit that strands is the exact shape the user had to force-quit in the
/// v1.6.8 freeze; nothing in the teardown is worth an unkillable zombie.
pub fn arm_exit_fuse() {
    std::thread::Builder::new()
        .name("exit-fuse".into())
        .spawn(|| {
            std::thread::sleep(EXIT_FUSE);
            // Still alive: the teardown is wedged. Complete the exit the
            // user asked for.
            log::error!(
                "[stall-watchdog] exit teardown exceeded {EXIT_FUSE:?}; completing the exit"
            );
            std::process::exit(0);
        })
        .expect("spawn the exit fuse");
}

/// The last resort: tell the user, then trade the wedged process for a
/// fresh one. The relaunch helper is spawned BEFORE the exit and outlives
/// this process, so ordering cannot race the single-instance handshake.
fn recover(app: &tauri::AppHandle, what: &str) -> ! {
    log::error!("[stall-watchdog] recovering from {what}: restarting the app");
    let _ = mac_notification_sys::set_application(&crate::persistent_notice::sender_identity(
        tauri::is_dev(),
        &app.config().identifier,
    ));
    let mut notice = mac_notification_sys::Notification::new();
    notice
        .title("VoxBar recovered from an internal stall")
        .message("Something inside VoxBar stopped responding, so it restarted itself. Dictation continues to work.");
    let _ = notice.send();

    if let Ok(exe) = std::env::current_exe() {
        // .../VoxBar.app/Contents/MacOS/voxbar -> .../VoxBar.app
        if let Some(app_bundle) = exe
            .ancestors()
            .nth(2)
            .and_then(|p| p.to_str())
            .map(str::to_string)
        {
            let helper = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "sleep 1; open -a '{}'",
                    app_bundle.replace('\'', "'\\''")
                ))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if let Err(e) = helper {
                log::error!("[stall-watchdog] relaunch helper failed: {e}");
            }
        }
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_are_conservative() {
        // 15s of unacquirable core state before recovery, 5s of wedged
        // teardown before the exit completes: long enough that healthy
        // contention (a tray rebuild, a SQLite write) never trips either,
        // short enough that the user never reaches for force-quit.
        assert_eq!(PROBE_INTERVAL.as_secs() * STALL_THRESHOLD as u64, 15);
        assert_eq!(EXIT_FUSE.as_secs(), 5);
        assert!(PROBE_TIMEOUT < PROBE_INTERVAL);
    }

    #[test]
    fn the_probe_degrades_to_stalled_when_it_cannot_finish() {
        // The join-timeout path: a probe thread that never finishes must
        // read as a stall, not hang the supervisor.
        let handle = std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_millis(50));
        });
        assert!(handle.join_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn the_probe_reads_healthy_when_the_locks_are_free() {
        let manager = CompanionManager::new();
        assert!(!manager.registry_probe_timed_out());
        assert!(!manager.lane_sessions_probe_timed_out());
    }

    #[test]
    fn a_held_registry_reads_as_stalled() {
        let manager = CompanionManager::new();
        let hold = manager.registry_hold_for_test();
        assert!(manager.registry_probe_timed_out());
        drop(hold);
        assert!(!manager.registry_probe_timed_out());
    }
}
