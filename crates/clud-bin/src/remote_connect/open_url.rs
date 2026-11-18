//! "Open Address": the operator asks the client to open a URL in the user's
//! browser. Only while remote control is granted, only `https://` (or a
//! loopback `http://` origin for local development), and the console always
//! says what was opened or refused, so nothing opens invisibly.
use super::console::Console;
use serde_json::Value;
use url::{Host, Url};

/// The longest address the client accepts.
const MAX_LEN: usize = 2048;

/// Handle one `open_url` event. `granted`: the session is connected and the
/// user has granted remote control. `open` launches the browser and reports
/// whether it started.
pub(super) fn handle(
    event: &Value,
    granted: bool,
    console: &Console,
    open: impl FnOnce(&str) -> bool,
) {
    if !granted {
        console.line("clud.bot refused to open an address (remote control is not granted).");
        return;
    }
    match sanitize(event["url"].as_str().unwrap_or("")) {
        Ok(url) if open(&url) => console.line(format!("clud.bot opened in your browser: {url}")),
        Ok(url) => console.line(format!("clud.bot could not open: {url}")),
        Err(reason) => console.line(format!("clud.bot refused to open an address ({reason}).")),
    }
}

/// Accept only an address the client is willing to open in the user's browser:
/// `https://` with a host, or a loopback `http://` origin. Everything else
/// (`file:`, `javascript:`, `data:`, bare paths, remote `http://`, embedded
/// credentials) is refused. The WHATWG parser decides the host, exactly as the
/// browser will, and the returned serialization is percent-encoded, so it is
/// safe to print.
pub(super) fn sanitize(raw: &str) -> Result<String, &'static str> {
    let raw = raw.trim();
    if raw.len() > MAX_LEN {
        return Err("address too long");
    }
    let url = Url::parse(raw).map_err(|_| "not a valid address")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("addresses with credentials are not allowed");
    }
    match url.scheme() {
        "https" if url.host().is_some() => Ok(url.into()),
        "https" => Err("missing host"),
        "http" if url.host().is_some_and(is_loopback) => Ok(url.into()),
        "http" => Err("only https or loopback http is allowed"),
        _ => Err("only https addresses are allowed"),
    }
}

fn is_loopback(host: Host<&str>) -> bool {
    match host {
        Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

#[cfg(test)]
#[path = "open_url_tests.rs"]
mod tests;
