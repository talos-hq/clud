use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;

use super::*;
use crate::toast::statusline::{read_live_toast, state_path, StatusStateWriter, StatusUsage};
use crate::toast::{Severity, Toast, ToastEvent, ToastHub};

const ROWS: u16 = 24;
const COLS: u16 = 80;
const IMAGE: u32 = 77;

fn compositor(tier: ToastTier, fallback: Fallback) -> (Compositor, Arc<ToastHub>) {
    let hub = ToastHub::new();
    let c = Compositor::new(ToastPumpOptions {
        process_gear: false,
        hub: Arc::clone(&hub),
        tier,
        fallback,
        usage: None,
        rows: ROWS,
        cols: COLS,
        image_id: IMAGE,
    });
    (c, hub)
}

fn compositor_with_usage(
    tier: ToastTier,
    fallback: Fallback,
    writer: Arc<StatusStateWriter>,
) -> (Compositor, Arc<ToastHub>) {
    let hub = ToastHub::new();
    let c = Compositor::new(ToastPumpOptions {
        process_gear: false,
        hub: Arc::clone(&hub),
        tier,
        fallback,
        usage: Some(writer),
        rows: ROWS,
        cols: COLS,
        image_id: IMAGE,
    });
    (c, hub)
}

fn show(hub: &ToastHub, text: &str, now: Instant) {
    hub.publish(ToastEvent::Show(Toast::new(
        "cpu",
        text,
        Severity::Warn,
        now,
    )));
}

fn apcs(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    text.split("\x1b_G")
        .skip(1)
        .map(|rest| rest.split("\x1b\\").next().unwrap_or("").to_string())
        .collect()
}

fn has(bytes: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(bytes).contains(needle)
}

fn count(bytes: &[u8], action: &str) -> usize {
    apcs(bytes)
        .iter()
        .filter(|cmd| {
            cmd.split(';')
                .next()
                .unwrap()
                .split(',')
                .any(|kv| kv == action)
        })
        .count()
}

fn uploaded_max_alpha(bytes: &[u8]) -> u8 {
    let mut payload = String::new();
    let mut reading = false;
    for command in apcs(bytes) {
        let Some((control, data)) = command.split_once(';') else {
            continue;
        };
        if control.split(',').any(|item| item == "a=t") {
            reading = true;
        }
        if reading {
            payload.push_str(data);
            if control.split(',').any(|item| item == "m=0") {
                break;
            }
        }
    }
    assert!(!payload.is_empty(), "expected a PNG upload");
    let png = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .unwrap();
    image::load_from_memory(&png)
        .unwrap()
        .to_rgba8()
        .pixels()
        .map(|pixel| pixel[3])
        .max()
        .unwrap()
}

// ── kitty tier ────────────────────────────────────────────────────────────

#[test]
fn kitty_toast_is_sent_placed_with_q2_and_the_cursor_is_restored() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    let echo = c.on_child(b"hello\r\nworld", now);
    assert_eq!(echo, b"hello\r\nworld", "nothing to draw yet");
    show(&hub, "cpu 287 %", now);
    let out = c.on_tick(now);
    assert_eq!(count(&out, "a=t"), 1);
    assert_eq!(count(&out, "a=p"), 1);
    for cmd in apcs(&out) {
        let control = cmd.split(';').next().unwrap();
        assert!(
            control.split(',').any(|kv| kv == "q=2"),
            "missing q=2: {control}"
        );
    }
    let text = String::from_utf8_lossy(&out);
    let place_at = text.find("a=p").unwrap();
    assert!(
        text[..place_at].contains("\x1b[1;"),
        "placement must be preceded by a CUP to row 1"
    );
    assert!(
        text.ends_with("\x1b[2;6H\x1b[?25h"),
        "cursor must return to (2,6) and be re-shown: {text:?}"
    );
}

#[test]
fn child_bytes_are_forwarded_first_and_the_placement_is_re_asserted_without_resending() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "hot", now);
    let first = c.on_tick(now);
    assert_eq!(count(&first, "a=t"), 1);
    for burst in [&b"abc"[..], &b"\r\nnext line"[..]] {
        let out = c.on_child(burst, now);
        assert!(out.starts_with(burst), "child bytes must lead the write");
        assert_eq!(count(&out, "a=t"), 0, "image is uploaded once");
        assert_eq!(
            count(&out, "a=p"),
            1,
            "placement is pinned after every burst"
        );
    }
}

#[test]
fn a_clear_reset_or_alt_switch_resends_the_image() {
    for wipe in [&b"\x1b[2J"[..], &b"\x1bc"[..], &b"\x1b[?1049h"[..]] {
        let now = Instant::now();
        let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
        show(&hub, "hot", now);
        c.on_tick(now);
        let out = c.on_child(wipe, now);
        assert_eq!(count(&out, "a=t"), 1, "wipe {wipe:?} must resend");
        assert_eq!(count(&out, "a=p"), 1);
    }
}

#[test]
fn a_changed_text_replaces_the_image() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "cpu 100 %", now);
    c.on_tick(now);
    show(&hub, "cpu 200 %", now);
    let out = c.on_tick(now);
    assert!(apcs(&out).iter().any(|cmd| cmd.starts_with("a=d,d=I")));
    assert_eq!(count(&out, "a=t"), 1);
}

#[test]
fn expiry_deletes_the_placement_and_repaints_nothing() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    c.on_child(b"some text", now);
    hub.publish(ToastEvent::Show(
        Toast::new("done", "back to normal", Severity::Info, now)
            .expiring_after(Duration::from_secs(1)),
    ));
    c.on_tick(now);
    let out = c.on_tick(now + Duration::from_secs(2));
    assert!(apcs(&out).iter().any(|cmd| cmd.starts_with("a=d,d=i")));
    assert_eq!(
        String::from_utf8_lossy(&out).matches("\x1b[").count(),
        0,
        "the text layer is untouched: no CSI at all"
    );
    assert!(
        c.wants_tick(),
        "a new event must be noticed during a quiet TUI"
    );
    let freed = c.finish();
    assert!(apcs(&freed).iter().any(|cmd| cmd.starts_with("a=d,d=I")));
}

// ── safe injection ────────────────────────────────────────────────────────

#[test]
fn injection_waits_for_the_end_of_a_split_escape_sequence() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "hot", now);
    let out = c.on_child(b"text\x1b[3", now);
    assert_eq!(out, b"text\x1b[3", "nothing may follow a partial CSI");
    assert_eq!(c.on_tick(now + Duration::from_secs(5)), Vec::<u8>::new());
    let out = c.on_child(b"1mX", now);
    assert!(out.starts_with(b"1mX"));
    assert_eq!(count(&out, "a=p"), 1);
}

#[test]
fn a_synchronized_update_holds_the_toast_until_it_ends_or_times_out() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "hot", now);
    assert_eq!(c.on_child(b"\x1b[?2026hframe", now), b"\x1b[?2026hframe");
    assert!(c.on_tick(now + Duration::from_millis(100)).is_empty());
    let late = c.on_tick(now + SYNC_DEFER_LIMIT + Duration::from_millis(10));
    assert_eq!(
        count(&late, "a=p"),
        1,
        "an abandoned block cannot hold a toast forever"
    );

    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "hot", now);
    c.on_child(b"\x1b[?2026hframe", now);
    let out = c.on_child(b" done\x1b[?2026l", now);
    assert_eq!(count(&out, "a=p"), 1);
}

#[test]
fn a_cursor_in_the_last_column_defers_injection() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "hot", now);
    let row = "x".repeat(usize::from(COLS));
    let out = c.on_child(row.as_bytes(), now);
    assert_eq!(
        out,
        row.as_bytes(),
        "a CUP now would cancel the pending wrap"
    );
    let out = c.on_child(b"\r\n", now);
    assert_eq!(count(&out, "a=p"), 1);
}

#[test]
fn origin_mode_is_suspended_and_the_cursor_restore_is_region_relative() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    c.on_child(b"\x1b[5;20r\x1b[?6h\x1b[3;4H", now);
    show(&hub, "hot", now);
    let out = c.on_tick(now);
    let text = String::from_utf8_lossy(&out);
    let off = text.find("\x1b[?6l").expect("DECOM must be switched off");
    assert!(off < text.find("a=p").unwrap());
    assert!(
        text.contains("\x1b[?6h\x1b[3;4H"),
        "DECOM back on, then a region-relative CUP: {text:?}"
    );
}

// ── text-cell tier ────────────────────────────────────────────────────────

fn terminal() -> vt100::Parser {
    vt100::Parser::new(ROWS, COLS, 0)
}

#[test]
fn a_text_toast_is_drawn_on_the_alternate_screen_and_restored_exactly() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::TextCells, Fallback::None);
    let mut term = terminal();
    let setup = b"\x1b[?1049h\x1b[1;1H\x1b[32mgreen status\x1b[0m\x1b[1;50Hright side words\x1b[5;3Hprompt> ";
    term.process(&c.on_child(setup, now));
    hub.publish(ToastEvent::Show(
        Toast::new("cpu", "cpu 300 %", Severity::Alert, now).expiring_after(Duration::from_secs(5)),
    ));
    term.process(&c.on_tick(now));
    let top: String = term.screen().rows(0, COLS).next().unwrap();
    assert!(top.contains("cpu 300 %"), "toast missing: {top:?}");
    assert_eq!(
        term.screen().cursor_position(),
        c.shadow().cursor_position()
    );

    // The child draws under the toast while it is visible.
    term.process(&c.on_child(b"\x1b[1;72HXYZ\x1b[5;11Htyped", now));
    let top: String = term.screen().rows(0, COLS).next().unwrap();
    assert!(
        top.contains("cpu 300 %"),
        "toast must survive the child's write"
    );

    term.process(&c.on_tick(now + Duration::from_secs(6)));
    assert_eq!(term.screen().contents(), c.shadow().contents());
    assert_eq!(
        term.screen().cursor_position(),
        c.shadow().cursor_position()
    );
    for col in 0..COLS {
        let (a, b) = (
            term.screen().cell(0, col).unwrap(),
            c.shadow().cell(0, col).unwrap(),
        );
        assert_eq!(
            (a.contents(), a.fgcolor(), a.bgcolor()),
            (b.contents(), b.fgcolor(), b.bgcolor()),
            "col {col}"
        );
    }
}

#[test]
fn a_scroll_under_a_text_toast_leaves_no_toast_cells_behind() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::TextCells, Fallback::None);
    let mut term = terminal();
    let mut setup = b"\x1b[?1049h".to_vec();
    for row in 1..=ROWS {
        setup.extend_from_slice(format!("\x1b[{row};1Hline {row}").as_bytes());
    }
    term.process(&c.on_child(&setup, now));
    hub.publish(ToastEvent::Show(
        Toast::new("cpu", "hot", Severity::Warn, now).expiring_after(Duration::from_secs(5)),
    ));
    term.process(&c.on_tick(now));
    term.process(&c.on_child(b"\x1b[3T", now));
    term.process(&c.on_child(b"\x1b[2S\x1b[4;1Hmore", now));
    term.process(&c.on_tick(now + Duration::from_secs(6)));
    assert_eq!(term.screen().contents(), c.shadow().contents());
}

#[test]
fn a_burst_that_starts_mid_sequence_repaints_the_whole_screen() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::TextCells, Fallback::None);
    let mut term = terminal();
    term.process(&c.on_child(b"\x1b[?1049h\x1b[1;1Hbase", now));
    hub.publish(ToastEvent::Show(
        Toast::new("cpu", "hot", Severity::Warn, now).expiring_after(Duration::from_secs(5)),
    ));
    term.process(&c.on_tick(now));
    term.process(&c.on_child(b"\x1b[3", now));
    term.process(&c.on_child(b"T\x1b[1;1Hafter", now));
    term.process(&c.on_tick(now + Duration::from_secs(6)));
    assert_eq!(term.screen().contents(), c.shadow().contents());
}

#[test]
fn on_the_main_screen_text_tier_uses_the_title_fallback() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::TextCells, Fallback::Title);
    c.on_child(b"inline prompt", now);
    hub.publish(ToastEvent::Show(
        Toast::new("cpu", "hot", Severity::Warn, now).expiring_after(Duration::from_secs(1)),
    ));
    let out = c.on_tick(now);
    assert!(has(&out, "\x1b[22;0t"), "title must be pushed");
    assert!(has(&out, "\x1b]2;clud \u{b7} hot\x07"));
    assert!(
        !has(&out, "\x1b[1;"),
        "the title fallback never moves the cursor"
    );
    let out = c.on_tick(now + Duration::from_secs(2));
    assert_eq!(out, b"\x1b[23;0t", "expiry pops the saved title");
}

#[test]
fn non_kitty_usage_fallback_keeps_model_and_cache_health_in_the_title() {
    let now = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let writer = Arc::new(StatusStateWriter::new(state_path(dir.path(), 80)));
    writer.publish_usage(StatusUsage {
        provider: "codex".into(),
        model: "gpt-5.6-terra".into(),
        request_count: 1,
        cached_input_tokens: 1,
        uncached_input_tokens: 2,
        output_tokens: 3,
        cache_health: "degraded".into(),
        generation: 0,
    });
    let (mut c, _) = compositor_with_usage(ToastTier::Fallback, Fallback::Title, writer);
    let out = c.on_tick(now);
    assert!(has(&out, "\x1b]2;clud \u{b7} gpt-5.6-terra - degraded\x07"));
}

#[test]
fn usage_title_fallback_is_bounded_and_control_free() {
    let now = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let writer = Arc::new(StatusStateWriter::new(state_path(dir.path(), 81)));
    writer.publish_usage(StatusUsage {
        provider: "codex".into(),
        model: format!("{}\x1b]2;bad", "m".repeat(200)),
        request_count: 1,
        cached_input_tokens: 0,
        uncached_input_tokens: 0,
        output_tokens: 0,
        cache_health: "healthy".into(),
        generation: 0,
    });
    let (mut c, _) = compositor_with_usage(ToastTier::Fallback, Fallback::Title, writer);
    let out = String::from_utf8_lossy(&c.on_tick(now)).into_owned();
    let title = out
        .split("\x1b]2;")
        .nth(1)
        .unwrap()
        .trim_end_matches('\x07');
    assert!(title.len() <= 96 + "clud \u{b7}  - healthy".len());
    assert!(!title.chars().any(char::is_control));
}

#[test]
fn the_status_file_fallback_mirrors_and_clears_the_toast() {
    let now = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let path = state_path(dir.path(), 4242);
    let writer = Arc::new(StatusStateWriter::new(path.clone()));
    let (mut c, hub) = compositor(
        ToastTier::Fallback,
        Fallback::StatusFile(Arc::clone(&writer)),
    );
    show(&hub, "cpu 150 %", now);
    assert!(
        c.on_tick(now).is_empty(),
        "the status file surface writes no terminal bytes"
    );
    let live = read_live_toast(&path, crate::toast::statusline::now_ms()).unwrap();
    assert_eq!(live.text, "cpu 150 %");
    hub.publish(ToastEvent::Close { key: "cpu".into() });
    c.on_tick(now);
    assert!(read_live_toast(&path, crate::toast::statusline::now_ms()).is_none());
}

#[test]
fn claude_status_line_carries_the_cpu_toast_instead_of_a_grid_overlay() {
    let now = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let path = state_path(dir.path(), 4243);
    let writer = Arc::new(StatusStateWriter::new(path.clone()));
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::StatusFile(Arc::clone(&writer)));
    show(&hub, "cpu 150 %", now);
    let out = c.on_tick(now);
    assert!(
        out.is_empty(),
        "no kitty image or cell bytes may cover the header: {out:?}"
    );
    assert!(c.input().cpu_rect().is_none());
    let live = read_live_toast(&path, crate::toast::statusline::now_ms()).unwrap();
    assert_eq!(live.text, "cpu 150 %");
    hub.publish(ToastEvent::Close { key: "cpu".into() });
    assert!(c.on_tick(now).is_empty());
    assert!(read_live_toast(&path, crate::toast::statusline::now_ms()).is_none());
}

#[test]
fn without_an_injected_status_line_the_cpu_reading_gets_the_grid_hud() {
    // #2395: the fallback is `None` when injection failed, so the CPU toast
    // must not be suppressed into an unread status file.
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "cpu 150 %", now);
    let out = c.on_tick(now);
    assert!(!out.is_empty(), "the grid HUD draws the CPU reading");
    assert!(c.input().cpu_rect().is_some());
}

#[test]
fn an_expiry_only_republish_is_mirrored_to_the_status_file() {
    // #2395: `shown_at` survives an unchanged republish, so only
    // `expires_at` differs and the status-line copy must still be refreshed.
    let now = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let path = state_path(dir.path(), 4244);
    let writer = Arc::new(StatusStateWriter::new(path.clone()));
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::StatusFile(Arc::clone(&writer)));
    let publish = |ttl_ms: u64| {
        let toast = Toast::new("cpu", "cpu 150 %", Severity::Warn, now)
            .expiring_after(Duration::from_millis(ttl_ms));
        hub.publish(ToastEvent::Show(toast));
    };
    publish(60_000);
    c.on_tick(now);
    let first = read_live_toast(&path, crate::toast::statusline::now_ms())
        .unwrap()
        .expires_ms
        .unwrap();
    publish(120_000);
    c.on_tick(now);
    let second = read_live_toast(&path, crate::toast::statusline::now_ms())
        .unwrap()
        .expires_ms
        .unwrap();
    assert!(second >= first + 50_000, "{first} -> {second}");
}

// ── dismiss, resize, off ──────────────────────────────────────────────────

#[test]
fn session_gear_arms_at_upper_right_and_opens_processes_on_click() {
    let now = Instant::now();
    let (mut c, _) = compositor(ToastTier::Kitty, Fallback::None);
    c.process_gear = true;
    assert!(c.on_tick(now).is_empty());
    assert!(c.input().gear_rect().is_none());
    let bytes = c.on_child(b"\x1b[?1000h\x1b[?1006h", now);
    assert!(bytes.windows(3).any(|bytes| bytes == b"\x1b_G"));
    let gear = c.input().gear_rect().unwrap();
    assert_eq!((gear.row, gear.col), (0, COLS - 2));
    let mut mouse = crate::toast::mouse::MouseFilter::new();
    let result = mouse.process_with_gear(b"\x1b[<0;80;1M", None, None, false, Some(gear));
    assert!(result.open_processes);
    assert!(result.bytes.is_empty());
    c.on_child(b"\x1b[?1006l", now);
    assert!(c.input().gear_rect().is_none());
}

#[test]
fn click_to_dismiss_is_armed_only_when_the_child_reports_sgr_mouse() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    let input = c.input();
    hub.publish(ToastEvent::Show(Toast::new(
        "other",
        "hot",
        Severity::Warn,
        now,
    )));
    c.on_tick(now);
    assert!(
        input.close_rect().is_none(),
        "clud must not rely on mouse the child did not enable"
    );
    c.on_child(b"\x1b[?1000h\x1b[?1006h", now);
    let rect = input
        .close_rect()
        .expect("armed once the child reports SGR mouse");
    assert_eq!(rect.row, 0);
    assert!(rect.col + rect.width < COLS);
    hub.dismiss_visible(now);
    let out = c.on_tick(now);
    assert!(apcs(&out).iter().any(|cmd| cmd.starts_with("a=d,d=i")));
    assert!(input.close_rect().is_none());
}

#[test]
fn a_resize_re_pins_the_toast_to_the_new_top_right() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "hot", now);
    let before = String::from_utf8_lossy(&c.on_tick(now)).into_owned();
    let after = String::from_utf8_lossy(&c.on_resize(30, 120, now)).into_owned();
    let col = |s: &str| {
        let i = s.find("a=p").unwrap();
        let cup = s[..i].rfind("\x1b[1;").unwrap();
        s[cup + 4..i]
            .split('H')
            .next()
            .unwrap()
            .parse::<u16>()
            .unwrap()
    };
    assert!(col(&after) > col(&before));
}

#[test]
fn token_usage_alone_never_creates_a_kitty_overlay() {
    let now = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let writer = Arc::new(StatusStateWriter::new(state_path(dir.path(), 77)));
    writer.publish_usage(StatusUsage {
        provider: "codex".into(),
        model: "gpt-5.6-terra".into(),
        request_count: 3,
        cached_input_tokens: 331_200_000,
        uncached_input_tokens: 1_410_000_000,
        output_tokens: 2_630_000,
        cache_health: "healthy".into(),
        generation: 0,
    });
    let (mut c, _) = compositor_with_usage(ToastTier::Kitty, Fallback::None, writer);
    assert!(c.on_tick(now).is_empty());
}

#[test]
fn title_fallback_keeps_model_and_cache_health_when_cpu_hud_cannot_fit() {
    let now = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let writer = Arc::new(StatusStateWriter::new(state_path(dir.path(), 80)));
    writer.publish_usage(StatusUsage {
        provider: "codex".into(),
        model: "gpt-6-sol".into(),
        request_count: 1,
        cached_input_tokens: 10,
        uncached_input_tokens: 20,
        output_tokens: 3,
        cache_health: "healthy".into(),
        generation: 0,
    });
    let (mut c, hub) = compositor_with_usage(ToastTier::Kitty, Fallback::Title, writer);
    let idle = c.on_resize(ROWS, 15, now);
    assert!(has(&idle, "gpt-6-sol"));
    assert!(has(&idle, "healthy"));
    assert_eq!(count(&idle, "a=t"), 0);

    show(&hub, "cpu 300 %", now + Duration::from_secs(1));
    let active = c.on_tick(now + Duration::from_secs(1));
    assert!(has(&active, "cpu 300 %"));
    assert_eq!(count(&active, "a=t"), 0);

    hub.publish(ToastEvent::Close {
        key: CPU_TOAST_KEY.into(),
    });
    let recovered = c.on_tick(now + Duration::from_secs(2));
    assert!(has(&recovered, "gpt-6-sol"));
    assert!(has(&recovered, "healthy"));
}

#[test]
fn cpu_hud_is_keyed_timed_and_independent_of_other_toasts() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    assert!(c.on_tick(now).is_empty());
    show(&hub, "cpu 300 %", now);
    let first = c.on_tick(now);
    assert_eq!(count(&first, "a=t"), 1);
    assert!(has(&first, "p=2"));
    assert!(!has(&first, "p=1"), "CPU must not be drawn twice");
    assert_eq!(uploaded_max_alpha(&first), 230);
    assert_eq!(
        count(&c.on_tick(now + Duration::from_millis(1999)), "a=t"),
        0
    );

    let faded = c.on_tick(now + Duration::from_secs(2));
    assert_eq!(uploaded_max_alpha(&faded), 128);
    show(&hub, "cpu 350 %", now + Duration::from_secs(3));
    assert_eq!(
        uploaded_max_alpha(&c.on_tick(now + Duration::from_secs(3))),
        128
    );

    hub.publish(ToastEvent::Show(Toast::new(
        "other",
        "another alert",
        Severity::Alert,
        now + Duration::from_secs(4),
    )));
    let combined = c.on_tick(now + Duration::from_secs(4));
    let text = String::from_utf8_lossy(&combined);
    let toast_place = text.find("p=1").unwrap();
    assert!(
        text[..toast_place].contains("\x1b[3;"),
        "toast overlaps CPU: {text:?}"
    );
    assert!(has(&combined, "p=2"));

    hub.dismiss_visible_except(now + Duration::from_secs(4), CPU_TOAST_KEY);
    assert!(hub.snapshot(now + Duration::from_secs(4)).visible.is_some());
    hub.publish(ToastEvent::Close {
        key: CPU_TOAST_KEY.into(),
    });
    let closed = c.on_tick(now + Duration::from_secs(5));
    assert!(has(&closed, "d=i,i=4194381,p=2"));
    assert!(!c.input().cpu_hovered());

    show(&hub, "cpu 200 %", now + Duration::from_secs(6));
    assert_eq!(
        uploaded_max_alpha(&c.on_tick(now + Duration::from_secs(6))),
        230
    );
}

#[test]
fn cpu_hud_coexists_with_an_older_alert_and_releases_its_rows_on_close() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    hub.publish(ToastEvent::Show(Toast::new(
        "other",
        "urgent",
        Severity::Alert,
        now,
    )));
    let ordinary = c.on_tick(now);
    assert!(has(&ordinary, "p=1"));
    assert!(!has(&ordinary, "p=2"));

    show(&hub, "cpu 300 %", now + Duration::from_secs(1));
    let both = c.on_tick(now + Duration::from_secs(1));
    let text = String::from_utf8_lossy(&both);
    let normal_place = text.find("p=1").unwrap();
    assert!(text[..normal_place].contains("\x1b[3;"));
    assert!(has(&both, "p=2"));

    hub.publish(ToastEvent::Close {
        key: CPU_TOAST_KEY.into(),
    });
    let released = c.on_tick(now + Duration::from_secs(2));
    let text = String::from_utf8_lossy(&released);
    let normal_place = text.find("p=1").unwrap();
    assert!(text[..normal_place].contains("\x1b[1;"));
    assert!(has(&released, "d=i,i=4194381,p=2"));
    assert!(has(&released, "d=I,i=4194381"));
}

#[test]
fn shrinking_below_four_rows_deletes_the_ordinary_toast_but_keeps_cpu() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    hub.publish(ToastEvent::Show(Toast::new(
        "other",
        "urgent",
        Severity::Alert,
        now,
    )));
    show(&hub, "cpu 300 %", now);
    let both = c.on_tick(now);
    assert!(has(&both, "p=1"));
    assert!(has(&both, "p=2"));

    let shrunk = c.on_resize(3, COLS, now + Duration::from_secs(1));
    assert!(has(&shrunk, "d=i,i=77,p=1"));
    assert!(has(&shrunk, "p=2"));
    assert!(!has(&c.finish(), "p=1"));
}

#[test]
fn a_short_cpu_episode_hides_before_two_seconds_and_restarts_on_next_episode() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "cpu 300 %", now);
    assert_eq!(uploaded_max_alpha(&c.on_tick(now)), 230);
    hub.publish(ToastEvent::Close {
        key: CPU_TOAST_KEY.into(),
    });
    assert!(has(
        &c.on_tick(now + Duration::from_secs(1)),
        "d=i,i=4194381,p=2"
    ));
    show(&hub, "cpu 300 %", now + Duration::from_secs(3));
    assert_eq!(
        uploaded_max_alpha(&c.on_tick(now + Duration::from_secs(3))),
        230
    );
    assert_eq!(
        uploaded_max_alpha(&c.on_tick(now + Duration::from_secs(5))),
        128
    );
}

#[test]
fn cpu_hud_re_pins_and_reuploads_after_child_image_drops() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "cpu 300 %", now);
    let first = c.on_tick(now);
    assert!(has(&first, "p=2"));
    for command in apcs(&first) {
        assert!(command
            .split(';')
            .next()
            .unwrap()
            .split(',')
            .any(|item| item == "q=2"));
    }
    let re_pinned = c.on_child(b"child redraw\r\n", now);
    assert_eq!(count(&re_pinned, "a=t"), 0);
    assert!(has(&re_pinned, "p=2"));
    for sequence in [b"\x1b[2J".as_slice(), b"\x1bc", b"\x1b[?1049h"] {
        let redrawn = c.on_child(sequence, now);
        assert_eq!(count(&redrawn, "a=t"), 1);
        assert!(has(&redrawn, "p=2"));
    }
    let finish = c.finish();
    assert!(has(&finish, "d=i,i=4194381,p=2"));
    assert!(has(&finish, "d=I,i=4194381"));
}

#[test]
fn cpu_hover_overrides_opacity_without_expanding_or_extending_lifetime() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "cpu 300 %", now);
    c.on_tick(now);
    let input = c.input();
    assert!(!input.cpu_hover_armed());
    input.set_cpu_hover(true);
    assert_eq!(c.cpu_opacity(now + Duration::from_secs(3)), 50);
    c.on_child(b"\x1b[?1000h\x1b[?1006h", now);
    assert!(
        !input.cpu_hover_armed(),
        "click-only mouse mode is not hover"
    );
    c.on_child(b"\x1b[?1003h", now);
    assert!(input.cpu_hover_armed());
    input.set_cpu_hover(true);
    assert_eq!(c.cpu_opacity(now + Duration::from_secs(1)), 90);
    input.set_cpu_hover(false);
    assert_eq!(c.cpu_opacity(now + Duration::from_secs(1)), 90);
    assert_eq!(
        uploaded_max_alpha(&c.on_tick(now + Duration::from_secs(2))),
        128
    );
    input.set_cpu_hover(true);
    let hovered = c.on_tick(now + Duration::from_secs(3));
    assert_eq!(uploaded_max_alpha(&hovered), 230);
    assert!(has(&hovered, "r=2"), "hover must not expand the panel");
    input.set_cpu_hover(false);
    assert_eq!(
        uploaded_max_alpha(&c.on_tick(now + Duration::from_secs(3))),
        128
    );

    hub.publish(ToastEvent::Show(
        Toast::new(
            "cpu",
            "cpu back to normal",
            Severity::Info,
            now + Duration::from_secs(4),
        )
        .expiring_after(Duration::from_secs(10)),
    ));
    assert_eq!(
        uploaded_max_alpha(&c.on_tick(now + Duration::from_secs(4))),
        128
    );
    input.set_cpu_hover(true);
    assert_eq!(
        uploaded_max_alpha(&c.on_tick(now + Duration::from_secs(4))),
        230
    );
    let expired = c.on_tick(now + Duration::from_secs(14));
    assert!(has(&expired, "d=i,i=4194381,p=2"));
    assert!(!input.cpu_hovered(), "expiry must clear stale hover");
}

#[test]
fn narrow_resize_removes_cpu_hud_and_wide_resize_starts_fresh_clock() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Kitty, Fallback::None);
    show(&hub, "cpu 300 %", now);
    c.on_tick(now);
    let narrow = c.on_resize(ROWS, 19, now + Duration::from_secs(3));
    assert!(has(&narrow, "d=i,i=4194381,p=2"));
    let wide = c.on_resize(ROWS, COLS, now + Duration::from_secs(4));
    assert!(has(&wide, "p=2"));
    assert_eq!(uploaded_max_alpha(&wide), 230);
}

#[test]
fn tier_off_is_a_verbatim_pass_through() {
    let now = Instant::now();
    let (mut c, hub) = compositor(ToastTier::Off, Fallback::Title);
    show(&hub, "hot", now);
    assert_eq!(c.on_child(b"\x1b[2Jdata", now), b"\x1b[2Jdata");
    assert!(c.on_tick(now).is_empty());
    assert!(!c.wants_tick());
}
