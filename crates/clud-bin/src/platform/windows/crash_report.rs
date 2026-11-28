//! Windows half of `crate::crash_report`: label the NTSTATUS a
//! `kernal_api::crash` spool record carries (sign-extended from `i32`).

use crate::crash_report::{hex_addr, windows_exception_name, NativeCrashMeta};

pub(crate) fn decode_fault(code: i64, addr: u64) -> NativeCrashMeta {
    NativeCrashMeta {
        signal_or_exception: Some(windows_exception_name(code).to_string()),
        signal_number: None,
        exception_code: Some(code),
        faulting_address: hex_addr(addr as usize),
    }
}
