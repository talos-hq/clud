use super::*;

/// #1357: Git Bash mintty gives a native exe pipes; detect it so the
/// subprocess-mode downgrade is announced rather than silent.
#[test]
fn mintty_without_console_detected_on_windows_with_term_and_msystem() {
    assert!(looks_like_mintty_without_console(
        true,
        false,
        false,
        Some("xterm"),
        Some("MINGW64")
    ));
}

#[test]
fn mintty_without_console_rejects_non_mintty_environments() {
    let xterm = Some("xterm");
    let mingw = Some("MINGW64");
    assert!(!looks_like_mintty_without_console(
        false, false, false, xterm, mingw
    ));
    assert!(!looks_like_mintty_without_console(
        true, true, false, xterm, mingw
    ));
    assert!(!looks_like_mintty_without_console(
        true, false, true, xterm, mingw
    ));
    assert!(!looks_like_mintty_without_console(
        true, false, false, None, mingw
    ));
    assert!(!looks_like_mintty_without_console(
        true,
        false,
        false,
        Some("dumb"),
        mingw
    ));
    assert!(!looks_like_mintty_without_console(
        true,
        false,
        false,
        Some(""),
        mingw
    ));
    assert!(!looks_like_mintty_without_console(
        true,
        false,
        false,
        xterm,
        Some("")
    ));
    // A plain `clud -p | cat` pipeline on Windows has no MSYSTEM.
    assert!(!looks_like_mintty_without_console(
        true, false, false, xterm, None
    ));
}

// F3Observer — byte-level observer for voice-mode F3 press detection.
// Observer, not interceptor: bytes are still forwarded verbatim to the
// child. These tests drive Steps 1–4 of the raw-pump refactor.

#[test]
fn observer_passes_arbitrary_bytes_through_without_detecting_f3() {
    // Random bytes, DSR, paste chunks, newlines — none of these should
    // register as F3 presses. The observer doesn't modify bytes; tests
    // here only assert the count of events it reports.
    let mut obs = F3Observer::new();
    assert_eq!(obs.observe(b"\x1b[6n").presses, 0, "DSR query is not F3");
    assert_eq!(obs.observe(b"hello\n").presses, 0);
    assert_eq!(obs.observe(b"\x03").presses, 0, "raw Ctrl+C byte is not F3");
    let smoke: Vec<u8> = (0..=255u8).collect();
    // The smoke vector happens to contain \x1b,O,R bytes somewhere, but
    // they are not adjacent in that order, so no press should fire.
    assert_eq!(obs.observe(&smoke).presses, 0);
}

#[test]
fn observer_detects_single_and_multiple_f3_presses() {
    let mut obs = F3Observer::new();
    assert_eq!(obs.observe(b"\x1bOR").presses, 1);
    let mut obs = F3Observer::new();
    assert_eq!(obs.observe(b"hello\x1bORworld").presses, 1);
    let mut obs = F3Observer::new();
    assert_eq!(obs.observe(b"\x1bOR\x1bOR\x1bOR").presses, 3);
}

#[test]
fn observer_detects_f3_across_fragmented_reads() {
    // 2-way split: \x1b | OR
    let mut obs = F3Observer::new();
    let mut total = 0;
    total += obs.observe(b"\x1b").presses;
    total += obs.observe(b"OR").presses;
    assert_eq!(total, 1, "2-way split should still detect one press");

    // 3-way split: \x1b | O | R
    let mut obs = F3Observer::new();
    let mut total = 0;
    for chunk in [&b"\x1b"[..], &b"O"[..], &b"R"[..]] {
        total += obs.observe(chunk).presses;
    }
    assert_eq!(total, 1, "3-way split should still detect one press");

    // Broken prefix then a clean press later: only the clean one counts.
    let mut obs = F3Observer::new();
    let mut total = 0;
    total += obs.observe(b"\x1b").presses;
    total += obs.observe(b"XYZ").presses; // breaks the prefix, X is not O
    total += obs.observe(b"\x1bOR").presses;
    assert_eq!(total, 1);
}

#[test]
fn observer_ignores_non_f3_escapes() {
    let mut obs = F3Observer::new();
    assert_eq!(obs.observe(b"\x1b[6n").presses, 0, "DSR");
    assert_eq!(obs.observe(b"\x1bOA").presses, 0, "SS3 up arrow");
    assert_eq!(obs.observe(b"\x1bOP").presses, 0, "F1 (SS3 P)");
    assert_eq!(
        obs.observe(b"\x1bOX\x1bOR tail").presses,
        1,
        "valid F3 after a bogus SS3 prefix should still count"
    );
}

// ─── Kitty keyboard-protocol release / repeat events ──────────────────
// Issue #13 hold-to-record uses release events when terminals support
// the kitty protocol. Three F3 encodings can carry release info:
//   * CSI tilde with event-type:    `\x1b[13;1:3~`
//   * CSI u (numeric):               `\x1b[13;1:3u`
//   * CSI u (functional encoding):   `\x1b[57346;1:3u`
// Repeats (event-type 2) are intentionally silent — they signal the
// key is still held and would otherwise spam the voice state machine.

#[test]
fn observer_detects_csi_tilde_f3_press() {
    let mut obs = F3Observer::new();
    let events = obs.observe(b"\x1b[13~");
    assert_eq!(events.presses, 1);
    assert_eq!(events.releases, 0);
}

#[test]
fn observer_detects_kitty_csi_u_press_and_release() {
    // Press then release via CSI u with the keycode-13 form.
    let mut obs = F3Observer::new();
    let events = obs.observe(b"\x1b[13;1:1u\x1b[13;1:3u");
    assert_eq!(events.presses, 1);
    assert_eq!(events.releases, 1);
}

#[test]
fn observer_detects_kitty_functional_encoding_release() {
    // F3 functional-encoding keycode is 57346 in the kitty protocol.
    let mut obs = F3Observer::new();
    let events = obs.observe(b"\x1b[57346;1:3u");
    assert_eq!(events.releases, 1);
    assert_eq!(events.presses, 0);
}

#[test]
fn observer_ignores_kitty_repeat_event() {
    // event-type 2 = autorepeat. Must NOT be counted as a fresh press —
    // doing so would tear down the recording the user is still holding.
    let mut obs = F3Observer::new();
    let events = obs.observe(b"\x1b[13;1:2~");
    assert_eq!(events.presses, 0);
    assert_eq!(events.releases, 0);
}

#[test]
fn observer_handles_release_split_across_reads() {
    // Same fragmentation tolerance as the legacy SS3 path: a release
    // sequence chopped one byte at a time must still register exactly
    // once.
    let mut obs = F3Observer::new();
    let mut presses = 0;
    let mut releases = 0;
    for &b in b"\x1b[13;1:3~" {
        let ev = obs.observe(&[b]);
        presses += ev.presses;
        releases += ev.releases;
    }
    assert_eq!(presses, 0);
    assert_eq!(releases, 1);
}

#[test]
fn observer_ignores_non_f3_csi_sequences() {
    // Other CSI sequences must not be mis-attributed to F3.
    let mut obs = F3Observer::new();
    assert_eq!(obs.observe(b"\x1b[1~").presses, 0, "Home (CSI 1~)");
    assert_eq!(obs.observe(b"\x1b[15~").presses, 0, "F5 (CSI 15~)");
    assert_eq!(obs.observe(b"\x1b[57347u").presses, 0, "F4 functional");
    assert_eq!(obs.observe(b"\x1b[6n").presses, 0, "DSR (still no F3)");
}

#[test]
fn console_stdin_normalization_is_windows_only() {
    let mut chunk = vec![b'a', 0x08, 0x7f, b'z'];
    normalize_interactive_console_stdin_chunk(&mut chunk);
    if crate::platform::host::WINDOWS {
        assert_eq!(chunk, vec![b'a', 0x7f, 0x7f, b'z']);
    } else {
        assert_eq!(chunk, vec![b'a', 0x08, 0x7f, b'z']);
    }
}

// ─── Issue #1350: console-input (extra_rx) parity with stdin ─────────

const PARITY_CLOSE: crate::toast::text_tier::CellRect = crate::toast::text_tier::CellRect {
    row: 0,
    col: 70,
    width: 3,
    height: 1,
};

fn parity_targets() -> Option<ToastHitTargets> {
    Some(ToastHitTargets {
        gear: None,
        close: Some(PARITY_CLOSE),
        cpu: None,
        hover_armed: false,
    })
}

/// Issue #1350: the `extra_rx` path used to write raw bytes to the PTY,
/// skipping the paste normalizer and the toast mouse filter. Both arms now
/// share one pipeline and must produce identical bytes and dismiss flags.
#[test]
fn extra_input_pipeline_matches_stdin_pipeline() {
    let inputs: [&[u8]; 3] = [
        b"\x1b[200~\"C:\\Users\\me\\my file.txt\"\x1b[201~",
        b"a\x1b[<0;71;1Mb\x1b[<0;71;1mc",
        b"hello world\r",
    ];
    let mut stdin_paste = BracketedPasteNormalizer::new();
    let mut stdin_mouse = crate::toast::mouse::MouseFilter::new();
    let mut extra_paste = BracketedPasteNormalizer::new();
    let mut extra_mouse = crate::toast::mouse::MouseFilter::new();
    let mut dismissed = false;
    for input in inputs {
        let via_stdin =
            filter_user_input_chunk(input, &mut stdin_paste, &mut stdin_mouse, parity_targets());
        let prepared = extra_chunk_for_pipeline(input, false);
        let via_extra = filter_user_input_chunk(
            &prepared,
            &mut extra_paste,
            &mut extra_mouse,
            parity_targets(),
        );
        assert_eq!(via_extra, via_stdin, "input {:?}", input);
        dismissed |= via_extra.dismissed;
    }
    assert!(
        dismissed,
        "click on the close button must dismiss via extra_rx"
    );
}

/// Issue #1350: a toast close click arriving via `extra_rx` is swallowed.
#[test]
fn extra_input_close_click_is_swallowed_and_dismisses() {
    let mut paste = BracketedPasteNormalizer::new();
    let mut mouse = crate::toast::mouse::MouseFilter::new();
    let prepared = extra_chunk_for_pipeline(b"a\x1b[<0;71;1Mb\x1b[<0;71;1mc", false);
    let result = filter_user_input_chunk(&prepared, &mut paste, &mut mouse, parity_targets());
    assert!(result.dismissed);
    assert_eq!(result.bytes, b"abc");
}

/// Issue #1350: drag-drop chunks (plain newline-joined paths) pass through
/// the shared pipeline byte-for-byte.
#[test]
fn extra_input_drop_paths_pass_through_unchanged() {
    let mut paste = BracketedPasteNormalizer::new();
    let mut mouse = crate::toast::mouse::MouseFilter::new();
    let drop = b"C:\\a b\\x.txt\nC:\\y.txt ";
    let prepared = extra_chunk_for_pipeline(drop, false);
    let result = filter_user_input_chunk(&prepared, &mut paste, &mut mouse, None);
    assert_eq!(result.bytes, drop);
}

/// Issue #1350: Backspace from `console_input` gets the same Windows
/// 0x08 -> 0x7f normalization the byte-stream reader applies.
#[test]
fn extra_input_backspace_normalization_is_windows_only() {
    let normalize = should_normalize_interactive_console_stdin(true);
    let prepared = extra_chunk_for_pipeline(&[b'a', 0x08, b'z'], normalize);
    let mut paste = BracketedPasteNormalizer::new();
    let mut mouse = crate::toast::mouse::MouseFilter::new();
    let result = filter_user_input_chunk(&prepared, &mut paste, &mut mouse, None);
    if crate::platform::host::WINDOWS {
        assert_eq!(result.bytes, vec![b'a', 0x7f, b'z']);
    } else {
        assert_eq!(result.bytes, vec![b'a', 0x08, b'z']);
    }
}

/// Issue #1350: F3 arriving via `console_input` must reach the voice
/// observer; the shared pipeline observes the prepared extra chunk.
#[test]
fn extra_input_f3_is_observed() {
    let normalize = should_normalize_interactive_console_stdin(true);
    let prepared = extra_chunk_for_pipeline(b"\x1bOR", normalize);
    let mut paste = BracketedPasteNormalizer::new();
    let mut mouse = crate::toast::mouse::MouseFilter::new();
    let result = filter_user_input_chunk(&prepared, &mut paste, &mut mouse, parity_targets());
    assert_eq!(
        result.bytes, b"\x1bOR",
        "F3 is still forwarded to the child"
    );
    let mut observer = F3Observer::new();
    assert_eq!(observer.observe(&prepared).presses, 1);
}

/// Well past every release delay: held bytes are due.
fn later() -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(5)
}

#[test]
fn local_pump_releases_lone_esc_after_short_idle_without_toast() {
    let mut paste = BracketedPasteNormalizer::new();
    let mut mouse = crate::toast::mouse::MouseFilter::new();
    let result = filter_user_input_chunk(b"\x1b", &mut paste, &mut mouse, None);
    assert!(result.bytes.is_empty());
    let now = std::time::Instant::now();
    let wait = pending_user_input_wait(PUMP_TICK, now, &paste, &mouse, false);
    assert!(wait <= bracketed_paste::HUMAN_PREFIX_FLUSH, "{wait:?}");
    assert_eq!(
        flush_pending_user_input(later(), &mut paste, &mut mouse, None),
        b"\x1b"
    );
    assert_eq!(
        pending_user_input_wait(PUMP_TICK, later(), &paste, &mouse, false),
        PUMP_TICK
    );
}

#[test]
fn local_pump_releases_lone_esc_through_toast_filter() {
    let mut paste = BracketedPasteNormalizer::new();
    let mut mouse = crate::toast::mouse::MouseFilter::new();
    let result = filter_user_input_chunk(b"\x1b", &mut paste, &mut mouse, parity_targets());
    assert!(result.bytes.is_empty());
    let wait = pending_user_input_wait(PUMP_TICK, std::time::Instant::now(), &paste, &mouse, true);
    assert!(wait <= INPUT_PENDING_FLUSH, "{wait:?}");
    assert_eq!(
        flush_pending_user_input(later(), &mut paste, &mut mouse, parity_targets()),
        b"\x1b"
    );
    assert!(!paste.has_pending());
    assert!(!mouse.has_pending());
}

/// #1717: a partial terminal report is held well past the 5 ms a lone Esc
/// gets, so input arriving in pieces a few milliseconds apart still reaches
/// the PTY as whole sequences instead of a partial one ConPTY would drop.
#[test]
fn a_partial_report_outlasts_the_lone_esc_release_but_is_not_held_forever() {
    for (held, short) in [
        (&b"\x1b"[..], true),
        (b"\x1b[", true),
        (b"\x1bO", true),
        (b"\x1b[<0;10;5", false),
        (b"\x1b[20", false),
    ] {
        let mut paste = BracketedPasteNormalizer::new();
        let mut mouse = crate::toast::mouse::MouseFilter::new();
        assert!(filter_user_input_chunk(held, &mut paste, &mut mouse, None)
            .bytes
            .is_empty());
        let now = std::time::Instant::now();
        let due = paste.flush_due_in(now).expect("held");
        if short {
            assert!(
                due <= bracketed_paste::HUMAN_PREFIX_FLUSH,
                "{held:?}: {due:?}"
            );
        } else {
            assert!(
                due > bracketed_paste::HUMAN_PREFIX_FLUSH,
                "{held:?}: {due:?}"
            );
            let just_after_short = now + bracketed_paste::HUMAN_PREFIX_FLUSH * 2;
            assert!(
                flush_pending_user_input(just_after_short, &mut paste, &mut mouse, None).is_empty(),
                "{held:?} must not be released after a short gap"
            );
        }
        assert_eq!(
            flush_pending_user_input(later(), &mut paste, &mut mouse, None),
            held,
            "released once due"
        );
    }
}

/// #1717: a report that arrives in pieces with idle gaps between them, each
/// gap long enough to release a lone Esc, still leaves as one write once it
/// has started (`ESC [ <`). Only a lone `ESC` / `ESC [` / `ESC O` is released
/// fast, because that is what an Esc or Alt keypress looks like.
#[test]
fn a_report_arriving_in_pieces_with_idle_gaps_is_written_whole() {
    let mut paste = BracketedPasteNormalizer::new();
    let mut mouse = crate::toast::mouse::MouseFilter::new();
    let mut writes = Vec::new();
    for piece in [&b"\x1b[<"[..], b"0;1", b"0;5", b"m"] {
        writes.push(filter_user_input_chunk(piece, &mut paste, &mut mouse, None).bytes);
        // The pump's idle tick, 20 ms after this piece arrived.
        let tick = std::time::Instant::now() + std::time::Duration::from_millis(20);
        writes.push(flush_pending_user_input(tick, &mut paste, &mut mouse, None));
    }
    writes.retain(|w| !w.is_empty());
    assert_eq!(writes, vec![b"\x1b[<0;10;5m".to_vec()]);
}

/// #1697: SGR any-motion reports as the Windows console reader delivers them
/// under VT input: one chunk per character.
const MOUSE_MOTION: &[u8] = b"\x1b[<35;31;18M\x1b[<35;31;19M\x1b[<35;33;20M";

/// Runs `chunks` through the shared pipeline the way the pump does (one
/// `filter_user_input_chunk` per chunk, then an idle flush) and returns each
/// non-empty PTY write.
fn pump_writes(chunks: &[&[u8]], targets: Option<ToastHitTargets>) -> Vec<Vec<u8>> {
    let mut paste = BracketedPasteNormalizer::new();
    let mut mouse = crate::toast::mouse::MouseFilter::new();
    let mut writes: Vec<Vec<u8>> = chunks
        .iter()
        .map(|chunk| filter_user_input_chunk(chunk, &mut paste, &mut mouse, targets).bytes)
        .collect();
    writes.push(flush_pending_user_input(
        later(),
        &mut paste,
        &mut mouse,
        targets,
    ));
    writes.retain(|write| !write.is_empty());
    writes
}

/// #1697: ConPTY's input parser flushes whatever sequence is still open at
/// the end of each write, so a report written across several writes reaches
/// the child as a lost `ESC [ <` plus literal `35;31;18M` text. Every write
/// the pump makes must therefore hold only whole escape sequences.
#[test]
fn byte_at_a_time_mouse_reports_reach_the_pty_as_whole_writes() {
    for targets in [None, parity_targets()] {
        let chunks: Vec<&[u8]> = MOUSE_MOTION.chunks(1).collect();
        let writes = pump_writes(&chunks, targets);
        assert_eq!(
            writes,
            vec![
                b"\x1b[<35;31;18M".to_vec(),
                b"\x1b[<35;31;19M".to_vec(),
                b"\x1b[<35;33;20M".to_vec(),
            ],
            "toast armed: {}",
            targets.is_some()
        );
    }
}

/// #1697: every split of a mixed input stream still forwards it byte-exact,
/// and no write ends inside an escape sequence before the idle flush.
#[test]
fn every_split_of_mixed_input_is_byte_exact_and_never_ends_mid_sequence() {
    let input: &[u8] = b"a\x1b[<35;31;18Mb\x1b[A\x1b[13;2u\x1b\r\x1bOPc\x1b[<0;5;5m";
    for split in 1..input.len() {
        let (left, right) = input.split_at(split);
        for targets in [None, parity_targets()] {
            let writes = pump_writes(&[left, right], targets);
            assert_eq!(writes.concat(), input, "split at {split}");
            for write in &writes[..writes.len() - 1] {
                assert!(
                    !ends_inside_escape_sequence(write),
                    "split at {split}: write {write:?} ends mid-sequence"
                );
            }
        }
    }
}

fn ends_inside_escape_sequence(write: &[u8]) -> bool {
    let Some(esc) = write.iter().rposition(|&b| b == 0x1b) else {
        return false;
    };
    match &write[esc + 1..] {
        [] | [b'O'] => true,
        [b'[', params @ ..] => params.iter().all(|b| (0x20..=0x3f).contains(b)),
        _ => false,
    }
}

#[test]
fn ctrl_c_byte_requests_interrupt() {
    assert!(!stdin_chunk_requests_interrupt(b"abc"));
    assert!(stdin_chunk_requests_interrupt(b"a\x03c"));
}

#[test]
fn only_real_stdin_gets_interactive_console_policy() {
    assert!(stdin_source_is_real_stdin::<std::io::Stdin>());
    assert!(!stdin_source_is_real_stdin::<std::io::Cursor<Vec<u8>>>());
}

// ─── should_spawn_byte_stream_stdin_reader (issue #188) ─────────────

/// Issue #188 GREEN: Windows + interactive console + extra_rx wired
/// suppresses the byte-stream stdin reader so the `console_input`
/// `ReadConsoleInputW` worker is the sole consumer of the STDIN
/// console queue. Without this gate, the byte-stream reader's
/// `ReadFile` call races with `ReadConsoleInputW` on the same queue
/// and Shift+Enter events surface as `\r` instead of `\n`.
#[test]
fn windows_interactive_with_extra_rx_suppresses_byte_stream_reader() {
    if crate::platform::host::WINDOWS {
        assert!(!should_spawn_byte_stream_stdin_reader(true, true));
    }
}

/// Issue #188: without an `extra_rx`, nothing else is consuming the
/// console queue, so the byte-stream reader must run — otherwise no
/// keystrokes reach the child at all.
#[test]
fn no_extra_rx_keeps_byte_stream_reader() {
    assert!(should_spawn_byte_stream_stdin_reader(true, false));
    assert!(should_spawn_byte_stream_stdin_reader(false, false));
}

/// Issue #188: piped stdin (`echo "..." | clud`) is not a console
/// queue at all — `ReadConsoleInputW` can't function on a pipe handle
/// — so the byte-stream reader must run even when an `extra_rx` is
/// supplied. The `interactive_real_stdin` gate keys on
/// `terminals_are_interactive()`.
#[test]
fn piped_stdin_keeps_byte_stream_reader_even_with_extra_rx() {
    assert!(should_spawn_byte_stream_stdin_reader(false, true));
}

/// Issue #188: POSIX has no conhost / `ReadFile` modifier-stripping
/// race, so the suppression must not apply there — the gate is
/// `platform::host::WINDOWS`-scoped.
#[test]
fn posix_keeps_byte_stream_reader_even_with_extra_rx() {
    if !crate::platform::host::WINDOWS {
        assert!(should_spawn_byte_stream_stdin_reader(true, true));
    }
}

// ─── BracketedPasteNormalizer (issue #63 / #79) ────────────────────

#[test]
fn paste_normalizer_passthrough_bytes_outside_paste() {
    let mut p = BracketedPasteNormalizer::new();
    // Plain typing — no PASTE_START seen — passes through verbatim.
    let out = p.process(b"hello world\n");
    assert_eq!(out, b"hello world\n");
}

/// session_paste_normalizes_path_on_drop — when a bracketed paste
/// arrives whose body looks like a dragged path, the body must be
/// rewritten through `normalize_dropped_path` before forwarding.
#[test]
fn session_paste_normalizes_path_on_drop() {
    let mut p = BracketedPasteNormalizer::new();
    // GNOME-Terminal-style file URI drop. Should canonicalize to
    // the platform-appropriate path form.
    let chunk = b"\x1b[200~file:///home/me/my%20file.txt\x1b[201~";
    let out = p.process(chunk);
    // Both POSIX and Windows must wrap output in bracketed-paste
    // markers and percent-decode the URI.
    assert!(out.starts_with(PASTE_START), "must keep PASTE_START");
    assert!(out.ends_with(PASTE_END), "must keep PASTE_END");
    let inner_start = PASTE_START.len();
    let inner_end = out.len() - PASTE_END.len();
    let inner = std::str::from_utf8(&out[inner_start..inner_end]).expect("utf8");
    assert!(
        inner.contains("my file.txt"),
        "inner must be percent-decoded; got {inner:?}"
    );
    // The original URI scheme is gone (normalized form is a path,
    // not a URI).
    assert!(!inner.contains("file://"), "URI must be stripped");
}

/// session_paste_passthrough_for_non_path_text — a paste of plain
/// text (e.g. a code snippet) must be forwarded VERBATIM with the
/// PASTE_START / PASTE_END markers preserved.
#[test]
fn session_paste_passthrough_for_non_path_text() {
    let mut p = BracketedPasteNormalizer::new();
    let chunk = b"\x1b[200~hello world\x1b[201~";
    let out = p.process(chunk);
    // No path → exact passthrough.
    assert_eq!(out, b"\x1b[200~hello world\x1b[201~");
}

#[test]
fn paste_normalizer_multiline_paste_with_path_first_line_is_passthrough() {
    // A multi-line paste whose first line happens to look like a
    // path must not have the path-rewrite applied. The whole-buffer
    // `looks_like_dropped_path` check handles this — multi-line
    // strings don't match the heuristic.
    let mut p = BracketedPasteNormalizer::new();
    let chunk = b"\x1b[200~/Users/me/x.txt\nlet x = 1;\x1b[201~";
    let out = p.process(chunk);
    assert_eq!(out, chunk);
}

#[test]
fn paste_normalizer_handles_split_chunks() {
    // PASTE_START split across two chunks, body in a third, end in a
    // fourth. The detector must reassemble correctly.
    let mut p = BracketedPasteNormalizer::new();
    let mut all = Vec::new();
    all.extend_from_slice(&p.process(b"abc\x1b[2"));
    all.extend_from_slice(&p.process(b"00~"));
    all.extend_from_slice(&p.process(b"hello"));
    all.extend_from_slice(&p.process(b"\x1b[201~tail"));
    assert_eq!(all, b"abc\x1b[200~hello\x1b[201~tail");
}

#[test]
fn paste_normalizer_broken_start_prefix_is_flushed() {
    // \x1b[2 then a non-matching byte — the partial prefix should
    // be forwarded verbatim, not swallowed.
    let mut p = BracketedPasteNormalizer::new();
    let out = p.process(b"\x1b[2X");
    assert_eq!(out, b"\x1b[2X");
}

#[test]
fn paste_normalizer_flush_releases_a_held_lone_esc() {
    // #1355: a lone Esc keypress is a PASTE_START prefix, so `process`
    // holds it. `flush_pending` must hand it back verbatim.
    let mut p = BracketedPasteNormalizer::new();
    assert!(p.process(b"\x1b").is_empty());
    assert!(p.has_pending());
    assert_eq!(p.flush_pending(), b"\x1b");
    assert!(!p.has_pending());
    assert_eq!(p.process(b"a"), b"a");
}

#[test]
fn paste_normalizer_flush_never_splits_an_open_paste() {
    let mut p = BracketedPasteNormalizer::new();
    assert!(p.process(b"\x1b[200~partial").is_empty());
    assert!(!p.has_pending());
    assert!(p.flush_pending().is_empty());
    assert_eq!(
        p.process(b"\x1b[201~"),
        b"\x1b[200~partial\x1b[201~".to_vec()
    );
}

#[test]
fn paste_normalizer_two_pastes_back_to_back() {
    // Two pastes, neither path-shaped, should pass through cleanly.
    let mut p = BracketedPasteNormalizer::new();
    let out = p.process(b"\x1b[200~foo\x1b[201~bar\x1b[200~baz\x1b[201~");
    assert_eq!(out, b"\x1b[200~foo\x1b[201~bar\x1b[200~baz\x1b[201~");
}

// ─── run_output_writer — dedicated writer-thread coalescing (issue #538) ──
//
// The pump used to do one `write_all` + one `flush` per chunk, inline in
// the same loop turn that forwarded stdin, so a slow terminal `flush()`
// delayed keystroke delivery and a chatty child multiplied syscalls. Now a
// dedicated writer thread (`run_output_writer`) drains everything already
// queued before issuing a single write+flush per wakeup. These tests
// exercise that helper directly — no PTY needed — so the O(1)-flush
// property is verified deterministically rather than depending on OS PTY
// buffering timing.

use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

/// A `Write` sink that counts `write()` and `flush()` calls and records
/// every byte it was handed, so tests can assert both "how many syscalls"
/// and "what bytes, in what order".
#[derive(Clone, Default)]
struct CountingSink {
    write_calls: std::sync::Arc<AtomicUsize>,
    flush_calls: std::sync::Arc<AtomicUsize>,
    received: std::sync::Arc<Mutex<Vec<u8>>>,
}

impl io::Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_calls.fetch_add(1, Ordering::SeqCst);
        self.received.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Acceptance criterion: "a burst of N small output chunks results in
/// O(1) flushes, not N." Sending the whole burst BEFORE the writer
/// thread's first `recv()` call (then closing the channel) guarantees
/// every chunk is already queued when the writer wakes, so this
/// deterministically forces one coalesced batch instead of racing the
/// writer thread's wakeup against the sends.
#[test]
fn output_writer_coalesces_a_burst_into_one_write_and_one_flush() {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let sink = CountingSink::default();

    let n = 200;
    let mut expected = Vec::new();
    for i in 0..n {
        let byte = b'a' + (i % 26) as u8;
        tx.send(vec![byte]).expect("send");
        expected.push(byte);
    }
    // Closing the channel lets `run_output_writer`'s loop terminate once
    // it has drained everything already queued.
    drop(tx);

    let sink_for_writer = sink.clone();
    let handle = std::thread::spawn(move || run_output_writer(rx, sink_for_writer));
    handle.join().expect("writer thread panicked");

    assert_eq!(
        sink.write_calls.load(Ordering::SeqCst),
        1,
        "expected exactly one write() for a fully-queued burst of {n} chunks"
    );
    assert_eq!(
        sink.flush_calls.load(Ordering::SeqCst),
        1,
        "expected exactly one flush() for a fully-queued burst of {n} chunks"
    );
    assert_eq!(
        *sink.received.lock().expect("lock"),
        expected,
        "coalesced bytes must be concatenated in send order, byte-accurate"
    );
}

/// A single chunk still results in exactly one write+flush (no
/// off-by-one from the coalescing loop).
#[test]
fn output_writer_single_chunk_is_one_write_and_one_flush() {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let sink = CountingSink::default();
    tx.send(b"hello".to_vec()).expect("send");
    drop(tx);

    let sink_for_writer = sink.clone();
    let handle = std::thread::spawn(move || run_output_writer(rx, sink_for_writer));
    handle.join().expect("writer thread panicked");

    assert_eq!(sink.write_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sink.flush_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*sink.received.lock().expect("lock"), b"hello");
}

/// Empty chunks must not trigger a write/flush at all (mirrors the old
/// `if !filtered.is_empty()` guard this replaces).
#[test]
fn output_writer_skips_write_for_all_empty_chunks() {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let sink = CountingSink::default();
    tx.send(Vec::new()).expect("send");
    tx.send(Vec::new()).expect("send");
    drop(tx);

    let sink_for_writer = sink.clone();
    let handle = std::thread::spawn(move || run_output_writer(rx, sink_for_writer));
    handle.join().expect("writer thread panicked");

    assert_eq!(sink.write_calls.load(Ordering::SeqCst), 0);
    assert_eq!(sink.flush_calls.load(Ordering::SeqCst), 0);
}

/// A slow sink must not stop the writer from eventually draining and
/// exiting cleanly once the channel closes — i.e. "flush remaining
/// chunks first" on shutdown, exercised without a live PTY.
#[test]
fn output_writer_flushes_remaining_chunks_before_exiting() {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let sink = CountingSink::default();
    tx.send(b"first".to_vec()).expect("send");
    std::thread::sleep(std::time::Duration::from_millis(20));
    tx.send(b"second".to_vec()).expect("send");
    drop(tx);

    let sink_for_writer = sink.clone();
    let handle = std::thread::spawn(move || run_output_writer(rx, sink_for_writer));
    handle.join().expect("writer thread panicked");

    assert_eq!(
        *sink.received.lock().expect("lock"),
        b"firstsecond",
        "both chunks must land, in order, before the writer thread exits"
    );
}

// ─── Ctrl+C detection (issue #1101) ───────────────────────────────────
//
// `clud grind` on a kitty-protocol terminal became uninterruptible: the
// pushed `DISAMBIGUATE_ESCAPE_CODES` flag made the terminal send Ctrl+C as
// `\x1b[99;5u` rather than `0x03`, and raw mode had already cleared `ISIG`
// so the `ctrlc` SIGINT handler could not cover for it. The primary fix is
// not pushing that flag; the CSI u decoder below is the backstop.

/// #1703: a CSI u Ctrl+C split across two reads of one stream is still one
/// interrupt, in every spelling; a release never counts.
#[test]
fn csi_u_ctrl_c_split_at_every_offset_requests_exactly_one_interrupt() {
    for seq in [&b"\x1b[99;5u"[..], b"\x1b[99;5:1u", b"\x1b[99;5:2u"] {
        for split in 1..seq.len() {
            let mut scanner = session_stdin::InterruptScanner::default();
            let (left, right) = seq.split_at(split);
            let hits = [
                scanner.requests_interrupt(left),
                scanner.requests_interrupt(right),
            ];
            assert_eq!(
                hits.iter().filter(|&&hit| hit).count(),
                1,
                "{seq:?} split at {split}: {hits:?}"
            );
            assert!(
                !scanner.requests_interrupt(b"a"),
                "no repeat after the match"
            );
        }
    }
    let release = b"\x1b[99;5:3u";
    for split in 1..release.len() {
        let mut scanner = session_stdin::InterruptScanner::default();
        let (left, right) = release.split_at(split);
        assert!(!scanner.requests_interrupt(left) && !scanner.requests_interrupt(right));
    }
}

/// #1703: the carried prefix is bounded, and an unrelated sequence split
/// across reads is not an interrupt.
#[test]
fn interrupt_scanner_ignores_other_split_sequences_and_bounds_its_carry() {
    let mut scanner = session_stdin::InterruptScanner::default();
    assert!(!scanner.requests_interrupt(b"\x1b[<35;31;1"));
    assert!(!scanner.requests_interrupt(b"8M"));
    let mut long = b"\x1b[".to_vec();
    long.extend(std::iter::repeat_n(b'1', 64));
    assert!(!scanner.requests_interrupt(&long));
    assert!(
        !scanner.requests_interrupt(b";5u"),
        "an overlong prefix is dropped"
    );
    assert!(
        scanner.requests_interrupt(b"x\x03"),
        "legacy 0x03 is unchanged"
    );
}

// ─── Keyboard enhancement stack cleanup (issue #1221) ──────────────────

#[test]
fn interrupted_child_before_keyboard_pop_unwinds_child_then_clud_frame() {
    // Model the lifecycle that regressed after Codex moved behind a PTY:
    // caller has one pre-existing frame; clud pushes one; the child pushes
    // one, then Ctrl+C force-terminates it before its matching pop. The
    // tracker sees only child output, so cleanup emits exactly the child's
    // pop; RawTerminalGuard::drop emits the final clud pop. The caller frame
    // is intentionally not represented in or removed by this tracker.
    let tracker = KeyboardEnhancementTracker::default();
    tracker.observe(b"Codex startup\x1b[");
    tracker.observe(b">2u");

    let child_cleanup = keyboard_enhancement_pop_bytes(tracker.take_unbalanced_pushes());
    let clud_cleanup = keyboard_enhancement_pop_bytes(1);
    assert_eq!(child_cleanup, b"\x1b[<1u");
    assert_eq!(
        [child_cleanup, clud_cleanup].concat(),
        b"\x1b[<1u\x1b[<1u",
        "the two session-owned frames must pop in LIFO order, leaving the caller frame"
    );
}

#[test]
fn keyboard_stack_lifecycle_preserves_caller_frame_after_forced_child_exit() {
    // The pre-#1221 cleanup had just one pop: it removed the dead child's
    // top frame and left clud's REPORT_EVENT_TYPES frame active at the shell.
    let mut pre_fix_stack = vec!["caller", "clud", "child"];
    pre_fix_stack.pop();
    assert_eq!(
        pre_fix_stack,
        ["caller", "clud"],
        "the RED lifecycle leaves clud's frame active without child cleanup"
    );

    // Green lifecycle: the tracker emits one pop for the unbalanced child
    // push, then RawTerminalGuard drops clud's own frame. The caller's
    // pre-existing keyboard protocol state remains untouched.
    let tracker = KeyboardEnhancementTracker::default();
    tracker.observe(b"\x1b[>7u");
    let mut fixed_stack = vec!["caller", "clud", "child"];
    for _ in 0..(tracker.take_unbalanced_pushes() + 1) {
        fixed_stack.pop();
    }
    assert_eq!(fixed_stack, ["caller"]);
}

#[test]
fn keyboard_enhancement_tracker_handles_fragmentation_balanced_pops_and_key_events() {
    let tracker = KeyboardEnhancementTracker::default();
    tracker.observe(b"\x1b[>1");
    tracker.observe(b"u\x1b[>2u\x1b[<1u");
    // CSI-u Ctrl+C is a key event, not a stack operation.
    tracker.observe(b"\x1b[99;5:3u");

    assert_eq!(
        tracker.take_unbalanced_pushes(),
        1,
        "one child frame remains after it popped only one of two pushes"
    );
    assert_eq!(tracker.take_unbalanced_pushes(), 0, "cleanup is idempotent");
}

#[test]
fn keyboard_guard_without_its_own_frame_still_unwinds_child_frames() {
    // A console that rejected clud's push owns no frame to pop, but a child
    // that pushed one over it must still be unwound (#1363).
    let mut guard = KeyboardEnhancementGuard::with_pushed(false);
    guard.child_tracker().observe(b"\x1b[>3u");
    let mut terminal = Vec::new();
    guard.unwind_to(&mut terminal);
    assert_eq!(terminal, b"\x1b[<1u");
}

/// #1701 / #1704: the exit sequence first undoes what the child left on
/// (tracked modes, then its keyboard frames), then turns off every input mode
/// that is always off outside a session and shows the cursor, then pops
/// clud's own frame.
#[test]
fn exit_reset_undoes_child_state_then_resets_input_modes_then_pops_clud_frame() {
    let mut guard = RawTerminalGuard {
        keyboard: KeyboardEnhancementGuard::with_pushed(true),
    };
    guard
        .keyboard
        .child_tracker()
        .observe(b"\x1b[>1u\x1b[?1049h\x1b[?1h\x1b[33m");
    let mut terminal = Vec::new();
    guard.write_exit_reset(&mut terminal);
    // Not a real terminal guard: skip its Drop, which writes to stdout and
    // leaves raw mode.
    std::mem::forget(guard);

    let mut expected = b"\x1b[?1049l\x1b[?1l\x1b[0m\x1b[<1u".to_vec();
    expected.extend_from_slice(CHILD_TERMINAL_MODES_RESET);
    expected.extend_from_slice(b"\x1b[<1u");
    assert_eq!(terminal, expected);
    for mode in [
        "1000", "1001", "1002", "1003", "1005", "1006", "1015", "1016", "1004", "2004",
    ] {
        let reset = format!("\x1b[?{mode}l");
        assert!(
            CHILD_TERMINAL_MODES_RESET
                .windows(reset.len())
                .any(|w| w == reset.as_bytes()),
            "exit reset must turn off ?{mode}"
        );
    }
    assert!(CHILD_TERMINAL_MODES_RESET.ends_with(b"\x1b[?25h"));
}

/// #1704: a child that restored its own modes leaves nothing for the
/// tracked reset, so a mode it never touched is never touched for it.
#[test]
fn a_child_that_cleaned_up_after_itself_needs_no_tracked_reset() {
    let mut guard = KeyboardEnhancementGuard::with_pushed(false);
    guard
        .child_tracker()
        .observe(b"\x1b[?1049h\x1b[?1h\x1b[?1l\x1b[?1049l");
    let mut terminal = Vec::new();
    guard.unwind_to(&mut terminal);
    assert!(terminal.is_empty(), "{terminal:?}");
}

#[test]
fn pushed_flags_exclude_disambiguate_escape_codes() {
    // The whole of issue #1101 is downstream of this one bit. A future
    // edit that re-adds it takes Ctrl+C away from every kitty-protocol
    // terminal again, so assert it directly rather than by behavior.
    assert!(
        !KEYBOARD_ENHANCEMENT_FLAGS.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        "DISAMBIGUATE_ESCAPE_CODES re-encodes Ctrl+C as CSI u — see issue #1101"
    );
    assert!(
        KEYBOARD_ENHANCEMENT_FLAGS.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES),
        "REPORT_EVENT_TYPES carries the F3 release event hold-to-record needs (#13)"
    );
}

#[test]
fn kitty_csi_u_ctrl_c_requests_interrupt() {
    // Bare form (no explicit event type) — what kitty sends on press.
    assert!(stdin_chunk_requests_interrupt(b"\x1b[99;5u"), "press");
    // Explicit press and autorepeat. A held key emits a stream of :2u,
    // which is exactly the flood the reporter saw; any one of them has
    // to be enough to break out.
    assert!(stdin_chunk_requests_interrupt(b"\x1b[99;5:1u"), "press :1");
    assert!(stdin_chunk_requests_interrupt(b"\x1b[99;5:2u"), "repeat :2");
    // Ctrl+Shift+C: terminal reports the shifted `C` (67) with ctrl+shift
    // modifiers (4 + 1 + 1 = 6).
    assert!(stdin_chunk_requests_interrupt(b"\x1b[67;6u"), "ctrl+shift");
    // Embedded in a larger chunk, and repeated.
    assert!(stdin_chunk_requests_interrupt(b"hi\x1b[99;5uthere"));
    assert!(stdin_chunk_requests_interrupt(
        b"\x1b[99;5:2u\x1b[99;5:2u\x1b[99;5:3u"
    ));
}

#[test]
fn kitty_csi_u_release_alone_does_not_request_interrupt() {
    // Event type 3 is a key release. The press that preceded it already
    // fired; counting the release too would interrupt twice.
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[99;5:3u"));
}

#[test]
fn non_ctrl_c_csi_u_sequences_do_not_request_interrupt() {
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[99u"), "bare c");
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[99;1u"), "c, no mods");
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[99;2u"), "shift+c");
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[99;3u"), "alt+c");
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[100;5u"), "ctrl+d");
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[27u"), "escape");
    assert!(
        !stdin_chunk_requests_interrupt(b"\x1b[13;1:3~"),
        "F3 release"
    );
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[6n"), "DSR query");
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[200~pasted\x1b[201~"));
}

#[test]
fn malformed_csi_u_sequences_do_not_request_interrupt_or_panic() {
    // Truncated (final byte lands in the next chunk), non-numeric params,
    // a modifier field of 0 (which would underflow the bitfield decrement),
    // and a lone ESC. None may match, none may panic.
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[99;5"));
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[99;xu"));
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[;;;u"));
    assert!(!stdin_chunk_requests_interrupt(b"\x1b[99;0u"));
    assert!(!stdin_chunk_requests_interrupt(b"\x1b["));
    assert!(!stdin_chunk_requests_interrupt(b"\x1b"));
    assert!(!stdin_chunk_requests_interrupt(&[0x1b, b'[', 0xff, b'u']));
}

/// #1310: ConPTY's startup `ESC[6n` must be answered by clud only when no
/// real console on stdin can answer it, and only on Windows.
#[test]
fn cursor_queries_are_answered_only_without_an_interactive_console() {
    assert!(!should_answer_cursor_queries(true));
    assert_eq!(
        should_answer_cursor_queries(false),
        crate::platform::host::WINDOWS
    );
}
