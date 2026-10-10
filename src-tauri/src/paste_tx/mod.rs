//! Receipt-sequenced clipboard paste ("reliable paste", debug-gated).
//!
//! The legacy clipboard paste (`clipboard::paste_via_clipboard`) restores the
//! previous clipboard after a fixed delay. The paste keystroke is only
//! *enqueued* at that point - the target application reads the clipboard
//! whenever its event loop gets to it, so any fixed delay can lose the race
//! and the user gets their old clipboard pasted back (#502).
//!
//! This module instead publishes the transcript as a *lazy promise* and waits
//! for the operating system to tell us that a consumer actually read the
//! clipboard - a "receipt" - before restoring:
//!
//! - Windows: delayed rendering (`SetClipboardData(CF_UNICODETEXT, NULL)`),
//!   the owner window receives `WM_RENDERFORMAT` on read.
//! - macOS: `declareTypes:owner:` with an owner object, the pasteboard calls
//!   `pasteboard:provideDataForType:` on read.
//!
//! Two rules make the receipt trustworthy:
//!
//! 1. Only receipts observed *after* the paste chord was injected count. A
//!    read before that is an eager third party (clipboard manager, antivirus)
//!    reacting to the clipboard change itself.
//! 2. Restoration only happens while we still own the clipboard
//!    (sequence number / changeCount unchanged, no ownership-lost event). If
//!    the user copied something else in the meantime, their action wins.
//!
//! The restore is additionally gated on a short quiet period after the *last*
//! receipt, because some applications read the clipboard several times per
//! paste (Chromium probes, then reads). A bounded timeout caps how long the
//! transcript may occupy the clipboard; the failure mode is always "the
//! transcript stays on the clipboard a bit longer", never "stale content gets
//! pasted".

// The shared transaction state is compiled on all platforms (for the unit
// tests), but only the macOS/Windows platform modules consume all of it.
#![cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]

use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

pub mod key_send;

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

/// How long after the *last* observed read the transcript stays on the
/// clipboard before restoring. Covers applications that read the clipboard
/// several times per paste (e.g. Chromium probe-then-read).
pub(crate) const QUIET_PERIOD: Duration = Duration::from_millis(200);

/// Upper bound on how long the transcript may occupy the clipboard before we
/// restore regardless of receipts. Long enough that a realistically loaded
/// target always gets to read first; short enough that a lost keystroke does
/// not strand the transcript on the clipboard for long.
pub(crate) const RESTORE_TIMEOUT: Duration = Duration::from_secs(8);

/// When the chord could not be injected at all, no legitimate receipt can
/// arrive, so restore quickly instead of waiting out the full timeout.
pub(crate) const FAILED_INJECTION_TIMEOUT: Duration = Duration::from_millis(500);

/// KB-223: stable prefix marking "the transcript was published, but the paste
/// chord could not be injected" - the paste failed. Unlike the pre-publish
/// `Err`s (nothing happened, fall back to the legacy paste), this failure
/// leaves a live transaction behind: the transcript sits on the clipboard as
/// a promise and the waiter owns the restore after
/// [`FAILED_INJECTION_TIMEOUT`]. The caller classifies on this prefix the
/// same way the settings UI classifies the refusal prefixes, and reports the
/// error as the paste failure instead of falling back over the live
/// transaction.
pub(crate) const CHORD_FAILURE_PREFIX: &str = "paste-chord-failed";

/// KB-223: builds the chord-failure error every platform returns when
/// `send_chord` fails. One constructor so the prefix and the wording stay in
/// sync with [`is_chord_failure`].
pub(crate) fn chord_failure_error(cause: &str) -> String {
    format!("{CHORD_FAILURE_PREFIX}: failed to send paste chord: {cause}")
}

/// KB-223: whether a `try_reliable_paste` error is the chord failure (the
/// transcript was published but nothing was pasted) rather than a pre-publish
/// "cannot start" error the caller may safely retry via the legacy path.
pub(crate) fn is_chord_failure(error: &str) -> bool {
    error.starts_with(CHORD_FAILURE_PREFIX)
}

/// KB-223: whether settling a transaction owes the auto-submit Enter. A
/// receipt alone is not enough once the chord failed: no paste happened, so
/// an Enter could submit whatever the target field held before, and the
/// failure is surfaced as a paste error instead. Pure so both platforms'
/// settle paths share one table.
pub(crate) fn auto_submit_owed(
    auto_submit: bool,
    receipt_seen: bool,
    injection_failed: bool,
) -> bool {
    auto_submit && receipt_seen && !injection_failed
}

/// Shared, cross-thread record of one paste transaction.
#[derive(Debug)]
pub(crate) struct TxState {
    /// When the transcript was published to the clipboard.
    pub published_at: Instant,
    /// When the paste chord was injected. Only receipts *after* this count as
    /// evidence the target read the transcript - earlier reads are eager third
    /// parties reacting to the clipboard change itself.
    pub injected_at: Option<Instant>,
    /// The chord could not be sent; short-circuit the wait.
    pub injection_failed: bool,
    /// Times at which a consumer requested the clipboard data.
    pub receipts: Vec<Instant>,
    /// Someone else took clipboard ownership (user copied elsewhere, ...).
    pub ownership_lost: bool,
    /// A newer paste transaction settled this one early (see flush logic in
    /// the platform modules).
    pub cancelled: bool,
    /// The post-paste Enter (auto-submit) has been sent for this transaction.
    /// (Read on Windows; the macOS path settles via `MacPending::settled`.)
    #[allow(dead_code)]
    pub auto_submit_sent: bool,
    /// First post-injection receipt has been logged.
    pub logged_receipt: bool,
}

impl TxState {
    pub fn new() -> Self {
        Self {
            published_at: Instant::now(),
            injected_at: None,
            injection_failed: false,
            receipts: Vec::new(),
            ownership_lost: false,
            cancelled: false,
            auto_submit_sent: false,
            logged_receipt: false,
        }
    }

    /// Records a read receipt, logging the first one that counts as evidence.
    pub fn record_receipt(&mut self, at: Instant) {
        self.receipts.push(at);
        if !self.logged_receipt {
            if let Some(injected) = self.injected_at {
                if at >= injected {
                    self.logged_receipt = true;
                    log::info!(
                        "[reliable-paste] clipboard read {}ms after chord",
                        at.duration_since(injected).as_millis()
                    );
                }
            }
        }
    }

    pub fn last_receipt_after_injection(&self) -> Option<Instant> {
        let injected = self.injected_at?;
        self.receipts.iter().copied().rev().find(|t| *t >= injected)
    }

    pub fn any_receipt_after_injection(&self) -> bool {
        self.last_receipt_after_injection().is_some()
    }
}

pub(crate) enum WaitDecision {
    KeepWaiting,
    /// Stop waiting; settle the transaction (auto-submit + guarded restore).
    Finish,
}

/// Pure decision: given the current transaction state, keep waiting for the
/// target to read, or finish now. Both platform event loops call this.
pub(crate) fn evaluate(state: &TxState, now: Instant) -> WaitDecision {
    if state.ownership_lost || state.cancelled {
        return WaitDecision::Finish;
    }
    if let Some(last) = state.last_receipt_after_injection() {
        if now.duration_since(last) >= QUIET_PERIOD {
            return WaitDecision::Finish;
        }
    }
    let deadline = if state.injection_failed {
        FAILED_INJECTION_TIMEOUT
    } else {
        RESTORE_TIMEOUT
    };
    if now.duration_since(state.published_at) >= deadline {
        return WaitDecision::Finish;
    }
    WaitDecision::KeepWaiting
}

/// Modifier hold for the receipt-sequenced chord, kept at parity with the
/// legacy path (100ms) for the beta: that hold was added in #165 because real
/// users' systems dropped chords released too quickly, and the beta should
/// validate the receipt mechanism without changing a second variable.
///
/// Once receipts are proven in the field this becomes a safe tuning knob - a
/// chord the target never recognizes produces no receipt and is logged ("no
/// read within timeout") rather than failing silently, so a shorter hold
/// (measured working at 10ms on a fast machine, cutting visible latency from
/// ~110ms to ~20ms) can be tried as its own experiment later.
const CHORD_HOLD_MS: u64 = 100;

/// Sends the platform paste chord for the configured method.
pub(crate) fn send_chord(
    enigo: &mut enigo::Enigo,
    paste_method: &crate::settings::PasteMethod,
) -> Result<(), String> {
    use crate::settings::PasteMethod;
    match paste_method {
        PasteMethod::CtrlV => crate::input::send_paste_ctrl_v(enigo, CHORD_HOLD_MS),
        PasteMethod::CtrlShiftV => crate::input::send_paste_ctrl_shift_v(enigo, CHORD_HOLD_MS),
        PasteMethod::ShiftInsert => crate::input::send_paste_shift_insert(enigo, CHORD_HOLD_MS),
        other => Err(format!(
            "Invalid paste method for clipboard paste: {:?}",
            other
        )),
    }
}

/// Attempts the receipt-sequenced paste. Returns `Err` in two shapes:
/// - Before anything has been published (the platform transaction cannot
///   start): the caller should fall back to the legacy paste path.
/// - Carrying [`CHORD_FAILURE_PREFIX`] (KB-223): the transcript was published
///   but the paste chord could not be injected, so nothing was pasted. The
///   transaction stays live - its waiter restores the clipboard (or leaves
///   the transcript, per the clipboard handling) after
///   [`FAILED_INJECTION_TIMEOUT`] - so the caller must NOT fall back over it;
///   the error is the paste failure to surface (the Wave-1 `paste_failed`
///   notice). On `Ok`, publishing and chord injection have completed and the
///   guarded restore (plus auto-submit) finishes asynchronously.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn try_reliable_paste(
    text: &str,
    app_handle: &tauri::AppHandle,
    paste_method: &crate::settings::PasteMethod,
    enigo: &mut enigo::Enigo,
    auto_submit: bool,
    auto_submit_key: crate::settings::AutoSubmitKey,
    clipboard_handling: crate::settings::ClipboardHandling,
) -> Result<(), String> {
    platform::run(
        text,
        app_handle,
        paste_method,
        enigo,
        auto_submit,
        auto_submit_key,
        clipboard_handling,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_after_publish(published_ago: Duration) -> TxState {
        let mut s = TxState::new();
        s.published_at = Instant::now() - published_ago;
        s
    }

    #[test]
    fn keeps_waiting_without_receipt_within_timeout() {
        let s = state_after_publish(Duration::from_millis(100));
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn finishes_after_quiet_period_once_read() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(250));
        s.receipts.push(Instant::now() - QUIET_PERIOD);
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn waits_through_quiet_period_after_recent_read() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(100));
        s.receipts.push(Instant::now() - Duration::from_millis(50));
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn pre_injection_receipt_does_not_count() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.receipts.push(Instant::now() - Duration::from_millis(200));
        s.injected_at = Some(Instant::now() - Duration::from_millis(100));
        assert!(!s.any_receipt_after_injection());
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn finishes_on_timeout_without_receipt() {
        let s = state_after_publish(RESTORE_TIMEOUT);
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn failed_injection_uses_short_timeout() {
        let mut s = state_after_publish(FAILED_INJECTION_TIMEOUT);
        s.injection_failed = true;
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn ownership_loss_finishes_immediately() {
        let mut s = state_after_publish(Duration::from_millis(10));
        s.ownership_lost = true;
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    // ------------------------------------------------------------------
    // KB-223: chord-failure reporting and the auto-submit it owes
    // ------------------------------------------------------------------

    /// The chord failure carries the stable prefix the caller classifies on,
    /// and the pre-publish "cannot start" errors do not: those still mean
    /// "fall back to the legacy paste", while a chord failure must be
    /// reported as the paste failure instead.
    #[test]
    fn chord_failure_is_classifiable_and_pre_publish_errors_are_not() {
        assert!(is_chord_failure(&chord_failure_error(
            "Failed to press Control key: input error"
        )));
        // The real pre-publish failure strings from both platforms.
        assert!(!is_chord_failure("declareTypes:owner: failed"));
        assert!(!is_chord_failure("OpenClipboard failed: clipboard busy"));
        assert!(!is_chord_failure(
            "reliable paste worker died before publishing"
        ));
    }

    /// KB-223: the auto-submit Enter is owed only on the success path. A
    /// chord failure owes nothing even if an eager third-party read got
    /// recorded after the injection mark - no paste happened, so an Enter
    /// could submit stale content.
    #[test]
    fn auto_submit_is_not_owed_after_a_chord_failure() {
        assert!(auto_submit_owed(true, true, false), "success owes it");
        assert!(
            !auto_submit_owed(true, false, false),
            "no receipt means unconfirmed paste"
        );
        assert!(
            !auto_submit_owed(false, true, false),
            "auto-submit disabled owes nothing"
        );
        assert!(
            !auto_submit_owed(true, true, true),
            "a chord failure owes no Enter, stray receipt or not"
        );
    }
}
