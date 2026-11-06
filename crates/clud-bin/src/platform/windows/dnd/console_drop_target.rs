//! Windows `IDropTarget` COM wiring for
//! [`crate::dnd::console_drop_target`]: the `OleInitialize` worker thread,
//! `RegisterDragDrop` displacement, the `IDropTarget` vtable and the
//! `CF_HDROP` extraction. Moved here from the `win` submodule of
//! `dnd/console_drop_target.rs` (#1953); the platform-agnostic dispatch,
//! effect decision and registration loop stay in that neutral module.

use crate::dnd::console_drop_target::{
    dispatch_dropfiles_to_injector, drag_effect, run_registration_loop, ConsoleDropTargetGuard,
    DragDropRegistrar, DropInjector, GuardInner, KeepAliveTrait, RefreshConfig, RefreshShutdown,
    RegisterError, DROPEFFECT_COPY_BITS,
};
use crate::dnd::drop_host::{resolve_drop_host, DropHost, ProcessEntry};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use windows::core::ComObject;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINTL, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Console::GetConsoleWindow;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
    RevokeDragDrop, DROPEFFECT,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
};

/// COM object implementing `IDropTarget`. Advertises
/// `DROPEFFECT_COPY` for a file drag (so the cursor switches from ⊘
/// to a copy indicator) and `DROPEFFECT_NONE` for anything else,
/// per [`drag_effect`]; on `Drop()` it extracts the
/// `CF_HDROP` bytes and hands them to
/// [`dispatch_dropfiles_to_injector`].
#[windows::core::implement(IDropTarget)]
struct ConsoleDropTarget {
    injector: Mutex<DropInjector>,
    /// Whether the data object seen by the current `DragEnter`
    /// offers `CF_HDROP`. `DragOver` gets no data object, so it
    /// reuses this; `DragLeave` and `Drop` clear it.
    carries_files: AtomicBool,
}

/// Build the `IDropTarget` clud registers. Split out of
/// [`register`] so tests can drive the real COM vtable without a
/// console window or `RegisterDragDrop` (#1362).
pub(super) fn new_drop_target(injector: DropInjector) -> IDropTarget {
    ComObject::new(ConsoleDropTarget {
        injector: Mutex::new(injector),
        carries_files: AtomicBool::new(false),
    })
    .to_interface::<IDropTarget>()
}

/// The effects the drag source allows, read from the in/out
/// `pdwEffect`. A null pointer violates the `IDropTarget` contract;
/// treat it as "copy allowed" (the result cannot be written back
/// anyway).
///
/// # Safety
///
/// `effect` is null or points to a readable `DROPEFFECT`.
unsafe fn source_allowed(effect: *const DROPEFFECT) -> u32 {
    if effect.is_null() {
        DROPEFFECT_COPY_BITS
    } else {
        // SAFETY: non-null per the check above; caller guarantees
        // it is readable.
        unsafe { (*effect).0 }
    }
}

/// # Safety
///
/// `effect` is null or points to a writable `DROPEFFECT`.
unsafe fn write_effect(effect: *mut DROPEFFECT, bits: u32) {
    if !effect.is_null() {
        // SAFETY: non-null per the check above; caller guarantees
        // it is writable.
        unsafe { *effect = DROPEFFECT(bits) };
    }
}

impl ConsoleDropTarget {
    /// Deliver a dropped payload. Returns whether the injector was
    /// called with at least one path.
    fn deliver(&self, data: &IDataObject) -> bool {
        // SAFETY: `data` is a live interface borrowed from OLE for
        // the duration of `Drop`; STGMEDIUM bookkeeping is handled
        // inside.
        let Some(buf) = (unsafe { copy_cf_hdrop_bytes(data) }) else {
            return false;
        };
        let Ok(guard) = self.injector.lock() else {
            return false;
        };
        // A panic must not unwind across the COM boundary. The
        // guard is held outside the closure, so a panicking
        // injector does not poison the mutex for the next drop.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch_dropfiles_to_injector(&buf, &guard)
        }))
        .unwrap_or(false)
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for ConsoleDropTarget_Impl {
    fn DragEnter(
        &self,
        data: windows_core::Ref<'_, IDataObject>,
        _key_state: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        let carries_files = data.as_ref().is_some_and(offers_cf_hdrop);
        self.carries_files.store(carries_files, Ordering::SeqCst);
        // SAFETY: `effect` is the in/out pointer OLE passes per the
        // IDropTarget contract.
        unsafe {
            let bits = drag_effect(source_allowed(effect), carries_files);
            write_effect(effect, bits);
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _key_state: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        let carries_files = self.carries_files.load(Ordering::SeqCst);
        // SAFETY: see DragEnter.
        unsafe {
            let bits = drag_effect(source_allowed(effect), carries_files);
            write_effect(effect, bits);
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows_core::Result<()> {
        self.carries_files.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn Drop(
        &self,
        data: windows_core::Ref<'_, IDataObject>,
        _key_state: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        self.carries_files.store(false, Ordering::SeqCst);
        // SAFETY: see DragEnter.
        let allowed = unsafe { source_allowed(effect) };
        // A source that forbids a copy gets nothing injected: the
        // drop is refused, so it must have no side effect either.
        let delivered = allowed & DROPEFFECT_COPY_BITS != 0
            && data.as_ref().is_some_and(|data| self.deliver(data));
        // SAFETY: see DragEnter.
        unsafe { write_effect(effect, drag_effect(allowed, delivered)) };
        Ok(())
    }
}

/// The one format clud reads: `CF_HDROP` in an `HGLOBAL`.
fn cf_hdrop_format() -> windows::Win32::System::Com::FORMATETC {
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Ole::CF_HDROP;

    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: core::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// Whether `data` can render [`cf_hdrop_format`]. Only `S_OK`
/// counts: some data objects answer `S_FALSE` for "not available".
fn offers_cf_hdrop(data: &IDataObject) -> bool {
    let format = cf_hdrop_format();
    // SAFETY: `data` is a live interface; `format` outlives the call.
    (unsafe { data.QueryGetData(&format) }) == windows::Win32::Foundation::S_OK
}

/// Pull the `CF_HDROP` payload out of an `IDataObject` and copy
/// its bytes into a `Vec<u8>`. Returns `None` if the object
/// doesn't carry a CF_HDROP TYMED_HGLOBAL medium. Always cleans
/// up the STGMEDIUM (via `ReleaseStgMedium`) and the `GlobalLock`
/// it took.
///
/// # Safety
///
/// `data` must be a live, AddRef'd `IDataObject`.
pub(super) unsafe fn copy_cf_hdrop_bytes(data: &IDataObject) -> Option<Vec<u8>> {
    use windows::Win32::System::Com::TYMED_HGLOBAL;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::ReleaseStgMedium;

    let format = cf_hdrop_format();

    let mut medium = match unsafe { data.GetData(&format) } {
        Ok(m) => m,
        Err(_) => return None,
    };

    let bytes = if medium.tymed == TYMED_HGLOBAL.0 as u32 {
        // SAFETY: tymed says hGlobal is the active union variant.
        let hglobal = unsafe { medium.u.hGlobal };
        let ptr = unsafe { GlobalLock(hglobal) } as *const u8;
        if ptr.is_null() {
            None
        } else {
            let len = unsafe { GlobalSize(hglobal) };
            let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
            unsafe {
                let _ = GlobalUnlock(hglobal);
            }
            Some(bytes)
        }
    } else {
        None
    };

    // SAFETY: pairs with GetData; ReleaseStgMedium handles the
    // union member and frees the HGLOBAL when pUnkForRelease is
    // null.
    unsafe {
        ReleaseStgMedium(&mut medium);
    }

    bytes
}

/// HWND wrapper that's `Send` so we can ship it into the worker
/// thread. Sound because we never dereference it as a Rust
/// pointer — only pass it back to the Win32 API, which is
/// thread-safe with respect to HWND values.
#[derive(Clone, Copy)]
struct SendHwnd(HWND);
// SAFETY: HWND is a kernel-owned handle; it's safe to Send/Sync
// because the Win32 API handles thread-safety for the underlying
// window.
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

/// COM-based registrar — calls `RegisterDragDrop` /
/// `RevokeDragDrop` on the worker thread that owns the OLE
/// apartment.
struct OleRegistrar {
    hwnds: Vec<SendHwnd>,
    target: IDropTarget,
    ole_initialized: AtomicBool,
}

// SAFETY: `target` is an apartment-threaded COM interface, but
// we only ever invoke `RegisterDragDrop` / `RevokeDragDrop` from
// the same worker thread that performed `OleInitialize` for it.
// The `Send`/`Sync` impls here only assert that the *handle* can
// be moved between threads, not that the COM object itself is
// free-threaded. The worker-thread invariant is preserved by
// construction: we spawn the worker, move the registrar in, and
// drop it on the same worker.
unsafe impl Send for OleRegistrar {}
unsafe impl Sync for OleRegistrar {}

impl DragDropRegistrar for OleRegistrar {
    fn register(&self) -> Result<(), i32> {
        let mut first_error: Option<i32> = None;
        let mut successes = 0usize;

        for hwnd in &self.hwnds {
            // SAFETY: pure FFI; hwnds are probed before construction.
            // Revoke first so clud can replace a terminal/backend target
            // while this foreground session owns the interaction.
            let _ = unsafe { RevokeDragDrop(hwnd.0) };

            // SAFETY: pure FFI; self.target is a ref-counted COM object.
            match unsafe { RegisterDragDrop(hwnd.0, &self.target) } {
                Ok(()) => successes += 1,
                Err(e) => {
                    first_error.get_or_insert(e.code().0);
                }
            }
        }

        if successes > 0 {
            Ok(())
        } else {
            Err(first_error.unwrap_or(0x8000_4005u32 as i32))
        }
    }

    fn revoke(&self) -> Result<(), i32> {
        let mut first_error: Option<i32> = None;
        for hwnd in &self.hwnds {
            // SAFETY: pure FFI; best-effort cleanup for every target.
            if let Err(e) = unsafe { RevokeDragDrop(hwnd.0) } {
                first_error.get_or_insert(e.code().0);
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
}

/// Held inside the guard so the OLE state outlives the worker
/// thread. Drop is intentionally side-effect-only (no panics);
/// the guard arranges for this to drop *after* the worker join.
struct OleKeepAlive {
    registrar: Arc<OleRegistrar>,
}

impl KeepAliveTrait for OleKeepAlive {}

impl Drop for OleKeepAlive {
    fn drop(&mut self) {
        // Normal cleanup happens on the worker STA thread. This is
        // only a fallback for an abnormal worker exit after OLE init.
        if self.registrar.ole_initialized.swap(false, Ordering::SeqCst) {
            // Best-effort cleanup — errors here can't be surfaced.
            let _ = self.registrar.revoke();
            // SAFETY: balanced against the OleInitialize the
            // worker performed at startup.
            unsafe { OleUninitialize() };
        }
    }
}

pub(super) fn register(
    injector: DropInjector,
    config: RefreshConfig,
) -> Result<ConsoleDropTargetGuard, RegisterError> {
    // 1. Resolve every HWND that can receive the Explorer drag.
    let hwnds = drop_target_hwnds();
    if hwnds.is_empty() {
        return Err(RegisterError::ConsoleWindowUnavailable);
    }

    // 2. Construct the IDropTarget COM object up-front. This is
    //    cheap and lets us hand a single `Arc<OleRegistrar>`
    //    into the worker thread.
    let target_iface = new_drop_target(injector);

    let registrar = Arc::new(OleRegistrar {
        hwnds: hwnds.into_iter().map(SendHwnd).collect(),
        target: target_iface,
        ole_initialized: AtomicBool::new(false),
    });

    // 3. Spawn the worker. It performs OleInitialize (becoming
    //    an STA), then runs the delay-then-refresh loop.
    let shutdown = Arc::new(RefreshShutdown::new());
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_registrar: Arc<OleRegistrar> = Arc::clone(&registrar);

    // Channel for surfacing the OleInitialize result back to the
    // caller. `mpsc::channel` keeps us out of additional
    // dependencies.
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), RegisterError>>();

    let worker = std::thread::Builder::new()
        .name("clud-dnd-refresh".to_string())
        .spawn(move || {
            // OleInitialize on this thread (becomes STA). If the
            // thread is already STA-initialized,
            // RPC_E_CHANGED_MODE is treated as a soft success
            // (skip the matching OleUninitialize on cleanup).
            // SAFETY: pure FFI, no preconditions.
            let ole_init = unsafe { OleInitialize(None) };
            let owns_ole = match ole_init {
                Ok(()) => true,
                Err(err) if err.code() == RPC_E_CHANGED_MODE => false,
                Err(err) => {
                    let _ = init_tx.send(Err(RegisterError::OleInitializeFailed(err.code().0)));
                    return;
                }
            };
            worker_registrar
                .ole_initialized
                .store(owns_ole, Ordering::SeqCst);

            // OLE is up — tell the caller the spawn succeeded.
            // From here on, registration failures land inside
            // the loop and are logged rather than surfaced.
            let _ = init_tx.send(Ok(()));

            if let Err(hr) = run_registration_loop(&*worker_registrar, config, &worker_shutdown) {
                crate::loop_console::diagnostic!(
                    "[clud:dnd] RegisterDragDrop failed (HRESULT 0x{:08x})",
                    hr as u32
                );
            }
            // Cleanup belongs on this STA thread: it is the one
            // that called OleInitialize and RegisterDragDrop.
            let _ = worker_registrar.revoke();
            if worker_registrar
                .ole_initialized
                .swap(false, Ordering::SeqCst)
            {
                // SAFETY: balanced against OleInitialize above.
                unsafe { OleUninitialize() };
            }
        })
        .map_err(|_| RegisterError::WorkerSpawnFailed)?;

    // 4. Block until the worker has either signaled OLE-init
    //    success, OLE-init failure, or the channel was dropped
    //    (worker panicked before sending). On failure, join the
    //    worker and surface the error.
    let init_result = init_rx
        .recv()
        .map_err(|_| RegisterError::WorkerSpawnFailed)?;

    if let Err(err) = init_result {
        shutdown.signal();
        let _ = worker.join();
        return Err(err);
    }

    // Note: we deliberately do NOT block until the *initial
    // RegisterDragDrop* completes. The whole point of #79 is the
    // initial-delay phase — blocking here would defeat it. If
    // the initial registration eventually fails, the worker
    // logs and exits but the guard is still considered "live"
    // until dropped.
    let keep_alive: Box<dyn KeepAliveTrait> = Box::new(OleKeepAlive { registrar });

    Ok(ConsoleDropTargetGuard {
        inner: Some(GuardInner {
            shutdown,
            worker: Some(worker),
            keep_alive: Some(keep_alive),
        }),
    })
}

fn drop_target_hwnds() -> Vec<HWND> {
    let mut hwnds = Vec::new();

    let env = |name: &str| std::env::var(name).ok();
    // SAFETY: pure FFI, no preconditions.
    let current_pid = unsafe { GetCurrentProcessId() };
    if let DropHost::WindowsTerminal { pid } = resolve_drop_host(&env, current_pid, process_entries)
    {
        for hwnd in visible_top_level_windows_for_pid(pid) {
            push_unique_hwnd(&mut hwnds, hwnd);
        }
    }

    // SAFETY: pure FFI, no preconditions.
    let console = unsafe { GetConsoleWindow() };
    push_unique_hwnd(&mut hwnds, console);
    hwnds
}

fn push_unique_hwnd(hwnds: &mut Vec<HWND>, hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    if hwnds.iter().any(|existing| existing.0 == hwnd.0) {
        return;
    }
    hwnds.push(hwnd);
}

fn process_entries() -> Vec<ProcessEntry> {
    // SAFETY: pure FFI snapshot.
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };

    let mut entries = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    // SAFETY: `entry` has the required dwSize field initialized.
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            entries.push(ProcessEntry {
                pid: entry.th32ProcessID,
                parent_pid: entry.th32ParentProcessID,
                exe: nul_terminated_wide_to_string(&entry.szExeFile),
            });

            // SAFETY: same initialized PROCESSENTRY32W buffer.
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }

    // SAFETY: closes the snapshot handle returned above.
    let _ = unsafe { CloseHandle(snapshot) };
    entries
}

fn nul_terminated_wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&unit| unit == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

struct EnumWindowsState {
    pid: u32,
    hwnds: Vec<HWND>,
}

fn visible_top_level_windows_for_pid(pid: u32) -> Vec<HWND> {
    let mut state = EnumWindowsState {
        pid,
        hwnds: Vec::new(),
    };
    let state_ptr = &mut state as *mut EnumWindowsState;
    // SAFETY: callback only uses `state_ptr` during this synchronous call.
    let _ = unsafe { EnumWindows(Some(enum_windows_proc), LPARAM(state_ptr as isize)) };
    state.hwnds
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> windows_core::BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut EnumWindowsState) };
    if unsafe { IsWindowVisible(hwnd).as_bool() } {
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == state.pid {
            push_unique_hwnd(&mut state.hwnds, hwnd);
        }
    }
    windows_core::BOOL(1)
}

#[cfg(test)]
mod win_tests {
    use super::*;

    /// The one test that runs the real Toolhelp snapshot the host
    /// decision consumes; the decision itself is covered on every
    /// host in `drop_host_tests.rs`.
    #[test]
    fn real_process_snapshot_contains_this_process() {
        // SAFETY: pure FFI, no preconditions.
        let current_pid = unsafe { GetCurrentProcessId() };
        let entries = process_entries();
        let me = entries
            .iter()
            .find(|entry| entry.pid == current_pid)
            .expect("the Toolhelp snapshot must list the test process itself");
        assert!(
            me.exe.to_ascii_lowercase().ends_with(".exe"),
            "unexpected exe name for the test process: {:?}",
            me.exe
        );

        // Without WT_SESSION or TERM_PROGRAM the snapshot is not needed.
        let host = resolve_drop_host(&|_: &str| None, current_pid, process_entries);
        assert_eq!(host, DropHost::ConsoleWindowOnly);
    }
}

/// Register `clud` as the `IDropTarget`; see
/// [`crate::dnd::console_drop_target::register_console_drop_target`].
pub fn register_console_drop_target(
    injector: DropInjector,
    config: RefreshConfig,
) -> Result<ConsoleDropTargetGuard, RegisterError> {
    register(injector, config)
}

/// Dispatch every pending message on this STA thread so OLE drag-drop
/// callbacks are serviced.
pub fn pump_ole_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    let mut msg = MSG::default();
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// One `responsive_sleep` slice: pump OLE messages, wait up to `step` for
/// more input, then pump again.
pub fn wait_for_messages(step: Duration) {
    pump_ole_messages();
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::MsgWaitForMultipleObjects(
            None,
            false,
            step.as_millis().min(u128::from(u32::MAX)) as u32,
            windows::Win32::UI::WindowsAndMessaging::QS_ALLINPUT,
        );
    }
    pump_ole_messages();
}

/// The real COM layer: the `IDropTarget` vtable driven with a fake
/// `IDataObject`, and the `CF_HDROP` `HGLOBAL` extraction (#1362).
#[cfg(test)]
#[path = "console_drop_target_com_tests.rs"]
mod com_tests;
