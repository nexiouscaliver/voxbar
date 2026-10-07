//! glibc allocator tuning (Linux only).
//!
//! Each dictation allocates multi-megabyte transient buffers - the captured
//! 16 kHz PCM (~64 KB per second, held twice: once for transcription, once
//! for the history WAV) plus the transcription engine's per-run mel/FFT
//! scratch (~80 KB per second of audio) - all freed within seconds.
//!
//! glibc's malloc serves allocations above its "mmap threshold" with a
//! private mmap that is returned to the OS on free. But the threshold is
//! *dynamic*: freeing an mmapped block raises it to that block's size (up to
//! 32 MB), so after the first dictation every later large buffer is served
//! from malloc arenas instead. Arena memory freed by the app is cached for
//! reuse, and interleaved small live allocations pin those pages, so the OS
//! never gets them back: RSS grows by roughly the transient-buffer volume of
//! each dictation, is never touched again, and slowly migrates to swap
//! (issue #1792 - measured at ~15 MB retained per 2-minute dictation;
//! pinning the threshold reduced that to ~0.5 MB).
//!
//! Both entry points are no-ops on non-glibc targets (Windows, macOS, musl):
//! this failure mode is specific to glibc's dynamic-threshold heuristic.

/// Pin glibc's mmap threshold so large transient buffers keep taking the
/// mmap path and are returned to the OS as soon as they are freed.
///
/// Must run before the workload allocates (called at the top of `run()`,
/// and of the transcription worker's own `run()`);
/// the cost is an mmap/munmap round-trip per multi-MB buffer, which is
/// negligible at dictation frequency.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn init_allocator() {
    // SAFETY: FFI call with no memory arguments; mallopt only updates
    // malloc's internal parameters.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn init_allocator() {}

/// Return freed-but-cached malloc arena memory to the OS.
///
/// Called once per finished transcription pipeline (see `FinishGuard`), and
/// by the transcription worker after each run or stream; it
/// sweeps whatever smaller-than-threshold churn still accumulates in the
/// arenas. Takes on the order of a millisecond, off the main thread.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn trim_freed_memory() {
    // SAFETY: FFI call with no memory arguments; malloc_trim releases whole
    // free pages back to the OS via madvise and is thread-safe.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn trim_freed_memory() {}

// --- Memory-pressure gate (spec F3) ----------------------------------------

/// Headroom kept free above the model's forecast footprint before the gate
/// refuses a load: 1.5 GiB (the OS, the app, and the engine's transient
/// run-time allocations all need room beyond the model file's size).
pub const DEFAULT_HEADROOM_BYTES: u64 = 1536 * 1024 * 1024;

// --- Kernel pressure verdict (macOS) ---------------------------------------

/// macOS kernel memory-pressure verdicts, the values of
/// `kern.memorystatus_vm_pressure_level` (the `kVMPressure*` tiers the
/// kernel exports to user space).
pub const PRESSURE_LEVEL_NORMAL: u32 = 1;
pub const PRESSURE_LEVEL_WARN: u32 = 2;
pub const PRESSURE_LEVEL_CRITICAL: u32 = 4;

/// Inactive-page credit factor when the pressure sysctl is UNREADABLE: the
/// documented middle ground between the comfortable (1.0) and warning (0.25)
/// postures, and the gate still fails open.
pub const PRESSURE_UNREADABLE_INACTIVE_FACTOR: f64 = 0.5;

/// How much of the machine's INACTIVE pages (reclaimable file cache) to
/// count as available, given the kernel's own pressure verdict. Pure.
///
/// History: the probe originally counted free+speculative+purgeable+inactive
/// unconditionally and overcounted 4.8x under real pressure (inactive pages
/// are reclaimed only slowly while the machine swaps). The fix excluded
/// inactive entirely, which starved the estimate on an IDLE machine: macOS
/// parks most reclaimable memory in INACTIVE when comfortable, so an idle
/// box with 8+ GiB genuinely available read as too tight and refused loads
/// it should have taken (the v1.0.0 regression). The kernel's pressure
/// verdict is what distinguishes the two states, so the credit now follows
/// it: full credit when NORMAL, a quarter when WARN, none when CRITICAL.
pub fn inactive_factor_for_pressure(level: u32) -> f64 {
    match level {
        PRESSURE_LEVEL_NORMAL => 1.0,
        PRESSURE_LEVEL_WARN => 0.25,
        PRESSURE_LEVEL_CRITICAL => 0.0,
        // Any value outside the documented verdicts (the undefined 0, or a
        // tier a future kernel adds) reads as at-least-warn: a brand-new
        // pressure tier is more likely a worse state than a better one, and
        // erring low here errs against the swap/OOM failure mode.
        _ => 0.25,
    }
}

/// One availability reading plus everything the gate's structured log line
/// needs to explain it. Plain data so callers (and tests) can inspect the
/// verdict's inputs instead of re-probing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AvailabilityProbe {
    /// Best-effort available RAM in bytes; `None` when the probe fails -
    /// callers must FAIL OPEN on `None`.
    pub available_bytes: Option<u64>,
    /// The kernel pressure verdict the reading was taken under: 1/2/4 on
    /// macOS, `None` when the sysctl is unreadable (fail-open factor
    /// applies), or the documented NORMAL passthrough constant on platforms
    /// without the sysctl.
    pub pressure_level: Option<u32>,
    /// The inactive-credit factor the probe's composition applied, or `None`
    /// when the reading did not use the composition at all (the
    /// `os_proc_available_memory` branch, or non-macOS probes that already
    /// fold reclaimable memory into the number they return).
    pub inactive_factor: Option<f64>,
}

/// The full availability probe: bytes, kernel pressure verdict, and the
/// inactive factor the composition applied. Sources per platform:
///
/// - macOS: `os_proc_available_memory()` first (libc does not bind it, so
///   the extern is declared below; available on macOS 11+, this app's
///   effective deployment floor - rustc links with
///   `-mmacosx-version-min=11.0.0`). FALLBACK: on this app's actual target
///   machine (macOS 26 / darwin 25.6) `os_proc_available_memory()` was
///   measured returning 0 for ordinary processes (verified from plain C,
///   outside any sandbox), so a zero reading falls back to the
///   pressure-scaled `host_statistics64` composition (see
///   [`pressure_adjusted_page_bytes`]) instead of poisoning the gate with
///   "0 bytes free".
/// - Linux: `/proc/meminfo` `MemAvailable` (the kernel's own reclaim
///   estimate).
/// - Windows: `GlobalMemoryStatusEx` `ullAvailPhys`.
pub fn probe_availability() -> AvailabilityProbe {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: extern with no arguments returning a plain u64 from
        // libSystem; thread-safe and allocation-free.
        let proc = unsafe { os_proc_available_memory() };
        if proc > 0 {
            return AvailabilityProbe {
                available_bytes: Some(proc),
                pressure_level: memory_pressure_level(),
                // This branch does not use the page composition; the factor
                // is not applicable (logged as n/a).
                inactive_factor: None,
            };
        }
        return host_statistics_probe();
    }
    #[cfg(target_os = "linux")]
    {
        // MemAvailable is the kernel's own reclaim estimate: it already
        // includes the Linux analog of macOS inactive file cache, so no
        // pressure scaling applies. The pressure field reads as the
        // documented NORMAL passthrough for the log line.
        let available = std::fs::read_to_string("/proc/meminfo").ok().and_then(|meminfo| {
            meminfo.lines().find_map(|line| {
                let rest = line.strip_prefix("MemAvailable:")?;
                let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
                Some(kb.saturating_mul(1024))
            })
        });
        AvailabilityProbe {
            available_bytes: available,
            pressure_level: Some(PRESSURE_LEVEL_NORMAL),
            inactive_factor: None,
        }
    }
    #[cfg(target_os = "windows")]
    {
        // ullAvailPhys counts only truly free physical memory (the standby
        // list, Windows's inactive analog, is excluded), so the reading is
        // already conservative and no scaling applies; the pressure field
        // reads as the documented NORMAL passthrough for the log line.
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: writable pointer to a correctly-sized struct of the
        // expected type; GlobalMemoryStatusEx only fills it in.
        let available = unsafe { GlobalMemoryStatusEx(&mut status) }
            .ok()
            .map(|_| status.ullAvailPhys);
        AvailabilityProbe {
            available_bytes: available,
            pressure_level: Some(PRESSURE_LEVEL_NORMAL),
            inactive_factor: None,
        }
    }
}

/// The kernel's own memory-pressure verdict via
/// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warn, 4 critical
/// ([`PRESSURE_LEVEL_*`]), or `None` when the sysctl is unreadable - callers
/// fail open with the [`PRESSURE_UNREADABLE_INACTIVE_FACTOR`] middle-ground
/// factor and a one-time warning.
#[cfg(target_os = "macos")]
fn memory_pressure_level() -> Option<u32> {
    let mut level: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::size_t;
    // SAFETY: sysctlbyname writing into a correctly sized buffer.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_vm_pressure_level".as_ptr(),
            &mut level as *mut _ as *mut libc::c_void,
            &mut len as *mut libc::size_t,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(level as u32)
}

/// Platforms without the macOS pressure sysctl: the probes there return
/// numbers that already carry their kernel's own reclaim semantics (Linux
/// MemAvailable includes reclaimable cache; Windows ullAvailPhys excludes
/// the standby list and is therefore conservative), so no inactive scaling
/// applies - the verdict passes through as NORMAL and the log line says so.
#[cfg(not(target_os = "macos"))]
fn memory_pressure_level() -> Option<u32> {
    Some(PRESSURE_LEVEL_NORMAL)
}

/// One-time warning for an unreadable pressure sysctl: every later probe
/// stays silent (the gate runs per model load; a log line each time would
/// be noise), but the first one must be visible.
#[cfg(target_os = "macos")]
static UNREADABLE_PRESSURE_LOG: std::sync::Once = std::sync::Once::new();

#[cfg(target_os = "macos")]
fn log_unreadable_pressure_once() {
    UNREADABLE_PRESSURE_LOG.call_once(|| {
        log::warn!(
            "memory probe: kern.memorystatus_vm_pressure_level unreadable; counting inactive \
             pages at factor {PRESSURE_UNREADABLE_INACTIVE_FACTOR:.2} and failing open (logged \
             once)"
        );
    });
}

/// The pure page-sum at the heart of the macOS fallback probe:
/// free + speculative + purgeable + inactive x factor, all times the page
/// size.
///
/// The static half (free+speculative+purgeable) is memory the kernel can
/// hand out immediately. The inactive term is the pressure-scaled credit
/// (see [`inactive_factor_for_pressure`] for the full history): full when
/// the kernel reports NORMAL pressure because an idle macOS parks most
/// reclaimable file cache in INACTIVE, a quarter under WARN, none under
/// CRITICAL - the state where those pages are being actively reclaimed to
/// keep up and the original 4.82x overcount was measured. The f64 multiply
/// is exact: page counts sit far below 2^53 and every factor in the matrix
/// (1.0, 0.5, 0.25, 0.0) is an exact binary fraction.
#[cfg(target_os = "macos")]
fn pressure_adjusted_page_bytes(
    vm: &libc::vm_statistics64,
    page_size: u64,
    inactive_factor: f64,
) -> u64 {
    let static_pages =
        vm.free_count as u64 + vm.speculative_count as u64 + vm.purgeable_count as u64;
    let inactive_pages = (vm.inactive_count as f64 * inactive_factor).round() as u64;
    static_pages.saturating_add(inactive_pages).saturating_mul(page_size)
}

/// macOS fallback probe: available RAM from `host_statistics64` as an
/// [`AvailabilityProbe`] - [`pressure_adjusted_page_bytes`] over the
/// kernel's page counters, with the inactive credit scaled by the kernel's
/// own pressure verdict. An unreadable verdict applies the documented 0.5
/// middle-ground factor and fails open (the gate never refuses on a probe
/// problem).
#[cfg(target_os = "macos")]
fn host_statistics_probe() -> AvailabilityProbe {
    use libc::{
        host_statistics64, mach_host_self, vm_statistics64, vm_statistics64_data_t, HOST_VM_INFO64,
    };
    let mut vm: vm_statistics64_data_t = unsafe { std::mem::zeroed() };
    let mut count = (std::mem::size_of::<vm_statistics64>()
        / std::mem::size_of::<libc::integer_t>())
        as libc::mach_msg_type_number_t;
    // SAFETY: standard mach host statistics writing into a correctly typed
    // vm_statistics64 with the matching HOST_VM_INFO64 flavor.
    let kr = unsafe {
        host_statistics64(
            mach_host_self(),
            HOST_VM_INFO64,
            &mut vm as *mut _ as *mut libc::integer_t,
            &mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return AvailabilityProbe {
            available_bytes: None,
            pressure_level: memory_pressure_level(),
            inactive_factor: None,
        };
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let level = memory_pressure_level();
    let factor = match level {
        Some(level) => inactive_factor_for_pressure(level),
        None => {
            log_unreadable_pressure_once();
            PRESSURE_UNREADABLE_INACTIVE_FACTOR
        }
    };
    AvailabilityProbe {
        available_bytes: Some(pressure_adjusted_page_bytes(&vm, page, factor)),
        pressure_level: level,
        inactive_factor: Some(factor),
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn os_proc_available_memory() -> u64;
}

/// Best-effort resident set size of a process in bytes, or `None` on any
/// failure. Used to credit the outgoing model's footprint when deciding
/// whether a new one fits (the pages it holds are freed before the new
/// model's peak).
///
/// - macOS: `proc_pid_rusage` `ri_resident_size` (libc binds both).
/// - Linux: `/proc/<pid>/statm` resident pages × page size.
/// - Windows: `GetProcessMemoryInfo` `WorkingSetSize` of a
///   `PROCESS_QUERY_LIMITED_INFORMATION` handle.
pub fn rss_bytes_for_pid(pid: u32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        use libc::{proc_pid_rusage, rusage_info_v4, RUSAGE_INFO_V4};
        let mut info: rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: pid from our own spawned child; buffer is a correctly
        // typed rusage_info_v4 for the RUSAGE_INFO_V4 flavor.
        let rc = unsafe {
            proc_pid_rusage(
                pid as libc::c_int,
                RUSAGE_INFO_V4,
                &mut info as *mut _ as *mut libc::rusage_info_t,
            )
        };
        (rc == 0).then_some(info.ri_resident_size)
    }
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        (page > 0).then(|| resident_pages.saturating_mul(page as u64))
    }
    #[cfg(target_os = "windows")]
    {
        use windows::core::PCWSTR;
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // SAFETY: standard handle + correctly-sized counters struct for our
        // own child process.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut counters = PROCESS_MEMORY_COUNTERS::default();
            let ok = GetProcessMemoryInfo(
                handle,
                &mut counters,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
            let _ = windows::Win32::Foundation::CloseHandle(handle);
            ok.is_ok().then_some(counters.WorkingSetSize as u64)
        }
    }
}

/// The pure refusal decision for the memory-pressure gate.
///
/// `free` is the (already resident-credited) bytes available; `forecast` the
/// incoming model's estimated footprint; `headroom` what must remain free
/// above the forecast. `None` (probe unavailable) NEVER refuses - fail open.
/// Boundary: `forecast + headroom == free` allows (saturating, so huge
/// forecasts still refuse), `== free + 1` refuses.
pub fn gate_should_refuse(free: Option<u64>, forecast: u64, headroom: u64) -> bool {
    match free {
        Some(free) => forecast.saturating_add(headroom) > free,
        None => false,
    }
}

/// Credit for the outgoing (about-to-be-replaced) model's footprint:
/// the measured worker RSS when there is one (`Some`), otherwise the
/// size-derived estimate used for in-process ONNX engines, else 0.
pub fn resident_credit(measured_rss: Option<u64>, estimate: Option<u64>) -> u64 {
    measured_rss.unwrap_or_else(|| estimate.unwrap_or(0))
}

// --- RAM auto-fallback resolver ---------------------------------------------

/// A downloaded model considered as a fallback when the selected model does
/// not fit free RAM. Plain data so the resolver below stays pure and
/// unit-testable without an app or a model manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackCandidate {
    pub id: String,
    /// Catalog recommended rank (lower = better); `u32::MAX` for unranked
    /// models so they sort last.
    pub rank: u32,
    /// Estimated footprint in bytes - the SAME size-derived forecast the
    /// F3 gate compares (`size_mb` MiB plus the fixed
    /// [`DEFAULT_HEADROOM_BYTES`] compute allowance), so "fits" here means
    /// exactly "the gate would allow this load".
    pub footprint_bytes: u64,
}

/// Pick the best already-downloaded model to fall back to after the selected
/// model was refused by the memory gate. Pure.
///
/// Preference: catalog `rank` ascending (lower = better), ties broken by the
/// smaller footprint (under pressure, smaller is safer), then by id for
/// determinism. Only downloaded candidates belong in `candidates` (the
/// "prefer the quant actually downloaded" rule - the caller lists exactly
/// what is on disk); the model that just failed is always excluded. A
/// candidate fits when the F3 gate would NOT refuse it against `free`.
/// `free == None` (probe unavailable) never resolves - the gate fails open
/// in that case, so there is nothing to fall back FROM.
pub fn resolve_fallback_model<'a>(
    free: Option<u64>,
    candidates: &'a [FallbackCandidate],
    failed_id: &str,
) -> Option<&'a FallbackCandidate> {
    let free = free?;
    candidates
        .iter()
        .filter(|candidate| candidate.id != failed_id)
        .filter(|candidate| {
            !gate_should_refuse(
                Some(free),
                candidate.footprint_bytes,
                DEFAULT_HEADROOM_BYTES,
            )
        })
        .min_by(|a, b| {
            a.rank
                .cmp(&b.rank)
                .then(a.footprint_bytes.cmp(&b.footprint_bytes))
                .then_with(|| a.id.cmp(&b.id))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn gate_table() {
        let headroom = DEFAULT_HEADROOM_BYTES;
        // (free, forecast, expected refusal)
        let cases: &[(Option<u64>, u64, bool)] = &[
            // Probe failure always fails open.
            (None, 0, false),
            (None, 64 * GIB, false),
            // Boundary: forecast + headroom == free -> allow (not >).
            (Some(headroom + 10 * GIB), 10 * GIB, false),
            // One byte over -> refuse.
            (Some(headroom + 10 * GIB - 1), 10 * GIB, true),
            (Some(headroom + 10 * GIB), 10 * GIB + 1, true),
            // Trivially fitting loads.
            (Some(16 * GIB), 697 * 1024 * 1024, false),
            // Forecast alone exceeds free.
            (Some(GIB), 2 * GIB, true),
            // Saturating: enormous forecast refuses against any real free.
            (Some(u64::MAX - 1), u64::MAX, true),
        ];
        for (free, forecast, expected) in cases {
            assert_eq!(
                gate_should_refuse(*free, *forecast, headroom),
                *expected,
                "free={free:?} forecast={forecast}"
            );
        }
    }

    #[test]
    fn resident_credit_prefers_measured_rss_then_estimate() {
        let est = 697 * 1024 * 1024;
        // Measured worker RSS wins (TranscribeCpp branch).
        assert_eq!(
            resident_credit(Some(800 * 1024 * 1024), Some(est)),
            800 * 1024 * 1024
        );
        // No worker (ONNX estimate branch): the size-derived estimate.
        assert_eq!(resident_credit(None, Some(est)), est);
        // Nothing resident.
        assert_eq!(resident_credit(None, None), 0);
        // Measured zero (empty worker) is a real measurement, not a fallback.
        assert_eq!(resident_credit(Some(0), Some(est)), 0);
    }

    #[test]
    fn effective_free_adds_resident_credit() {
        let headroom = GIB; // smaller than the credit, so both sides differ
        let free = 8 * GIB;
        let credit = resident_credit(Some(2 * GIB), None);
        assert_eq!(credit, 2 * GIB);
        // With the credit the 8 GiB forecast fits (8 + 1 <= 8 + 2)…
        assert!(!gate_should_refuse(Some(free + credit), 8 * GIB, headroom));
        // …while without it, the same load is refused (8 + 1 > 8).
        assert!(gate_should_refuse(Some(free), 8 * GIB, headroom));
    }

    #[test]
    fn headroom_default_is_one_and_a_half_gib() {
        assert_eq!(DEFAULT_HEADROOM_BYTES, 3 * GIB / 2);
    }

    /// The live probe sanity check (macOS): the reading must be positive and
    /// cannot exceed the machine's physical RAM (`hw.memsize`). This is the
    /// undercount guard's unit-testable half - a wildly wrong reading (0 or
    /// > total) fails here.
    #[cfg(target_os = "macos")]
    #[test]
    fn available_memory_reads_between_zero_and_total() {
        let mut total: libc::c_ulonglong = 0;
        let mut len = std::mem::size_of::<libc::c_ulonglong>() as libc::size_t;
        // SAFETY: sysctlbyname writing into a correctly sized buffer.
        let rc = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                &mut total as *mut _ as *mut libc::c_void,
                &mut len as *mut libc::size_t,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(rc, 0, "sysctlbyname hw.memsize failed");
        let available = probe_availability()
            .available_bytes
            .expect("probe returned None");
        assert!(available > 0, "available memory must be positive, got 0");
        assert!(
            available <= total,
            "available {available} exceeds physical RAM {total}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn available_memory_reads_between_zero_and_total() {
        let total: u64 = std::fs::read_to_string("/proc/meminfo")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|v| v.trim().split_whitespace().next().unwrap().parse().ok())
            .unwrap();
        let available = probe_availability()
            .available_bytes
            .expect("probe returned None");
        assert!(available > 0);
        assert!(available <= total.saturating_mul(1024));
    }

    #[test]
    fn rss_for_invalid_pid_is_none() {
        // pid u32::MAX is never allocated; the probe must fail, not hang or
        // return garbage.
        assert_eq!(rss_bytes_for_pid(u32::MAX), None);
    }

    #[test]
    fn fallback_resolver_picks_best_rank_that_fits() {
        // free = 10 GiB; headroom 1.5 GiB; footprints in bytes below.
        let free = Some(10 * GIB);
        let candidates = vec![
            cand("big-best", 1, 10 * GIB),   // best rank, does NOT fit
            cand("mid-second", 2, 5 * GIB),  // fits
            cand("small-third", 3, 1 * GIB), // fits
            cand("tiny-unranked", u32::MAX, 100 * 1024 * 1024),
        ];
        // Rank wins over size when both fit: rank-2 beats rank-3 even though
        // it is bigger.
        assert_eq!(
            resolve_fallback_model(free, &candidates, "selected").map(|c| c.id.as_str()),
            Some("mid-second")
        );
    }

    #[test]
    fn fallback_resolver_skips_non_fitting_and_falls_to_worse_ranks() {
        let free = Some(4 * GIB);
        let candidates = vec![
            cand("rank1-too-big", 1, 10 * GIB),
            cand("rank2-too-big", 2, 4 * GIB), // 4 + 1.5 > 4 -> refused
            cand("rank3-fits", 3, 2 * GIB),    // 2 + 1.5 <= 4 -> fits
        ];
        assert_eq!(
            resolve_fallback_model(free, &candidates, "selected").map(|c| c.id.as_str()),
            Some("rank3-fits")
        );
    }

    #[test]
    fn fallback_resolver_excludes_the_failed_model() {
        let free = Some(10 * GIB);
        let candidates = vec![
            cand("failed", 1, 1 * GIB), // would fit and outrank - but it JUST failed
            cand("other", 2, 1 * GIB),
        ];
        assert_eq!(
            resolve_fallback_model(free, &candidates, "failed").map(|c| c.id.as_str()),
            Some("other")
        );
    }

    #[test]
    fn fallback_resolver_breaks_rank_ties_by_footprint_then_id() {
        let free = Some(10 * GIB);
        // Same rank (alternate quants of one model share the descriptor
        // rank): the smaller footprint wins.
        let same_rank = vec![
            cand("q8", 4, 700 * 1024 * 1024),
            cand("f16", 4, 1500 * 1024 * 1024),
        ];
        assert_eq!(
            resolve_fallback_model(free, &same_rank, "selected").map(|c| c.id.as_str()),
            Some("q8")
        );
        // Fully tied: stable, deterministic by id.
        let tied = vec![cand("b", 4, 1 * GIB), cand("a", 4, 1 * GIB)];
        assert_eq!(
            resolve_fallback_model(free, &tied, "selected").map(|c| c.id.as_str()),
            Some("a")
        );
    }

    #[test]
    fn fallback_resolver_returns_none_when_nothing_fits_or_probe_unavailable() {
        let free = Some(2 * GIB);
        let too_big = vec![cand("a", 1, 10 * GIB), cand("b", 2, 8 * GIB)];
        assert_eq!(resolve_fallback_model(free, &too_big, "selected"), None);
        // Empty candidate list (only the failed model downloaded).
        assert_eq!(resolve_fallback_model(free, &[], "selected"), None);
        // Probe unavailable: the gate fails open, so there is no refusal to
        // answer - never resolve.
        assert_eq!(
            resolve_fallback_model(None, &[cand("a", 1, 1 * GIB)], "selected"),
            None
        );
    }

    #[test]
    fn fallback_resolver_fit_boundary_matches_the_gate() {
        // The resolver's fit check IS the gate: boundary forecast+headroom ==
        // free fits, one byte over does not.
        let forecast = 4 * GIB;
        let free_exact = Some(forecast + DEFAULT_HEADROOM_BYTES);
        let candidates = vec![cand("edge", 1, forecast)];
        assert_eq!(
            resolve_fallback_model(free_exact, &candidates, "selected").map(|c| c.id.as_str()),
            Some("edge")
        );
        assert_eq!(
            resolve_fallback_model(Some(free_exact.unwrap() - 1), &candidates, "selected"),
            None
        );
    }

    /// The factor matrix as specified: full inactive credit under NORMAL
    /// pressure (an idle macOS parks reclaimable file cache in inactive),
    /// a quarter under WARN, none under CRITICAL, the 0.5 middle ground
    /// when the sysctl is unreadable, and at-least-warn for any verdict
    /// outside the documented tiers.
    #[test]
    fn inactive_factor_matrix() {
        assert_eq!(inactive_factor_for_pressure(PRESSURE_LEVEL_NORMAL), 1.0);
        assert_eq!(inactive_factor_for_pressure(PRESSURE_LEVEL_WARN), 0.25);
        assert_eq!(inactive_factor_for_pressure(PRESSURE_LEVEL_CRITICAL), 0.0);
        // Undefined/future verdicts read as at-least-warn.
        assert_eq!(inactive_factor_for_pressure(0), 0.25);
        assert_eq!(inactive_factor_for_pressure(3), 0.25);
        assert_eq!(inactive_factor_for_pressure(99), 0.25);
        // Unreadable sysctl: the documented middle-ground constant.
        assert_eq!(PRESSURE_UNREADABLE_INACTIVE_FACTOR, 0.5);
    }

    /// Fixture test for [`pressure_adjusted_page_bytes`]: the static half is
    /// always free + speculative + purgeable, and the inactive term follows
    /// the factor exactly (including the old exclude-inactive-entirely
    /// behavior at factor 0.0 - the CRITICAL posture and the pre-pressure
    /// fix).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_composition_scales_inactive_pages_by_the_pressure_factor() {
        let mut vm: libc::vm_statistics64_data_t = unsafe { std::mem::zeroed() };
        vm.free_count = 1_000;
        vm.speculative_count = 200;
        vm.purgeable_count = 50;
        // The contested category: 4x everything else combined.
        vm.inactive_count = 5_000;

        let page = 16_384_u64;
        let compose = |factor| pressure_adjusted_page_bytes(&vm, page, factor);
        // NORMAL (1.0): everything counts - the v1.0.0 starvation fix.
        assert_eq!(compose(1.0), (1_000 + 200 + 50 + 5_000) * page);
        // Unreadable (0.5): half the inactive credit.
        assert_eq!(compose(0.5), (1_250 + 2_500) * page);
        // WARN (0.25): a quarter.
        assert_eq!(compose(0.25), (1_250 + 1_250) * page);
        // CRITICAL (0.0): exactly the old conservative composition, and
        // never the over-counting one.
        assert_eq!(compose(0.0), (1_250) * page);
        assert_ne!(compose(0.0), (1_250 + 5_000) * page);
        // Zeroed counters (fresh boot edge) saturate to 0, never panic.
        let empty: libc::vm_statistics64_data_t = unsafe { std::mem::zeroed() };
        assert_eq!(pressure_adjusted_page_bytes(&empty, page, 1.0), 0);
    }

    /// The same page fixture flips the gate's verdict as the kernel's
    /// pressure verdict worsens: static 1755 MiB, inactive 2048 MiB, and
    /// Parakeet Unified EN Q8_0's 731 MiB forecast + the 1.5 GiB headroom
    /// (2267 MiB needed). NORMAL gives 3803 MiB (allow), WARN gives exactly
    /// 2267 MiB (the allow boundary), CRITICAL gives 1755 MiB (refuse).
    #[cfg(target_os = "macos")]
    #[test]
    fn gate_boundaries_flip_with_the_pressure_verdict() {
        let page = 16_384_u64;
        let mib_pages = |mib: u64| (mib * 1024 * 1024 / page) as libc::natural_t;
        let mut vm: libc::vm_statistics64_data_t = unsafe { std::mem::zeroed() };
        vm.free_count = mib_pages(1_000);
        vm.speculative_count = mib_pages(500);
        vm.purgeable_count = mib_pages(255);
        vm.inactive_count = mib_pages(2_048);

        let forecast = 731 * 1024 * 1024; // Parakeet Unified EN Q8_0 file
        let needed = forecast + DEFAULT_HEADROOM_BYTES;
        assert_eq!(needed, 2_267 * 1024 * 1024, "fixture must sit at the boundary");

        let compose = |factor| pressure_adjusted_page_bytes(&vm, page, factor);
        // NORMAL: 1755 + 2048 = 3803 MiB.
        assert_eq!(compose(1.0), 3_803 * 1024 * 1024);
        assert!(!gate_should_refuse(Some(compose(1.0)), forecast, DEFAULT_HEADROOM_BYTES));
        // WARN: 1755 + 512 = exactly the 2267 MiB requirement - the boundary
        // itself allows, one page less refuses.
        let warn_available = compose(0.25);
        assert_eq!(warn_available, needed);
        assert!(!gate_should_refuse(Some(warn_available), forecast, DEFAULT_HEADROOM_BYTES));
        assert!(gate_should_refuse(Some(warn_available - page), forecast, DEFAULT_HEADROOM_BYTES));
        // CRITICAL: static only, 1755 MiB.
        assert_eq!(compose(0.0), 1_755 * 1024 * 1024);
        assert!(gate_should_refuse(Some(compose(0.0)), forecast, DEFAULT_HEADROOM_BYTES));
    }

    /// SUCCESS CRITERION for the operator regression: pressure NORMAL with
    /// 4 GiB or more available must allow the Parakeet Unified EN Q8_0
    /// load (~2.2 GiB forecast + headroom with the default 1.5 GiB).
    #[test]
    fn normal_pressure_with_four_gib_available_allows_the_parakeet_q8_forecast() {
        let forecast = 731 * 1024 * 1024;
        let needed = forecast + DEFAULT_HEADROOM_BYTES;
        assert!(
            needed <= 4 * GIB,
            "parakeet q8 + default headroom must fit inside 4 GiB"
        );
        // The bytes-level gate: 4 GiB allows with ~1.8 GiB to spare, and the
        // actual refusal boundary sits at `needed` bytes (allow), one byte
        // under it (refuse).
        assert!(!gate_should_refuse(Some(4 * GIB), forecast, DEFAULT_HEADROOM_BYTES));
        assert!(!gate_should_refuse(Some(needed), forecast, DEFAULT_HEADROOM_BYTES));
        assert!(gate_should_refuse(Some(needed - 1), forecast, DEFAULT_HEADROOM_BYTES));

        // Through the composition too: a NORMAL-pressure reading assembled
        // from pages (2 GiB static + 2 GiB inactive = 4 GiB available)
        // allows the same load, where the CRITICAL posture would not.
        #[cfg(target_os = "macos")]
        {
            let page = 16_384_u64;
            let mib_pages = |mib: u64| (mib * 1024 * 1024 / page) as libc::natural_t;
            let mut vm: libc::vm_statistics64_data_t = unsafe { std::mem::zeroed() };
            vm.free_count = mib_pages(1_500);
            vm.speculative_count = mib_pages(412);
            vm.purgeable_count = mib_pages(136);
            vm.inactive_count = mib_pages(2_048);
            let normal = pressure_adjusted_page_bytes(&vm, page, 1.0);
            assert_eq!(normal, 4 * GIB);
            assert!(!gate_should_refuse(Some(normal), forecast, DEFAULT_HEADROOM_BYTES));
            let critical = pressure_adjusted_page_bytes(&vm, page, 0.0);
            assert_eq!(critical, 2 * GIB);
            assert!(gate_should_refuse(Some(critical), forecast, DEFAULT_HEADROOM_BYTES));
        }
    }

    /// Helper: candidate with an id, rank, and footprint.
    fn cand(id: &str, rank: u32, footprint_bytes: u64) -> FallbackCandidate {
        FallbackCandidate {
            id: id.to_string(),
            rank,
            footprint_bytes,
        }
    }

    // --- Probe calibration harness (informational) ---------------------------

    /// Read a vm page-count sysctl by name (macOS), returning pages.
    #[cfg(target_os = "macos")]
    fn sysctl_page_count(name: &std::ffi::CStr) -> Option<u64> {
        let mut value: libc::c_uint = 0;
        let mut len = std::mem::size_of::<libc::c_uint>() as libc::size_t;
        // SAFETY: sysctlbyname writing into a correctly sized buffer.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut value as *mut _ as *mut libc::c_void,
                &mut len as *mut libc::size_t,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(value as u64)
    }

    /// INFORMATIONAL MEASUREMENT HARNESS - not a pass/fail test.
    ///
    /// On this app's target machine `os_proc_available_memory()` returns 0, so
    /// every live memory decision uses the `host_statistics64` fallback:
    /// free + speculative + purgeable + inactive x factor, where the factor
    /// follows the kernel's own pressure verdict (inactive counted fully
    /// under NORMAL pressure, 0.25 under WARN, 0 under CRITICAL, 0.5 when
    /// the verdict is unreadable). Run it explicitly on the live box with:
    ///
    /// ```text
    /// cargo test --lib -- memory::tests::probe_calibration_snapshot --ignored --nocapture
    /// ```
    ///
    /// It prints (a) the probe's current free-RAM value, (b) the pressure
    /// verdict and the composition the fallback applied, (c) a ground-truth
    /// estimate computed independently from sysctl (vm.page_free_count +
    /// vm.page_speculative_count + vm.page_purgeable_count) x page size,
    /// (d) the inactive term and what each factor would credit, and (e) the
    /// delta and ratio between probe and ground truth. There are
    /// deliberately NO assertions: the values move with system load, so
    /// anything asserted here would flake on CI.
    ///
    /// Compiles on every platform; only macOS computes the sysctl ground
    /// truth, others print the probe alone.
    #[test]
    #[ignore = "informational measurement harness; run with --ignored --nocapture"]
    fn probe_calibration_snapshot() {
        const MIB: f64 = 1024.0 * 1024.0;
        let mib = |bytes: u64| format!("{:.1} MiB", bytes as f64 / MIB);

        let probe = probe_availability().available_bytes;
        match probe {
            Some(bytes) => println!("probe available bytes: {bytes} ({})", mib(bytes)),
            None => println!("probe available bytes: None (probe unavailable)"),
        }

        #[cfg(target_os = "macos")]
        {
            // The kernel's own pressure verdict and the inactive factor it
            // drives - the new half of the probe's behavior.
            let level = memory_pressure_level();
            let verdict = match level {
                Some(PRESSURE_LEVEL_NORMAL) => {
                    " (normal: inactive credited in full)".to_string()
                }
                Some(PRESSURE_LEVEL_WARN) => " (warn: inactive credited at 0.25)".to_string(),
                Some(PRESSURE_LEVEL_CRITICAL) => " (critical: inactive not credited)".to_string(),
                Some(other) => {
                    format!(" (unrecognized verdict {other}: treated as at-least-warn)")
                }
                None => " (unreadable: 0.5 middle-ground factor, fail open)".to_string(),
            };
            if let Some(level) = level {
                println!("kern.memorystatus_vm_pressure_level: {level}{verdict}");
            } else {
                println!("kern.memorystatus_vm_pressure_level: unreadable{verdict}");
            }

            // The raw kernel call and the pressure-scaled fallback it
            // degrades to, so the snapshot shows which branch of the probe
            // is live here and what the composition applied.
            let raw = unsafe { os_proc_available_memory() };
            println!(
                "os_proc_available_memory(): {raw}{}",
                if raw == 0 {
                    " (zero -> fallback is live)"
                } else {
                    ""
                }
            );
            let fallback = host_statistics_probe();
            match (fallback.available_bytes, fallback.inactive_factor) {
                (Some(bytes), Some(factor)) => println!(
                    "host_statistics64 composition: {bytes} ({}) [free+speculative+purgeable + inactive x {factor:.2}]",
                    mib(bytes)
                ),
                (Some(bytes), None) => println!(
                    "host_statistics64 composition: {bytes} ({}) [no inactive factor applied]",
                    mib(bytes)
                ),
                (None, _) => println!("host_statistics64 composition: probe failed"),
            }

            // Ground truth, computed independently via sysctl.
            let page = sysctl_page_count(c"hw.pagesize")
                .or_else(|| {
                    // hw.pagesize is normally available; fall back to sysconf.
                    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
                    (size > 0).then_some(size as u64)
                })
                .expect("page size unavailable");
            let free = sysctl_page_count(c"vm.page_free_count");
            let speculative = sysctl_page_count(c"vm.page_speculative_count");
            let purgeable = sysctl_page_count(c"vm.page_purgeable_count");
            let inactive = sysctl_page_count(c"vm.page_inactive_count");
            println!(
                "sysctl pages (page size {page}): free={free:?} speculative={speculative:?} purgeable={purgeable:?} inactive={inactive:?}"
            );
            if let Some(inactive) = inactive {
                let inactive_bytes = inactive.saturating_mul(page);
                println!(
                    "inactive term ({}) credited by the matrix: normal {:.1}, unreadable {:.1}, warn {:.1}, critical {:.1}",
                    mib(inactive_bytes),
                    inactive_bytes as f64 / MIB,
                    0.5 * inactive_bytes as f64 / MIB,
                    0.25 * inactive_bytes as f64 / MIB,
                    0.0,
                );
            }

            if let (Some(free), Some(speculative)) = (free, speculative) {
                // vm.page_purgeable_count cannot be read via sysctl(3) on
                // every macOS version: on macOS 26 sysctlbyname fails with
                // ENOMEM for a 4-byte buffer and returns 0 for an 8-byte one
                // (measured), even though the sysctl CLI reports a value -
                // the counter is effectively unmeasurable from this process.
                // Count it as zero and say so rather than skipping the
                // measurement.
                let purgeable_pages = purgeable.unwrap_or(0);
                if purgeable.is_none() {
                    println!("note: vm.page_purgeable_count sysctl unreadable here; ground truth treated as 0");
                }
                let ground = (free + speculative + purgeable_pages).saturating_mul(page);
                println!(
                    "ground truth (free+speculative+purgeable x page): {ground} ({})",
                    mib(ground)
                );
                if let Some(probe_bytes) = probe {
                    if ground > 0 {
                        let delta_bytes = probe_bytes as i64 - ground as i64;
                        let ratio = probe_bytes as f64 / ground as f64;
                        println!(
                            "delta (probe - ground truth): {delta_bytes} bytes ({:+.1} MiB)",
                            delta_bytes as f64 / MIB
                        );
                        println!("ratio (probe / ground truth): {ratio:.3}");
                        if purgeable.is_none() {
                            println!(
                                "note: the probe also counts purgeable pages (from the \
                                 vm_statistics64 struct) and the pressure-scaled inactive \
                                 credit, which this static ground truth does not - under \
                                 NORMAL pressure expect the delta to track the inactive \
                                 term, which is the intended behavior, not an over-count"
                            );
                        }
                    } else {
                        println!("ground truth is zero; ratio undefined");
                    }
                }
            } else {
                println!("sysctl vm.page_* counters unavailable; no ground truth computed");
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            println!("no independent sysctl ground truth on this platform; probe-only snapshot");
        }
    }
}
