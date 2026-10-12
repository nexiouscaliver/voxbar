#[cfg(target_os = "macos")]
use std::process::Command;

/// Checks if the MacBook is in clamshell mode (lid closed with external display)
///
/// This queries the macOS IORegistry for the AppleClamshellState key.
/// Returns true if the lid is closed, false if open.
#[cfg(target_os = "macos")]
pub fn is_clamshell() -> Result<bool, String> {
    let output = Command::new("ioreg")
        .args(["-r", "-k", "AppleClamshellState", "-d", "4"])
        .output()
        .map_err(|e| format!("Failed to execute ioreg: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "ioreg command failed with status: {}",
            output.status
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Look for "AppleClamshellState" = Yes in the output
    Ok(stdout.contains("\"AppleClamshellState\" = Yes"))
}

/// Raw IOKit / CoreFoundation FFI for the lid watcher below. These are the
/// stable C APIs every macOS IOKit client uses (the shapes in IOKit.framework
/// and CoreFoundation.framework headers); the crate tree carries no iokit-sys
/// binding, so the five entry points the watcher needs are declared here.
/// Port constants: kIOMasterPortDefault and its renamed successor
/// kIOMainPortDefault are both the literal 0 bootstrap port.
#[cfg(target_os = "macos")]
mod watcher_ffi {
    use std::ffi::{c_char, c_int, c_void};

    pub const KIO_MASTER_PORT_DEFAULT: u32 = 0;
    pub const KERN_SUCCESS: c_int = 0;

    /// Opaque IONotificationPortRef.
    #[repr(C)]
    pub struct IonotificationPort {
        _private: [u8; 0],
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        pub fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        pub fn IOServiceGetMatchingService(
            master_port: u32,
            matching: *mut c_void,
        ) -> u32;
        pub fn IONotificationPortCreate(master_port: u32) -> *mut IonotificationPort;
        pub fn IONotificationPortGetRunLoopSource(
            notify_port: *mut IonotificationPort,
        ) -> *const c_void;
        pub fn IOServiceAddInterestNotification(
            notify_port: *mut IonotificationPort,
            service: u32,
            interest_type: *const c_char,
            callback: extern "C" fn(
                refcon: *mut c_void,
                service: u32,
                message_type: u32,
                message_argument: *mut c_void,
            ),
            refcon: *mut c_void,
            notification: *mut u32,
        ) -> c_int;
        pub fn IOObjectRelease(object: u32) -> c_int;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFRunLoopGetCurrent() -> *const c_void;
        pub fn CFRunLoopAddSource(
            run_loop: *const c_void,
            source: *const c_void,
            mode: *const c_void,
        );
        pub fn CFRunLoopRun();
        pub static kCFRunLoopCommonModes: *const c_void;
    }
}

/// KB-010: lid-state watcher state shared with the IOKit interest callback.
#[cfg(target_os = "macos")]
struct ClamshellWatcherState {
    on_change: Box<dyn Fn() + Send + Sync>,
    /// The lid state as of the last handled notification; the callback only
    /// fires `on_change` when the freshly probed state differs, so the many
    /// unrelated messages IOPMrootDomain broadcasts (AC attach, power
    /// assertions, sleep transitions) never trigger restarts.
    last_closed: std::sync::atomic::AtomicBool,
}

/// The IOKit interest callback: runs on the watcher thread's runloop. It
/// must not block that runloop (the handler does subprocess probes and, at
/// the far end, blocking CoreAudio restarts), so the whole body is handed
/// to a fresh worker thread. `refcon` points at a `ClamshellWatcherState`
/// leaked for the process lifetime in `spawn_clamshell_watcher`, so the
/// raw pointer dereference is always valid while the process runs.
#[cfg(target_os = "macos")]
extern "C" fn clamshell_interest_callback(
    refcon: *mut std::ffi::c_void,
    _service: u32,
    _message_type: u32,
    _message_argument: *mut std::ffi::c_void,
) {
    // Carried as usize so the worker-thread closure is Send (the pointer is
    // valid for the process lifetime; see spawn_clamshell_watcher).
    let refcon = refcon as usize;
    std::thread::spawn(move || {
        let state = unsafe { &*(refcon as *const ClamshellWatcherState) };
        let closed_now = match is_clamshell() {
            Ok(closed) => closed,
            Err(e) => {
                log::debug!("clamshell watcher: state probe failed ({e}); skipping");
                return;
            }
        };
        let was_closed = state.last_closed.swap(closed_now, std::sync::atomic::Ordering::SeqCst);
        if was_closed != closed_now {
            log::info!(
                "clamshell watcher: lid {}",
                if closed_now { "closed" } else { "open" }
            );
            (state.on_change)();
        }
    });
}

/// KB-010: spawn the lid (clamshell) watcher. `is_clamshell` above is a
/// poll, consulted only at device-resolve time, so a lid flip between
/// sessions left an always-on stream on the wrong microphone until
/// something else re-resolved it, and flipping the lid mid always-on
/// capture never switched devices at all. This watcher registers a
/// kIOGeneralInterest notification on IOPMrootDomain (the IORegistry
/// service that publishes AppleClamshellState; the kernel delivers clamshell
/// state changes to interested user-space clients as general interest
/// messages) on a dedicated runloop thread, and calls `on_change` whenever
/// the probed lid state actually changes. The handler re-runs
/// desired_microphone and restarts the guarded stream (see
/// AudioRecordingManager::handle_clamshell_change).
#[cfg(target_os = "macos")]
pub fn spawn_clamshell_watcher<F>(on_change: F)
where
    F: Fn() + Send + Sync + 'static,
{
    use std::sync::atomic::AtomicBool;

    // Leaked on purpose: the watcher (and IOKit's refcon) outlives every
    // other part of the app; releasing it would only race the runloop.
    // Carried as usize so the runloop-thread closure below is Send.
    let refcon = Box::into_raw(Box::new(ClamshellWatcherState {
        on_change: Box::new(on_change),
        last_closed: AtomicBool::new(is_clamshell().unwrap_or(false)),
    })) as usize;

    let spawned = std::thread::Builder::new()
        .name("clamshell-watcher".to_string())
        .spawn(move || unsafe {
            let master_port = watcher_ffi::KIO_MASTER_PORT_DEFAULT;

            let matching = watcher_ffi::IOServiceMatching(c"IOPMrootDomain".as_ptr());
            if matching.is_null() {
                log::warn!("clamshell watcher: IOServiceMatching failed");
                return;
            }
            // Consumes the matching dictionary.
            let service = watcher_ffi::IOServiceGetMatchingService(master_port, matching);
            if service == 0 {
                log::warn!("clamshell watcher: IOPMrootDomain not found");
                return;
            }

            let notify_port = watcher_ffi::IONotificationPortCreate(master_port);
            if notify_port.is_null() {
                log::warn!("clamshell watcher: IONotificationPortCreate failed");
                let _ = watcher_ffi::IOObjectRelease(service);
                return;
            }

            let mut notification: u32 = 0;
            // The interest type is the VALUE of the C macro
            // kIOGeneralInterest (IOKitKeys.h), which expands to the
            // string "IOGeneralInterest"; passing the macro's NAME is an
            // unknown interest type and the registration returns
            // kIOReturnUnsupported (verified on this host).
            let kr = watcher_ffi::IOServiceAddInterestNotification(
                notify_port,
                service,
                c"IOGeneralInterest".as_ptr(),
                clamshell_interest_callback,
                refcon as *mut std::ffi::c_void,
                &mut notification,
            );
            if kr != watcher_ffi::KERN_SUCCESS {
                log::warn!("clamshell watcher: IOServiceAddInterestNotification failed ({kr})");
                let _ = watcher_ffi::IOObjectRelease(service);
                return;
            }
            // The interest notification holds its own reference to the
            // service; release the one from the lookup.
            let _ = watcher_ffi::IOObjectRelease(service);
            let _ = notification;

            let source = watcher_ffi::IONotificationPortGetRunLoopSource(notify_port);
            watcher_ffi::CFRunLoopAddSource(
                watcher_ffi::CFRunLoopGetCurrent(),
                source,
                watcher_ffi::kCFRunLoopCommonModes,
            );
            log::info!("clamshell watcher: observing IOPMrootDomain lid state");
            // Runs for the process lifetime; nothing stops this thread.
            watcher_ffi::CFRunLoopRun();
        });

    if let Err(e) = spawned {
        log::warn!("clamshell watcher: failed to spawn the runloop thread: {e}");
    }
}

/// Non-macOS stub: clamshell mode does not exist, so there is no lid to
/// watch and the handler is dropped unused.
#[cfg(not(target_os = "macos"))]
pub fn spawn_clamshell_watcher<F>(on_change: F)
where
    F: Fn() + Send + Sync + 'static,
{
    let _ = on_change;
}

/// Checks if the Mac is a laptop by detecting battery presence
///
/// This uses pmset to check for battery information.
/// Returns true if a battery is detected (laptop), false otherwise (desktop)
#[cfg(target_os = "macos")]
#[tauri::command]
#[specta::specta]
pub fn is_laptop() -> Result<bool, String> {
    let output = Command::new("pmset")
        .arg("-g")
        .arg("batt")
        .output()
        .map_err(|e| e.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Check if InternalBattery is present (laptops have batteries, desktops typically don't)
    Ok(stdout.contains("InternalBattery"))
}

/// Stub implementation for non-macOS platforms
/// Always returns false since clamshell mode is macOS-specific
#[cfg(not(target_os = "macos"))]
pub fn is_clamshell() -> Result<bool, String> {
    Ok(false)
}

/// Stub implementation for non-macOS platforms
/// Always returns false since laptop detection is macOS-specific
#[cfg(not(target_os = "macos"))]
#[tauri::command]
#[specta::specta]
pub fn is_laptop() -> Result<bool, String> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn test_clamshell_check() {
        // This will run on macOS and should not panic
        let result = is_clamshell();
        assert!(result.is_ok());
        let _ = result.unwrap();
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_is_laptop() {
        let result = is_laptop();
        assert!(result.is_ok());
        if let Ok(is_laptop) = result {
            println!("Is laptop: {}", is_laptop);
        }
    }

    /// KB-010 pin: the lid watcher must actually observe the kernel's
    /// clamshell signal rather than poll. The live registration needs a
    /// runloop an integration test here cannot drive with a real lid flip,
    /// so the registration facts are pinned against this committed source
    /// (the same convention as the KB-105 overlay sequencing pin and the
    /// persistent_notice authorization-gate pin):
    /// 1. the interest is registered on IOPMrootDomain, the IORegistry
    ///    service that publishes AppleClamshellState, with
    ///    kIOGeneralInterest (the kernel delivers clamshell state changes
    ///    to user space as general interest messages on that service);
    /// 2. the callback never blocks the watcher's runloop (the handler
    ///    probes ioreg and eventually restarts CoreAudio), it hands the
    ///    work to a worker thread;
    /// 3. on_change fires only when the probed lid state differs from the
    ///    last seen one, so unrelated root-domain messages (AC attach,
    ///    power assertions, sleep) never trigger a stream restart.
    #[test]
    fn kb010_lid_watcher_registers_an_iokit_interest_and_dedups() {
        let source = include_str!("clamshell.rs");

        let watcher = source
            .find("pub fn spawn_clamshell_watcher")
            .expect("spawn_clamshell_watcher exists");
        let watcher_body = &source[watcher..source.len()];

        assert!(
            watcher_body.contains("IOServiceMatching(c\"IOPMrootDomain\".as_ptr())"),
            "the interest must be registered on IOPMrootDomain, the service that publishes AppleClamshellState"
        );
        assert!(
            watcher_body.contains("c\"IOGeneralInterest\".as_ptr()"),
            "the interest type must be the kIOGeneralInterest macro's VALUE (the bare string), not its name"
        );
        assert!(
            watcher_body.contains("clamshell_interest_callback"),
            "the registration must name the callback"
        );

        let callback = source
            .find("extern \"C\" fn clamshell_interest_callback")
            .expect("clamshell_interest_callback exists");
        let callback_body = &source[callback..watcher];

        let offload = callback_body
            .find("std::thread::spawn")
            .expect("the callback offloads its work to a worker thread");
        assert!(
            offload < callback_body.find("is_clamshell()").expect("the callback probes the lid state"),
            "the probe must run off the runloop thread"
        );

        let swap = callback_body
            .find("last_closed.swap")
            .expect("the callback compares against the last seen lid state");
        let fire = callback_body
            .find("(state.on_change)()")
            .expect("the callback fires the handler");
        assert!(
            swap < fire,
            "the state comparison must gate the handler call"
        );
        assert!(
            callback_body[swap..fire]
                .contains("was_closed != closed_now"),
            "the handler fires only on an actual lid transition"
        );
    }

    /// KB-010 runtime probe: the FFI declarations link against IOKit and
    /// CoreFoundation, and the interest registration itself succeeds on
    /// THIS host (kern_return_t == KERN_SUCCESS). The callback firing
    /// needs a runloop plus a physical lid flip a unit test cannot drive;
    /// this proves the watcher's registration path is live (no early
    /// return) rather than trusting the signatures compile.
    #[test]
    #[cfg(target_os = "macos")]
    fn kb010_iokit_interest_registration_succeeds_on_this_host() {
        extern "C" fn noop(
            _refcon: *mut std::ffi::c_void,
            _service: u32,
            _message_type: u32,
            _message_argument: *mut std::ffi::c_void,
        ) {
        }

        unsafe {
            let matching = watcher_ffi::IOServiceMatching(c"IOPMrootDomain".as_ptr());
            assert!(!matching.is_null(), "IOServiceMatching must build the dictionary");
            let service = watcher_ffi::IOServiceGetMatchingService(
                watcher_ffi::KIO_MASTER_PORT_DEFAULT,
                matching,
            );
            assert_ne!(service, 0, "IOPMrootDomain must resolve");
            let port = watcher_ffi::IONotificationPortCreate(watcher_ffi::KIO_MASTER_PORT_DEFAULT);
            assert!(!port.is_null(), "the notification port must allocate");
            let mut notification: u32 = 0;
            let kr = watcher_ffi::IOServiceAddInterestNotification(
                port,
                service,
                c"IOGeneralInterest".as_ptr(),
                noop,
                std::ptr::null_mut(),
                &mut notification,
            );
            assert_eq!(
                kr,
                watcher_ffi::KERN_SUCCESS,
                "the general-interest registration must succeed"
            );
            // The registration holds its own service reference.
            let _ = watcher_ffi::IOObjectRelease(service);
        }
    }
}
