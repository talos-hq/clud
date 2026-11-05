//! Protected desktop-service recognition is unavailable on this host.

pub fn verified_desktop_service(_pid: u32) -> bool {
    false
}

pub fn task_has_exited(_pid: u32) -> bool {
    false
}
