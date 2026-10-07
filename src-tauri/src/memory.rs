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

/// Best-effort system-wide available RAM in bytes, or `None` when the probe
/// fails - callers must FAIL OPEN on `None`.
///
/// - macOS: `os_proc_available_memory()` (libc does not bind it, so the
///   extern is declared here; it is available on macOS 11+, this app's
///   effective deployment floor - rustc links with
///   `-mmacosx-version-min=11.0.0`). Deliberately NOT the naive
///   `host_statistics64` free page count, which under-reports memory the
///   kernel can reclaim. FALLBACK: on this app's actual target machine
///   (macOS 26 / darwin 25.6) `os_proc_available_memory()` was measured
///   returning 0 for ordinary processes (verified from plain C, outside any
///   sandbox, this session), so a zero reading falls back to the calibrated
///   free+speculative+purgeable `host_statistics64` sum (see
///   [`fallback_page_bytes`]) instead of poisoning the gate with
///   "0 bytes free".
/// - Linux: `/proc/meminfo` `MemAvailable` (the kernel's own reclaim
///   estimate).
/// - Windows: `GlobalMemoryStatusEx` `ullAvailPhys`.
pub fn available_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: extern with no arguments returning a plain u64 from
        // libSystem; thread-safe and allocation-free.
        let proc = unsafe { os_proc_available_memory() };
        if proc > 0 {
            Some(proc)
        } else {
            host_statistics_available()
        }
    }
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in statm.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: writable pointer to a correctly-sized struct of the
        // expected type; GlobalMemoryStatusEx only fills it in.
        unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
        Some(status.ullAvailPhys)
    }
}

/// The pure page-sum at the heart of the macOS fallback probe:
/// free + speculative + purgeable, deliberately EXCLUDING inactive pages.
///
/// Calibration on the target machine (see `probe_calibration_snapshot`)
/// measured the previous free+inactive+purgeable+speculative sum at 4.82x
/// the sysctl ground truth: inactive pages (~4 GiB on a 24 GiB box) are
/// cached data the kernel reclaims only slowly under real pressure, so
/// counting them made the memory gate and the RAM auto-fallback wildly
/// optimistic - the fallback would almost never fire even with RAM
/// genuinely low. This composition matches the harness's ground truth
/// (`vm.page_free_count` + `vm.page_speculative_count` + purgeable, x page
/// size). `vm_statistics64` does carry `purgeable_count`, so it is counted;
/// had it been missing, the correct degradation is free+speculative only -
/// never a silent reintroduction of inactive.
#[cfg(target_os = "macos")]
fn fallback_page_bytes(vm: &libc::vm_statistics64, page_size: u64) -> u64 {
    (vm.free_count as u64 + vm.speculative_count as u64 + vm.purgeable_count as u64)
        .saturating_mul(page_size)
}

/// macOS fallback probe: available RAM from `host_statistics64`, in bytes -
/// [`fallback_page_bytes`] over the kernel's page counters. Conservative by
/// design (inactive pages are NOT counted as available); the gate's headroom
/// absorbs the remaining optimism from speculative/purgeable pages.
#[cfg(target_os = "macos")]
fn host_statistics_available() -> Option<u64> {
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
        return None;
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    Some(fallback_page_bytes(&vm, page))
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
        let available = available_memory_bytes().expect("probe returned None");
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
        let available = available_memory_bytes().expect("probe returned None");
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

    /// Fixture test for [`fallback_page_bytes`]: the composition is
    /// free + speculative + purgeable and inactive is excluded (see the
    /// function's doc for the calibration rationale).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_fallback_page_sum_excludes_inactive_pages() {
        let mut vm: libc::vm_statistics64_data_t = unsafe { std::mem::zeroed() };
        vm.free_count = 1_000;
        vm.speculative_count = 200;
        vm.purgeable_count = 50;
        // The over-counted category: 5x everything else combined. Counting it
        // was the 4.82x calibration error.
        vm.inactive_count = 5_000;

        let page = 16_384_u64;
        assert_eq!(
            fallback_page_bytes(&vm, page),
            (1_000 + 200 + 50) * page,
            "must count free + speculative + purgeable"
        );
        assert_ne!(
            fallback_page_bytes(&vm, page),
            (1_000 + 200 + 50 + 5_000) * page,
            "inactive pages must NOT be counted as available"
        );
        // Zeroed counters (fresh boot edge) saturate to 0, never panic.
        let empty: libc::vm_statistics64_data_t = unsafe { std::mem::zeroed() };
        assert_eq!(fallback_page_bytes(&empty, page), 0);
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
    /// every live memory decision uses the `host_statistics64` fallback
    /// (free + speculative + purgeable; inactive pages were dropped after
    /// this harness measured them at 4.82x the sysctl ground truth). Run it
    /// explicitly on the live box with:
    ///
    /// ```text
    /// cargo test --lib -- memory::tests::probe_calibration_snapshot --ignored --nocapture
    /// ```
    ///
    /// It prints (a) the probe's current free-RAM value, (b) a ground-truth
    /// estimate computed independently from sysctl
    /// (vm.page_free_count + vm.page_speculative_count +
    /// vm.page_purgeable_count) x page size, and (c) the delta and ratio
    /// between the two. There are deliberately NO assertions: the values move
    /// with system load, so anything asserted here would flake on CI. Read
    /// the printed ratio to judge whether the fallback's optimism (counting
    /// inactive pages) stays within what the gate's headroom absorbs.
    ///
    /// Compiles on every platform; only macOS computes the sysctl ground
    /// truth, others print the probe alone.
    #[test]
    #[ignore = "informational measurement harness; run with --ignored --nocapture"]
    fn probe_calibration_snapshot() {
        const MIB: f64 = 1024.0 * 1024.0;
        let mib = |bytes: u64| format!("{:.1} MiB", bytes as f64 / MIB);

        let probe = available_memory_bytes();
        match probe {
            Some(bytes) => println!("probe available_memory_bytes(): {bytes} ({})", mib(bytes)),
            None => println!("probe available_memory_bytes(): None (probe unavailable)"),
        }

        #[cfg(target_os = "macos")]
        {
            // The raw kernel call and the fallback it degrades to, so the
            // snapshot shows which branch of the probe is live here.
            let raw = unsafe { os_proc_available_memory() };
            println!(
                "os_proc_available_memory(): {raw}{}",
                if raw == 0 {
                    " (zero -> fallback is live)"
                } else {
                    ""
                }
            );
            match host_statistics_available() {
                Some(bytes) => println!(
                    "host_statistics64 fallback: {bytes} ({}) [free+speculative+purgeable]",
                    mib(bytes)
                ),
                None => println!("host_statistics64 fallback: None"),
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
            println!(
                "sysctl pages (page size {page}): free={free:?} speculative={speculative:?} purgeable={purgeable:?}"
            );

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
                                "note: the probe still counts purgeable pages (from the \
                                 vm_statistics64 struct), which this ground truth cannot \
                                 measure here - expect the delta to equal the probe's \
                                 purgeable component, NOT an inactive over-count"
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
