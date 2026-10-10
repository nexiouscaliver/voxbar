//! A thin Rust seam that carries update-flow decisions into voxbar.log.
//!
//! The updater plugin is registered here in Rust, but every check is
//! FRONTEND-driven (src/components/update-checker/updaterFlow.ts), and
//! webview console output never reaches the file log. Without this seam
//! the file log cannot answer "did the app check for updates, and what did
//! the user do about it" (the observed double check at boot was
//! unexplainable from voxbar.log alone). The frontend calls this once per
//! decision point; each call writes exactly one line.

/// The one line each decision writes. Detail is frontend-supplied (often an
/// error message), so newlines collapse to spaces to keep the promise of
/// one line per call.
fn update_decision_line(stage: &str, detail: Option<&str>) -> String {
    let detail = detail.unwrap_or("-").replace(['\n', '\r'], " ");
    format!("update decision: stage={stage} detail={detail}")
}

#[tauri::command]
#[specta::specta]
pub fn log_update_decision(stage: String, detail: Option<String>) {
    log::info!("{}", update_decision_line(&stage, detail.as_deref()));
}

#[cfg(test)]
mod tests {
    use super::update_decision_line;

    /// One line per call, with and without detail, and a multi-line error
    /// detail collapses instead of splitting the log line.
    #[test]
    fn update_decision_writes_one_line_per_call() {
        assert_eq!(
            update_decision_line("check_started", None),
            "update decision: stage=check_started detail=-"
        );
        assert_eq!(
            update_decision_line("offered", Some("1.3.0")),
            "update decision: stage=offered detail=1.3.0"
        );
        let multi = update_decision_line("install_failed", Some("boom\nsecond line"));
        assert_eq!(
            multi,
            "update decision: stage=install_failed detail=boom second line"
        );
        assert!(!multi.contains('\n'));
        assert!(!multi.contains('\r'));
    }
}
