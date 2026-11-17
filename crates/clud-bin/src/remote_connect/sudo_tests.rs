use super::*;
use std::sync::{Arc, Mutex};

const PASSWORD: &str = "pw-SENTINEL-4711";

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Shared {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    fn take(&self) -> String {
        let text = self.text();
        self.0.lock().unwrap().clear();
        text
    }
}

impl Write for Shared {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A flow wired to an in-memory leader, a console capture and every event sent.
struct Harness {
    sudo: Sudo,
    console: Console,
    screen: Shared,
    leader_in: Shared,
    replies: mpsc::Sender<Reply>,
    sent: Vec<Value>,
}

impl Harness {
    fn new() -> Self {
        let screen = Shared::default();
        let leader_in = Shared::default();
        let (replies, rx) = mpsc::channel();
        let mut sudo = Sudo::new();
        sudo.user = "alice".into();
        sudo.attach(Link::fake(Box::new(leader_in.clone()), rx));
        Self {
            sudo,
            console: Console::with_sink(screen.clone()),
            screen,
            leader_in,
            replies,
            sent: Vec::new(),
        }
    }

    fn record(&mut self, event: Option<Value>) -> Option<Value> {
        if let Some(event) = &event {
            self.sent.push(event.clone());
        }
        event
    }

    fn request(&mut self, id: &str) -> Option<Value> {
        let event = self.sudo.on_request(Some(id), true, &self.console);
        self.record(event)
    }

    fn reply(&mut self, reply: Reply) -> Option<Value> {
        self.replies.send(reply).unwrap();
        let event = self.sudo.poll(&self.console);
        self.record(event)
    }

    fn type_line(&mut self, line: &str) -> Option<Value> {
        let event = self.sudo.on_line(format!("{line}\n"), &self.console);
        self.record(event)
    }

    fn shown(&self) -> String {
        self.console.settle();
        self.screen.text()
    }

    /// Request, probe says a password is needed, user answers `y`.
    fn to_password_prompt(&mut self, id: &str) {
        assert_eq!(self.request(id).unwrap()["state"], "prompting");
        assert_eq!(self.leader_in.take(), "probe\n");
        assert_eq!(self.reply(Reply::NeedsPassword), None);
        assert!(self.sudo.wants_line());
        assert_eq!(self.type_line("y"), None);
        assert_eq!(self.sudo.phase, Phase::Password(1));
    }
}

fn state(event: Option<Value>) -> String {
    event.expect("an event")["state"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn the_wire_carries_only_the_closed_fields() {
    let allowed = ["type", "request_id", "state", "until"];
    for wire in [
        Wire::Prompting,
        Wire::Active(Some(1_790_000_000)),
        Wire::Active(None),
        Wire::Denied,
        Wire::Unavailable,
        Wire::Dropped,
        Wire::Expired,
    ] {
        for id in [None, Some("r1")] {
            let event = state_event(id, wire);
            for key in event.as_object().unwrap().keys() {
                assert!(allowed.contains(&key.as_str()), "{key} in {event}");
            }
            assert_eq!(event["type"], "sudo_state");
        }
    }
    assert_eq!(
        state_event(Some("r1"), Wire::Active(Some(5))),
        json!({"type":"sudo_state", "request_id":"r1", "state":"active", "until":5})
    );
    assert_eq!(
        state_event(None, Wire::Dropped),
        json!({"type":"sudo_state", "state":"dropped"})
    );
}

#[test]
fn the_question_names_the_operator_the_cap_and_lasting_changes() {
    let [first, second, question] = consent_lines();
    assert_eq!(
        first,
        "Zach Vorhies is asking for administrator (sudo) access for up to 4 hours."
    );
    assert_eq!(second, "Changes made as root stay after sudo ends.");
    assert_eq!(question, "Allow sudo for Zach Vorhies? [y/N]: ");
    assert_eq!(
        password_prompt("alice"),
        "Password for alice (typing is hidden; it is never sent to clud.bot): "
    );
}

#[test]
fn anything_but_yes_declines_without_asking_for_the_password() {
    for answer in ["", "n", "no", "yep", "sure"] {
        let mut h = Harness::new();
        h.request("r1");
        h.leader_in.take();
        h.reply(Reply::NeedsPassword);
        assert_eq!(state(h.type_line(answer)), "denied", "{answer:?}");
        assert!(!h.sudo.wants_line());
        assert!(h.leader_in.text().is_empty(), "nothing reached the leader");
        assert!(!h.shown().contains("Password for"), "{answer:?}");
    }
}

#[test]
fn a_correct_password_reaches_only_the_leader_and_activates_sudo() {
    let mut h = Harness::new();
    h.to_password_prompt("r1");
    assert_eq!(h.type_line(PASSWORD), None);
    assert_eq!(h.leader_in.take(), format!("validate\n{PASSWORD}\n"));
    assert_eq!(h.sudo.phase, Phase::Validating(1));
    let active = h
        .reply(Reply::Active {
            until: 1_790_000_000,
        })
        .unwrap();
    assert_eq!(
        active,
        json!({"type":"sudo_state", "request_id":"r1", "state":"active", "until":1_790_000_000u64})
    );
    // A repeated click re-reports the state without asking again.
    assert_eq!(state(h.request("r2")), "active");
    assert!(h.leader_in.text().is_empty());
    let shown = h.shown();
    assert!(
        shown.contains("Allow sudo for Zach Vorhies? [y/N]: "),
        "{shown}"
    );
    assert!(
        shown.contains("Changes made as root stay after sudo ends."),
        "{shown}"
    );
    assert!(shown.contains("Sudo is on for Zach Vorhies"), "{shown}");
    assert!(!shown.contains(PASSWORD), "password shown locally: {shown}");
    for event in &h.sent {
        assert!(!event.to_string().contains(PASSWORD), "{event}");
    }
}

#[test]
fn three_wrong_passwords_deny_and_each_attempt_asks_again() {
    let mut h = Harness::new();
    h.to_password_prompt("r1");
    for attempt in 1..=3u8 {
        assert_eq!(h.sudo.phase, Phase::Password(attempt));
        h.type_line(&format!("wrong-{attempt}"));
        assert_eq!(h.leader_in.take(), format!("validate\nwrong-{attempt}\n"));
        let event = h.reply(Reply::Wrong);
        if attempt < 3 {
            assert_eq!(event, None);
        } else {
            assert_eq!(state(event), "denied");
        }
    }
    assert!(!h.sudo.wants_line());
    let shown = h.shown();
    assert!(
        shown.contains("(1 of 3)") && shown.contains("(2 of 3)"),
        "{shown}"
    );
    assert!(!shown.contains("(3 of 3)"), "{shown}");
    assert_eq!(shown.matches("Password for alice").count(), 3, "{shown}");
}

#[test]
fn an_empty_password_declines() {
    let mut h = Harness::new();
    h.to_password_prompt("r1");
    assert_eq!(state(h.type_line("")), "denied");
    assert!(h.leader_in.text().is_empty());
}

#[test]
fn a_duplicate_request_while_asking_is_ignored() {
    let mut h = Harness::new();
    h.request("r1");
    assert_eq!(h.request("r2"), None);
    h.reply(Reply::NeedsPassword);
    assert_eq!(h.request("r3"), None);
    assert_eq!(h.leader_in.text(), "probe\n");
}

#[test]
fn a_request_without_control_or_a_shell_is_unavailable() {
    let mut h = Harness::new();
    let event = h.sudo.on_request(Some("r1"), false, &h.console);
    assert_eq!(
        event,
        Some(json!({"type":"sudo_state", "request_id":"r1", "state":"unavailable"}))
    );
    assert!(h.leader_in.text().is_empty());
    let mut lone = Sudo::new();
    assert_eq!(
        state(lone.on_request(Some("r1"), true, &h.console)),
        "unavailable"
    );
}

#[test]
fn no_password_needed_and_no_rights_are_reported_without_a_prompt() {
    let mut h = Harness::new();
    h.request("r1");
    assert_eq!(h.reply(Reply::NoPassword).unwrap()["state"], "active");
    assert!(h.reply(Reply::NoPassword).is_none());
    let mut h = Harness::new();
    h.request("r1");
    assert_eq!(state(h.reply(Reply::Unavailable)), "unavailable");
    assert!(!h.shown().contains("Allow sudo"));
}

#[test]
fn drop_and_expiry_end_an_active_ticket() {
    let mut h = Harness::new();
    h.to_password_prompt("r1");
    h.type_line(PASSWORD);
    h.leader_in.take();
    h.reply(Reply::Active { until: 9 });
    assert_eq!(h.sudo.on_drop(&h.console), None);
    assert_eq!(h.leader_in.take(), "drop\n");
    assert_eq!(state(h.reply(Reply::Dropped)), "dropped");

    let mut h = Harness::new();
    h.to_password_prompt("r1");
    h.type_line(PASSWORD);
    h.reply(Reply::Active { until: 9 });
    assert_eq!(state(h.reply(Reply::Expired)), "expired");
    // Expired means asking again, from the start.
    assert_eq!(state(h.request("r2")), "prompting");
}

#[test]
fn dropping_while_validating_ignores_the_late_activation() {
    let mut h = Harness::new();
    h.to_password_prompt("r1");
    h.type_line(PASSWORD);
    h.sudo.on_drop(&h.console);
    assert_eq!(h.reply(Reply::Active { until: 9 }), None);
    assert_eq!(state(h.reply(Reply::Dropped)), "dropped");
    assert!(!h.shown().contains("Sudo is on"));
}

#[test]
fn shutdown_tells_the_leader_to_drop_and_forgets_the_link() {
    let mut h = Harness::new();
    h.to_password_prompt("r1");
    h.type_line(PASSWORD);
    h.reply(Reply::Active { until: 9 });
    h.leader_in.take();
    // The leader confirms; shutdown must not wait out its deadline.
    h.replies.send(Reply::Dropped).unwrap();
    let started = std::time::Instant::now();
    h.sudo.shutdown(&h.console);
    assert!(started.elapsed() < DROP_WAIT);
    assert_eq!(h.leader_in.text(), "drop\n");
    assert!(h.sudo.link.is_none());
    assert_eq!(
        state(h.sudo.on_request(Some("r2"), true, &h.console)),
        "unavailable"
    );
}

#[test]
fn shutdown_mid_password_releases_the_prompt() {
    let mut h = Harness::new();
    h.to_password_prompt("r1");
    h.replies.send(Reply::Dropped).unwrap();
    h.sudo.shutdown(&h.console);
    assert!(!h.sudo.wants_line());
    assert!(h.sudo.echo.is_none());
    assert!(h.shown().contains("Sudo request cancelled."));
}

#[test]
fn the_shell_ending_ends_sudo() {
    let mut h = Harness::new();
    h.to_password_prompt("r1");
    h.type_line(PASSWORD);
    h.reply(Reply::Active { until: 9 });
    // The leader exited: its reply stream ends.
    let (dead, rx) = mpsc::channel::<Reply>();
    drop(dead);
    h.sudo.link.as_mut().unwrap().replies = rx;
    assert_eq!(state(h.sudo.poll(&h.console)), "dropped");
    assert!(h.sudo.link.is_none());
}
