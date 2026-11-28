//! Ignored research probe for #468.
//!
//! This file deliberately uses raw Win32 process/job APIs instead of
//! `running_process::NativeProcess`: the point of the spike is to measure the
//! primitives that NativeProcess normally wraps or hides.
//!
//! Run manually:
//!
//! ```bash
//! soldr cargo test -p clud --test integration diagnostics::platform::windows::win32_hooking_probe -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Listed in docs/architecture/ci.md "Manual Windows probes"; keep that checklist in sync.

mod win32 {
    use std::collections::HashMap;
    use std::ffi::{c_void, CString};
    use std::io::{BufRead, BufReader, Read};
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdout, Command, Stdio};
    use std::ptr::{null, null_mut};
    use std::time::{Duration, Instant};

    use kernal_api::platform::fs::TemporaryDirectory;
    use windows_sys::Win32::Foundation::{
        CloseHandle, DuplicateHandle, GetLastError, DUPLICATE_SAME_ACCESS, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectAssociateCompletionPortInformation,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows_sys::Win32::System::Memory::{
        VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateRemoteThread, GetCurrentProcess, GetExitCodeProcess, GetExitCodeThread, OpenProcess,
        OpenProcessToken, WaitForSingleObject, INFINITE, LPTHREAD_START_ROUTINE,
        PROCESS_CREATE_THREAD, PROCESS_DUP_HANDLE, PROCESS_QUERY_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
    };
    use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus};

    const JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO: u32 = 4;
    const JOB_OBJECT_MSG_NEW_PROCESS: u32 = 6;
    const JOB_OBJECT_MSG_EXIT_PROCESS: u32 = 7;
    const PROCESS_BASIC_INFORMATION_CLASS: u32 = 0;
    const SYSTEM_EXTENDED_HANDLE_INFORMATION_CLASS: u32 = 64;
    const OBJECT_NAME_INFORMATION_CLASS: u32 = 1;
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryInformationProcess(
            process_handle: HANDLE,
            process_information_class: u32,
            process_information: *mut c_void,
            process_information_length: u32,
            return_length: *mut u32,
        ) -> i32;

        fn NtQuerySystemInformation(
            system_information_class: u32,
            system_information: *mut c_void,
            system_information_length: u32,
            return_length: *mut u32,
        ) -> i32;

        fn NtQueryObject(
            handle: HANDLE,
            object_information_class: u32,
            object_information: *mut c_void,
            object_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *const u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcessBasicInformation {
        reserved1: *mut c_void,
        peb_base_address: *mut c_void,
        reserved2: [*mut c_void; 2],
        unique_process_id: usize,
        reserved3: *mut c_void,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PebPrefix {
        reserved1: [u8; 4],
        reserved2: [usize; 2],
        ldr: usize,
        process_parameters: *const RtlUserProcessParametersPrefix,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RtlUserProcessParametersPrefix {
        reserved1: [u8; 16],
        reserved2: [usize; 10],
        image_path_name: UnicodeString,
        command_line: UnicodeString,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SystemHandleInformationExHeader {
        number_of_handles: usize,
        reserved: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SystemHandleTableEntryInfoEx {
        object: *mut c_void,
        unique_process_id: usize,
        handle_value: usize,
        granted_access: u32,
        creator_back_trace_index: u16,
        object_type_index: u16,
        handle_attributes: u32,
        reserved: u32,
    }

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn new(handle: HANDLE, context: &str) -> Self {
            assert!(
                !handle.is_null() && handle != INVALID_HANDLE_VALUE,
                "{context} failed: {}",
                unsafe { GetLastError() }
            );
            Self(handle)
        }

        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct JobIocp {
        job: OwnedHandle,
        port: OwnedHandle,
    }

    impl JobIocp {
        fn new() -> Self {
            let job = OwnedHandle::new(
                unsafe { CreateJobObjectW(null(), null()) },
                "CreateJobObjectW",
            );
            let port = OwnedHandle::new(
                unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) },
                "CreateIoCompletionPort",
            );

            let assoc = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: job.raw(),
                CompletionPort: port.raw(),
            };
            assert_win32(
                unsafe {
                    SetInformationJobObject(
                        job.raw(),
                        JobObjectAssociateCompletionPortInformation,
                        &assoc as *const _ as *const c_void,
                        size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
                    )
                },
                "SetInformationJobObject(completion port)",
            );

            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            assert_win32(
                unsafe {
                    SetInformationJobObject(
                        job.raw(),
                        JobObjectExtendedLimitInformation,
                        &limits as *const _ as *const c_void,
                        size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    )
                },
                "SetInformationJobObject(kill-on-close)",
            );

            Self { job, port }
        }

        fn assign(&self, child: &Child) {
            let process = child.raw_handle();
            assert_win32(
                unsafe { AssignProcessToJobObject(self.job.raw(), process) },
                "AssignProcessToJobObject",
            );
        }

        fn next_event(&self, timeout: Duration) -> Option<JobEvent> {
            let mut message = 0u32;
            let mut key = 0usize;
            let mut overlapped = null_mut();
            let ok = unsafe {
                GetQueuedCompletionStatus(
                    self.port.raw(),
                    &mut message,
                    &mut key,
                    &mut overlapped,
                    timeout.as_millis().try_into().unwrap_or(u32::MAX),
                )
            };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                if err == WAIT_TIMEOUT {
                    return None;
                }
                panic!("GetQueuedCompletionStatus failed: {err}");
            }
            Some(JobEvent {
                message,
                pid: overlapped as usize as u32,
            })
        }
    }

    struct JobEvent {
        message: u32,
        pid: u32,
    }

    trait ChildExt {
        fn raw_handle(&self) -> HANDLE;
    }

    impl ChildExt for Child {
        fn raw_handle(&self) -> HANDLE {
            use std::os::windows::io::AsRawHandle;
            self.as_raw_handle() as HANDLE
        }
    }

    struct ProbeChild {
        child: Child,
        stdout: BufReader<ChildStdout>,
    }

    impl ProbeChild {
        fn spawn(args: &[&str]) -> Self {
            Self::spawn_with_env(args, &[])
        }

        fn spawn_with_env(args: &[&str], envs: &[(&str, &Path)]) -> Self {
            let mut command = Command::new(probe_target_path());
            command
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (key, value) in envs {
                command.env(key, value);
            }
            let mut child = command.spawn().expect("spawn probe-target");
            let stdout = child.stdout.take().expect("probe-target stdout piped");
            Self {
                child,
                stdout: BufReader::new(stdout),
            }
        }

        fn wait_ready(&mut self) -> String {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read PROBE_READY");
            assert!(read > 0, "probe-target exited before PROBE_READY");
            assert!(
                line.contains("PROBE_READY"),
                "expected PROBE_READY, got {line:?}"
            );
            line
        }

        fn wait_with_output(mut self) -> (std::process::ExitStatus, String, String) {
            let mut stdout = String::new();
            self.stdout
                .read_to_string(&mut stdout)
                .expect("read remaining stdout");
            let mut stderr = String::new();
            if let Some(mut pipe) = self.child.stderr.take() {
                pipe.read_to_string(&mut stderr)
                    .expect("read remaining stderr");
            }
            let status = self.child.wait().expect("wait probe-target");
            (status, stdout, stderr)
        }

        fn kill_and_wait(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    #[ignore = "research-only #468 Win32 probe; run manually on Windows"]
    fn t01_assert_non_elevated() {
        let mut token = null_mut();
        assert_win32(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
            "OpenProcessToken",
        );
        let token = OwnedHandle::new(token, "OpenProcessToken");

        let mut elevation: TOKEN_ELEVATION = unsafe { zeroed() };
        let mut returned = 0u32;
        assert_win32(
            unsafe {
                GetTokenInformation(
                    token.raw(),
                    TokenElevation,
                    &mut elevation as *mut _ as *mut c_void,
                    size_of::<TOKEN_ELEVATION>() as u32,
                    &mut returned,
                )
            },
            "GetTokenInformation(TokenElevation)",
        );
        assert_eq!(elevation.TokenIsElevated, 0, "test must run non-elevated");
    }

    #[test]
    #[ignore = "research-only #468 Win32 probe; run manually on Windows"]
    fn t02_job_iocp_delivers_descendant_lifecycle() {
        let job = JobIocp::new();
        let mut probe = ProbeChild::spawn(&["spawn-chain", "4", "50", "0"]);
        probe.wait_ready();
        job.assign(&probe.child);

        let mut new_pids = Vec::new();
        let mut exit_codes = HashMap::<u32, u32>::new();
        let mut process_handles = HashMap::<u32, OwnedHandle>::new();
        let mut active_zero = false;
        let deadline = Instant::now() + Duration::from_secs(10);

        while Instant::now() < deadline {
            if let Some(event) = job.next_event(Duration::from_millis(250)) {
                match event.message {
                    JOB_OBJECT_MSG_NEW_PROCESS => {
                        new_pids.push(event.pid);
                        let handle = open_process(
                            PROCESS_QUERY_LIMITED_INFORMATION,
                            event.pid,
                            "OpenProcess(new process)",
                        );
                        process_handles.insert(event.pid, handle);
                    }
                    JOB_OBJECT_MSG_EXIT_PROCESS => {
                        let handle = process_handles
                            .get(&event.pid)
                            .expect("EXIT_PROCESS pid was seen in NEW_PROCESS");
                        let mut code = 259u32;
                        assert_win32(
                            unsafe { GetExitCodeProcess(handle.raw(), &mut code) },
                            "GetExitCodeProcess",
                        );
                        exit_codes.insert(event.pid, code);
                    }
                    JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO => {
                        active_zero = true;
                        break;
                    }
                    _ => {}
                }
            }
        }

        let status = probe.child.wait().expect("wait root child");
        assert!(status.success(), "root child status: {status}");
        assert!(
            active_zero,
            "job never emitted ACTIVE_PROCESS_ZERO; new={new_pids:?} exits={exit_codes:?}"
        );
        assert!(
            new_pids.len() >= 4,
            "expected root + descendants NEW_PROCESS events, got {new_pids:?}"
        );
        assert!(
            exit_codes.len() >= 4,
            "expected root + descendants EXIT_PROCESS events, got {exit_codes:?}"
        );
        assert!(
            exit_codes.values().all(|code| *code == 0),
            "all probe exits should be code 0: {exit_codes:?}"
        );
        eprintln!(
            "[t02] events: {} NEW, {} EXIT, ACTIVE_PROCESS_ZERO={active_zero}",
            new_pids.len(),
            exit_codes.len()
        );
    }

    #[test]
    #[ignore = "research-only #468 Win32 probe; run manually on Windows"]
    fn t03_read_child_command_line_and_image_via_peb() {
        let mut probe = ProbeChild::spawn(&["sleep-then-exit", "5000", "0"]);
        probe.wait_ready();

        let handle = open_process(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
            probe.child.id(),
            "OpenProcess(query + vm read)",
        );
        let params = read_process_parameters(handle.raw());
        let command_line = read_remote_unicode(handle.raw(), params.command_line);
        let image_path = read_remote_unicode(handle.raw(), params.image_path_name);

        assert!(
            command_line.contains("sleep-then-exit"),
            "command line did not include test argv: {command_line:?}"
        );
        assert!(
            image_path
                .to_ascii_lowercase()
                .ends_with("probe-target.exe"),
            "unexpected image path: {image_path:?}"
        );
        eprintln!("[t03] cmdline = {command_line:?}");
        eprintln!("[t03] image   = {image_path:?}");
        probe.kill_and_wait();
    }

    #[test]
    #[ignore = "research-only #468 Win32 probe; run manually on Windows"]
    fn t04_enumerate_open_file_handles_in_child() {
        let temp = TemporaryDirectory::new().expect("tempdir");
        let target = temp
            .path()
            .join(format!("clud_probe_{}.bin", std::process::id()));
        let target_text = target.to_string_lossy().into_owned();
        let needle = target
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .to_ascii_lowercase();

        let mut probe = ProbeChild::spawn(&["open-and-hold", &target_text, "5000"]);
        probe.wait_ready();

        let names = enumerate_process_handle_names(probe.child.id());
        let matched = names
            .iter()
            .find(|name| name.to_ascii_lowercase().contains(&needle))
            .cloned();
        probe.kill_and_wait();

        assert!(
            matched.is_some(),
            "did not find {needle:?} in child handle snapshot; names={names:#?}"
        );
        eprintln!("[t04] match: {}", matched.expect("checked Some above"));
    }

    #[test]
    #[ignore = "research-only #468 Win32 probe; run manually on Windows"]
    fn t05_breakaway_prevented_by_default_limits() {
        let job = JobIocp::new();
        let mut probe = ProbeChild::spawn(&["try-breakaway"]);
        probe.wait_ready();
        job.assign(&probe.child);

        let (status, stdout, stderr) = probe.wait_with_output();
        assert!(
            status.success(),
            "try-breakaway failed: status={status} stderr={stderr:?}"
        );
        assert!(
            stdout.contains("BREAKAWAY_DENIED os_error=5"),
            "expected ERROR_ACCESS_DENIED breakaway failure, stdout={stdout:?}"
        );
        eprintln!("[t05] {}", stdout.trim());
    }

    #[test]
    #[ignore = "research-only #468 Win32 probe; run manually on Windows"]
    fn t06_inject_dll_via_createremotethread() {
        let temp = TemporaryDirectory::new().expect("tempdir");
        let sink = temp.path().join("probe-dll-sink.txt");
        let dll = probe_dll_path();
        let mut probe = ProbeChild::spawn_with_env(
            &["sleep-then-exit", "5000", "0"],
            &[("CLUD_PROBE_DLL_SINK", sink.as_path())],
        );
        probe.wait_ready();

        inject_dll(probe.child.id(), &dll);
        assert!(
            wait_until(Duration::from_secs(5), || {
                std::fs::read_to_string(&sink)
                    .map(|body| body.contains("INJECTED pid="))
                    .unwrap_or(false)
            }),
            "probe DLL did not write sink file at {}",
            sink.display()
        );
        let body = std::fs::read_to_string(&sink).expect("read sink");
        eprintln!("[t06] sink contents: {body:?}");
        probe.kill_and_wait();
    }

    fn assert_win32(ok: i32, context: &str) {
        assert!(ok != 0, "{context} failed: {}", unsafe { GetLastError() });
    }

    fn nt_success(status: i32) -> bool {
        status >= 0
    }

    fn open_process(access: u32, pid: u32, context: &str) -> OwnedHandle {
        OwnedHandle::new(unsafe { OpenProcess(access, 0, pid) }, context)
    }

    fn read_process_parameters(process: HANDLE) -> RtlUserProcessParametersPrefix {
        let mut basic: ProcessBasicInformation = unsafe { zeroed() };
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                PROCESS_BASIC_INFORMATION_CLASS,
                &mut basic as *mut _ as *mut c_void,
                size_of::<ProcessBasicInformation>() as u32,
                null_mut(),
            )
        };
        assert!(
            nt_success(status),
            "NtQueryInformationProcess(ProcessBasicInformation) failed: {status:#x}"
        );
        let peb: PebPrefix = read_remote_struct(process, basic.peb_base_address.cast());
        read_remote_struct(process, peb.process_parameters)
    }

    fn read_remote_struct<T: Copy>(process: HANDLE, remote: *const T) -> T {
        assert!(!remote.is_null(), "remote pointer is null");
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        let mut read = 0usize;
        assert_win32(
            unsafe {
                ReadProcessMemory(
                    process,
                    remote.cast(),
                    value.as_mut_ptr().cast(),
                    size_of::<T>(),
                    &mut read,
                )
            },
            "ReadProcessMemory(struct)",
        );
        assert_eq!(read, size_of::<T>(), "short ReadProcessMemory(struct)");
        unsafe { value.assume_init() }
    }

    fn read_remote_unicode(process: HANDLE, value: UnicodeString) -> String {
        if value.length == 0 {
            return String::new();
        }
        assert!(
            !value.buffer.is_null(),
            "remote UNICODE_STRING buffer is null"
        );
        let units = usize::from(value.length) / 2;
        let mut buffer = vec![0u16; units];
        let mut read = 0usize;
        assert_win32(
            unsafe {
                ReadProcessMemory(
                    process,
                    value.buffer.cast(),
                    buffer.as_mut_ptr().cast(),
                    usize::from(value.length),
                    &mut read,
                )
            },
            "ReadProcessMemory(UNICODE_STRING)",
        );
        assert_eq!(
            read,
            usize::from(value.length),
            "short ReadProcessMemory(UNICODE_STRING)"
        );
        String::from_utf16_lossy(&buffer)
    }

    fn enumerate_process_handle_names(pid: u32) -> Vec<String> {
        let process = open_process(
            PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION,
            pid,
            "OpenProcess(PROCESS_DUP_HANDLE)",
        );
        let mut buffer_len = 1024 * 1024;
        let mut buffer = vec![0u8; buffer_len];
        loop {
            let mut returned = 0u32;
            let status = unsafe {
                NtQuerySystemInformation(
                    SYSTEM_EXTENDED_HANDLE_INFORMATION_CLASS,
                    buffer.as_mut_ptr().cast(),
                    buffer_len as u32,
                    &mut returned,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH {
                buffer_len = (returned as usize).max(buffer_len * 2);
                buffer.resize(buffer_len, 0);
                continue;
            }
            assert!(
                nt_success(status),
                "NtQuerySystemInformation(SystemExtendedHandleInformation) failed: {status:#x}"
            );
            break;
        }

        let header = unsafe { &*(buffer.as_ptr() as *const SystemHandleInformationExHeader) };
        let first = unsafe {
            buffer
                .as_ptr()
                .add(size_of::<SystemHandleInformationExHeader>())
                as *const SystemHandleTableEntryInfoEx
        };
        let entries = unsafe { std::slice::from_raw_parts(first, header.number_of_handles) };
        let mut names = Vec::new();
        for entry in entries
            .iter()
            .filter(|entry| entry.unique_process_id == pid as usize)
        {
            let mut duplicate = null_mut();
            let ok = unsafe {
                DuplicateHandle(
                    process.raw(),
                    entry.handle_value as HANDLE,
                    GetCurrentProcess(),
                    &mut duplicate,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if ok == 0 {
                continue;
            }
            let duplicate = OwnedHandle(duplicate);
            if let Some(name) = query_object_name(duplicate.raw()) {
                names.push(name);
            }
        }
        names
    }

    fn query_object_name(handle: HANDLE) -> Option<String> {
        let mut buffer = vec![0u8; 64 * 1024];
        let mut returned = 0u32;
        let status = unsafe {
            NtQueryObject(
                handle,
                OBJECT_NAME_INFORMATION_CLASS,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut returned,
            )
        };
        if !nt_success(status) {
            return None;
        }
        let name = unsafe { &*(buffer.as_ptr() as *const UnicodeString) };
        if name.length == 0 || name.buffer.is_null() {
            return None;
        }
        let units = usize::from(name.length) / 2;
        let slice = unsafe { std::slice::from_raw_parts(name.buffer, units) };
        Some(String::from_utf16_lossy(slice))
    }

    fn inject_dll(pid: u32, dll: &Path) {
        let process = open_process(
            PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_WRITE
                | PROCESS_VM_READ,
            pid,
            "OpenProcess(inject)",
        );
        let wide: Vec<u16> = dll
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let bytes = wide.len() * size_of::<u16>();
        let remote = unsafe {
            VirtualAllocEx(
                process.raw(),
                null(),
                bytes,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        assert!(!remote.is_null(), "VirtualAllocEx failed: {}", unsafe {
            GetLastError()
        });
        let mut written = 0usize;
        assert_win32(
            unsafe {
                WriteProcessMemory(
                    process.raw(),
                    remote,
                    wide.as_ptr().cast(),
                    bytes,
                    &mut written,
                )
            },
            "WriteProcessMemory(dll path)",
        );
        assert_eq!(written, bytes, "short WriteProcessMemory(dll path)");

        let kernel32 = unsafe { GetModuleHandleA(cstr("kernel32.dll").as_ptr().cast()) };
        assert!(!kernel32.is_null(), "GetModuleHandleA(kernel32.dll) failed");
        let load_library =
            unsafe { GetProcAddress(kernel32, cstr("LoadLibraryW").as_ptr().cast()) };
        assert!(
            load_library.is_some(),
            "GetProcAddress(LoadLibraryW) failed"
        );
        let start = unsafe {
            std::mem::transmute::<windows_sys::Win32::Foundation::FARPROC, LPTHREAD_START_ROUTINE>(
                load_library,
            )
        };
        let thread = OwnedHandle::new(
            unsafe { CreateRemoteThread(process.raw(), null(), 0, start, remote, 0, null_mut()) },
            "CreateRemoteThread(LoadLibraryW)",
        );
        let wait = unsafe { WaitForSingleObject(thread.raw(), INFINITE) };
        assert_eq!(
            wait, WAIT_OBJECT_0,
            "LoadLibrary thread wait failed: {wait}"
        );
        let mut exit_code = 0u32;
        assert_win32(
            unsafe { GetExitCodeThread(thread.raw(), &mut exit_code) },
            "GetExitCodeThread",
        );
        assert_ne!(exit_code, 0, "LoadLibraryW returned NULL");
        unsafe {
            VirtualFreeEx(process.raw(), remote, 0, MEM_RELEASE);
        }
    }

    fn cstr(value: &str) -> CString {
        CString::new(value).expect("CString")
    }

    fn probe_target_path() -> PathBuf {
        build_probe_bins();
        find_target_file("probe-target.exe")
    }

    fn probe_dll_path() -> PathBuf {
        build_probe_bins();
        find_target_file("probe_dll.dll")
    }

    fn build_probe_bins() {
        static BUILT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        BUILT.get_or_init(|| {
            let root = workspace_root();
            let mut command = cargo_command();
            command
                .current_dir(&root)
                .arg("build")
                .arg("-p")
                .arg("probe-target")
                .arg("-p")
                .arg("probe-dll");
            let status = command.status().expect("spawn cargo build for probe bins");
            assert!(status.success(), "probe bin build failed: {status}");
        });
    }

    fn cargo_command() -> Command {
        if command_exists("soldr") {
            let mut command = Command::new("soldr");
            command.arg("--no-cache").arg("cargo");
            command
        } else {
            Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        }
    }

    fn command_exists(name: &str) -> bool {
        Command::new(name)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn find_target_file(file_name: &str) -> PathBuf {
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root().join("target"));
        let candidates = [
            target_dir
                .join("x86_64-pc-windows-msvc")
                .join("debug")
                .join(file_name),
            target_dir
                .join("aarch64-pc-windows-msvc")
                .join("debug")
                .join(file_name),
            target_dir.join("debug").join(file_name),
        ];
        candidates
            .into_iter()
            .filter(|path| path.is_file())
            .max_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
            .unwrap_or_else(|| panic!("{file_name} not found under {}", target_dir.display()))
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .expect("workspace root")
    }

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}
