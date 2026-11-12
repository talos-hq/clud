use super::*;
use serde_json::json;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Screen(Arc<Mutex<Vec<u8>>>);

impl Write for Screen {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Run one `open_url` event; returns what was opened and what the user saw.
fn run(url: &str, granted: bool, opens: bool) -> (Vec<String>, String) {
    let screen = Screen::default();
    let console = Console::with_sink(screen.clone());
    let mut opened = Vec::new();
    handle(
        &json!({"type":"open_url", "url":url}),
        granted,
        &console,
        |url| {
            opened.push(url.to_owned());
            opens
        },
    );
    console.settle();
    let text = String::from_utf8_lossy(&screen.0.lock().unwrap()).into_owned();
    (opened, text)
}

#[test]
fn accepts_https_and_loopback_http() {
    assert_eq!(
        sanitize("https://clud.bot/connect"),
        Ok("https://clud.bot/connect".to_owned())
    );
    assert_eq!(
        sanitize("  https://docs.example.com  "),
        Ok("https://docs.example.com/".to_owned())
    );
    for ok in [
        "http://localhost:8080/x",
        "http://LOCALHOST/",
        "http://127.0.0.1:3000",
        "http://127.8.0.1/",
        "http://[::1]:5000/ui",
    ] {
        assert!(sanitize(ok).is_ok(), "{ok}");
    }
}

#[test]
fn refuses_everything_else() {
    for bad in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "data:text/html,<script>",
        "http://evil.example.com",
        "http://localhost.evil.example.com/",
        "http://127.0.0.1@evil.example.com/",
        // The browser reads `\` as `/`, so the host here is evil.example.com.
        "http://evil.example.com\\@localhost/",
        "https://user:pw@clud.bot/",
        "/etc/passwd",
        "https://",
        "https://user:pw@",
        "",
    ] {
        assert!(sanitize(bad).is_err(), "{bad}");
    }
    let long = format!("https://clud.bot/{}", "a".repeat(MAX_LEN));
    assert_eq!(sanitize(&long), Err("address too long"));
}

#[test]
fn printed_address_carries_no_terminal_control_bytes() {
    let url = sanitize("https://clud.bot/\u{1b}]0;x\u{7}").unwrap();
    assert!(!url.chars().any(char::is_control), "{url:?}");
}

#[test]
fn opens_only_while_control_is_granted() {
    let (opened, text) = run("https://clud.bot/connect", false, true);
    assert!(opened.is_empty());
    assert!(text.contains("remote control is not granted"), "{text}");

    let (opened, text) = run("https://clud.bot/connect", true, true);
    assert_eq!(opened, ["https://clud.bot/connect"]);
    assert!(
        text.contains("opened in your browser: https://clud.bot/connect"),
        "{text}"
    );
}

#[test]
fn a_refused_address_never_reaches_the_browser() {
    let (opened, text) = run("file:///etc/passwd", true, true);
    assert!(opened.is_empty());
    assert!(text.contains("refused to open an address"), "{text}");

    let (opened, text) = run("https://clud.bot/", true, false);
    assert_eq!(opened, ["https://clud.bot/"]);
    assert!(text.contains("could not open: https://clud.bot/"), "{text}");
}
