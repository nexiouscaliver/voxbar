use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

// Define the response structure from Swift
#[repr(C)]
pub struct AppleLLMResponse {
    pub response: *mut c_char,
    pub success: c_int,
    pub error_message: *mut c_char,
}

// Link to the Swift functions
extern "C" {
    pub fn is_apple_intelligence_available() -> c_int;
    pub fn free_apple_llm_response(response: *mut AppleLLMResponse);
}

// Safe wrapper functions
pub fn check_apple_intelligence_availability() -> bool {
    unsafe { is_apple_intelligence_available() == 1 }
}

/// KB-262: WHY Apple Intelligence is (un)available, as a tri-state the
/// frontend localizes differently. A stub-linked build ("this copy of
/// VoxBar was compiled without it") is a different message than an OS
/// that lacks it ("requires macOS Tahoe..."), but the bool probe returned
/// the same false for both, so a build-lacks Mac showed the OS
/// requirements copy - and the stub's own distinct string never reached
/// the availability path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum AppleIntelligenceStatus {
    Available,
    OsUnsupported,
    BuildLacks,
}

/// The tri-state probe behind the check_apple_intelligence_available
/// command. Platform gating is internal so callers (and the command) stay
/// cfg-free:
/// - Apple Silicon macOS: the build script links the stub swift exactly
///   when the toolchain cannot compile FoundationModels (build.rs emits
///   the apple_intelligence_stub cfg on the stub arm only), so a
///   stub-linked build answers BuildLacks without a runtime probe - the
///   stub's is_apple_intelligence_available is a constant 0 that carries
///   no build-versus-OS information. A real-linked build probes the OS.
/// - Every other target (Intel macOS, Linux, Windows): the OS cannot run
///   Apple Intelligence at all.
pub fn check_apple_intelligence_status() -> AppleIntelligenceStatus {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        #[cfg(apple_intelligence_stub)]
        {
            AppleIntelligenceStatus::BuildLacks
        }
        #[cfg(not(apple_intelligence_stub))]
        {
            if check_apple_intelligence_availability() {
                AppleIntelligenceStatus::Available
            } else {
                AppleIntelligenceStatus::OsUnsupported
            }
        }
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        AppleIntelligenceStatus::OsUnsupported
    }
}

// Link to the Swift function for system prompt support
extern "C" {
    pub fn process_text_with_system_prompt_apple(
        system_prompt: *const c_char,
        user_content: *const c_char,
        max_tokens: i32,
    ) -> *mut AppleLLMResponse;
}

/// Process text with Apple Intelligence using separate system prompt and user content
pub fn process_text_with_system_prompt(
    system_prompt: &str,
    user_content: &str,
    max_tokens: i32,
) -> Result<String, String> {
    let system_cstr = CString::new(system_prompt).map_err(|e| e.to_string())?;
    let user_cstr = CString::new(user_content).map_err(|e| e.to_string())?;

    let response_ptr = unsafe {
        process_text_with_system_prompt_apple(system_cstr.as_ptr(), user_cstr.as_ptr(), max_tokens)
    };

    if response_ptr.is_null() {
        return Err("Null response from Apple LLM".to_string());
    }

    let response = unsafe { &*response_ptr };

    let result = if response.success == 1 {
        if response.response.is_null() {
            Ok(String::new())
        } else {
            let c_str = unsafe { CStr::from_ptr(response.response) };
            let rust_str = c_str.to_string_lossy().into_owned();
            Ok(rust_str)
        }
    } else {
        let error_c_str = if !response.error_message.is_null() {
            unsafe { CStr::from_ptr(response.error_message) }
        } else {
            c"Unknown error"
        };
        let error_msg = error_c_str.to_string_lossy().into_owned();
        Err(error_msg)
    };

    // Clean up the response
    unsafe { free_apple_llm_response(response_ptr) };

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_availability() {
        let available = check_apple_intelligence_availability();
        println!("Apple Intelligence available: {}", available);
    }

    // KB-262: the probe must answer with the tri-state, and the answer is
    // compile-time deterministic per target: a stub-linked build always
    // reports BuildLacks (never the OS-unsupported verdict the bool probe
    // forced it into), a real-linked build reports the OS verdict, and
    // non-Apple-Silicon targets report OsUnsupported.
    #[test]
    fn availability_probe_reports_the_tri_state_kb262() {
        let status = check_apple_intelligence_status();
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            #[cfg(apple_intelligence_stub)]
            assert_eq!(status, AppleIntelligenceStatus::BuildLacks);
            #[cfg(not(apple_intelligence_stub))]
            assert!(
                matches!(
                    status,
                    AppleIntelligenceStatus::Available
                        | AppleIntelligenceStatus::OsUnsupported
                ),
                "a real-linked build must answer from the OS probe, got {:?}",
                status
            );
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        assert_eq!(status, AppleIntelligenceStatus::OsUnsupported);
    }

    // KB-262 source pin: the BuildLacks verdict is only as good as the
    // build script flagging its stub arm. build.rs must emit the
    // apple_intelligence_stub rustc-cfg EXACTLY ONCE, inside the stub
    // (else) arm of the real-vs-stub selection and before the stub's tail
    // expression, so a real-linked build never sets it. Pinned on the
    // source (the established convention when the real seam cannot run
    // under a test): the selection block cannot move without tripping the
    // anchors.
    #[test]
    fn build_script_flags_the_stub_arm_for_the_probe_kb262() {
        let build_script = include_str!("../build.rs");
        let cfg_line = "cargo:rustc-cfg=apple_intelligence_stub";
        let first = build_script
            .find(cfg_line)
            .expect("build.rs must emit the apple_intelligence_stub cfg");
        let last = build_script.rfind(cfg_line).expect("cfg line present");
        assert_eq!(
            first, last,
            "the cfg must be emitted exactly once, on one selection arm"
        );
        // It sits inside the stub arm: after the selection's else opens
        // and before the STUB_SWIFT_FILE tail expression it guards.
        let selection = "let source_file = if has_foundation_models {";
        let selection_at = build_script
            .find(selection)
            .expect("the real-vs-stub selection must stay in one piece");
        let else_at = build_script[selection_at..]
            .find("} else {")
            .map(|at| selection_at + at)
            .expect("the selection must keep its else (stub) arm");
        let stub_tail = build_script[selection_at..]
            .find("STUB_SWIFT_FILE\n    };")
            .map(|at| selection_at + at)
            .expect("the stub arm must keep the STUB_SWIFT_FILE tail");
        assert!(
            first > else_at && first < stub_tail,
            "the cfg emission must stay inside the stub arm, between its \
             opening and the STUB_SWIFT_FILE tail"
        );
        // And never inside the real arm (which closes before the else).
        assert!(
            first > else_at,
            "a real-linked build must never receive the cfg"
        );
    }
}
