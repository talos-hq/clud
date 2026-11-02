//! Avoid winit's XKB loader panic when a minimal Linux desktop lacks a library.

use std::ffi::CStr;

/// Why no external helper produced a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalFailure {
    Cancelled,
    Unavailable,
}

/// Linux fallback when the native window cannot load (#2197).
pub fn external_prompt(
    title: &str,
    label: &str,
) -> Result<zeroize::Zeroizing<String>, ExternalFailure> {
    if !super::host::LINUX {
        return Err(ExternalFailure::Unavailable);
    }
    super::secret_helper::prompt(title, label)
}

pub fn configure(options: &mut eframe::NativeOptions) {
    options.renderer = eframe::Renderer::Glow;
}

pub fn graphics_available() -> bool {
    if !super::host::LINUX {
        return true;
    }
    let present = |name| {
        std::env::var(name)
            .ok()
            .is_some_and(|value| !value.is_empty())
    };
    // Match winit's backend choice: Wayland wins when both displays exist.
    available_with(
        present("WAYLAND_DISPLAY") || present("WAYLAND_SOCKET"),
        present("DISPLAY"),
        can_load,
    )
}

fn available_with(wayland: bool, x11: bool, mut load: impl FnMut(&CStr) -> bool) -> bool {
    (wayland || x11) && load(c"libxkbcommon.so.0") && (wayland || load(c"libxkbcommon-x11.so.0"))
}

fn can_load(name: &CStr) -> bool {
    // SAFETY: name is a static, terminated library name and the flags are valid.
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return false;
    }
    // SAFETY: this releases exactly the reference acquired above. No symbols
    // or pointers into the library escape this availability check.
    unsafe { libc::dlclose(handle) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_x11_keyboard_library_refuses_before_entering_the_window_backend() {
        let mut checked = Vec::new();
        assert!(!available_with(false, true, |name| {
            checked.push(name.to_bytes().to_vec());
            name != c"libxkbcommon-x11.so.0"
        }));
        assert_eq!(checked.len(), 2);
    }

    #[test]
    fn wayland_does_not_require_the_x11_library_even_with_xwayland_present() {
        assert!(available_with(true, true, |name| name == c"libxkbcommon.so.0"));
        assert!(!available_with(true, false, |_| false));
    }

    #[test]
    fn headless_requests_do_not_load_desktop_libraries() {
        assert!(!available_with(false, false, |_| {
            panic!("a headless request must not load a graphics library")
        }));
    }
}
