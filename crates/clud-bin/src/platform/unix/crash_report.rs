//! Unix half of `crate::crash_report`: label the fault code a
//! `kernal_api::crash` spool record carries.
//!
//! The record stores the platform's raw code with no namespace tag. On Linux
//! that is the signal number. On macOS it is the Mach exception kind for a
//! hardware fault, but the raw signal number for the SIGABRT chain, and `6`
//! means both `EXC_BREAKPOINT` and `SIGABRT`, so the label says so rather
//! than guessing.

use crate::crash_report::{hex_addr, NativeCrashMeta};

#[cfg(target_os = "macos")]
use crate::crash_report::mac_exception_kind_name;
#[cfg(not(target_os = "macos"))]
use crate::crash_report::signal_name;

#[cfg(not(target_os = "macos"))]
pub(crate) fn decode_fault(code: i64, addr: u64) -> NativeCrashMeta {
    let signo = i32::try_from(code).unwrap_or(0);
    NativeCrashMeta {
        signal_or_exception: Some(signal_name(signo).to_string()),
        signal_number: Some(signo),
        exception_code: None,
        faulting_address: hex_addr(addr as usize),
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn decode_fault(code: i64, addr: u64) -> NativeCrashMeta {
    let kind = u32::try_from(code).unwrap_or(0);
    let label = if kind == 6 {
        "EXC_BREAKPOINT_OR_SIGABRT"
    } else {
        mac_exception_kind_name(kind)
    };
    NativeCrashMeta {
        signal_or_exception: Some(label.to_string()),
        signal_number: Some(kind as i32),
        exception_code: Some(code),
        faulting_address: hex_addr(addr as usize),
    }
}

/// The fault code and label this platform's record carries for a segfault,
/// for host-neutral tests of the record -> report conversion.
#[cfg(all(test, not(target_os = "macos")))]
pub(crate) const SEGV_FAULT: (i64, &str) = (11, "SIGSEGV");
#[cfg(all(test, target_os = "macos"))]
pub(crate) const SEGV_FAULT: (i64, &str) = (1, "EXC_BAD_ACCESS");
