use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::*;

#[test]
fn parses_seconds_minutes_and_hours() {
    assert_eq!(parse_interval("30s").unwrap(), 30);
    assert_eq!(parse_interval("5m").unwrap(), 300);
    assert_eq!(parse_interval("2h").unwrap(), 7200);
    assert_eq!(parse_interval(" 90s ").unwrap(), 90);
}

#[test]
fn rejects_bad_and_unbounded_intervals() {
    assert!(parse_interval("").is_err());
    assert!(parse_interval("5").is_err(), "a unit is required");
    assert!(parse_interval("0s").is_err());
    assert!(parse_interval("25h").is_err(), "bound is 24h");
    assert!(parse_interval("99d").is_err(), "day unit is not supported");
    assert!(parse_interval("abc").is_err());
}

#[test]
fn formats_seconds_like_the_loop_prints_them() {
    assert_eq!(format_secs(30), "30s");
    assert_eq!(format_secs(90), "90s");
    assert_eq!(format_secs(300), "5m");
    assert_eq!(format_secs(7200), "2h");
}

#[test]
fn wait_returns_true_after_a_full_sleep() {
    let interrupted = AtomicBool::new(false);
    assert!(wait_with_tick(
        Duration::from_millis(50),
        Duration::from_millis(10),
        &interrupted
    ));
}

#[test]
fn wait_aborts_promptly_when_interrupted() {
    let interrupted = AtomicBool::new(false);
    let started = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            interrupted.store(true, Ordering::SeqCst);
        });
        assert!(!wait_with_tick(
            Duration::from_secs(30),
            Duration::from_millis(10),
            &interrupted
        ));
    });
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "interrupt must not wait out the interval: {:?}",
        started.elapsed()
    );
}

#[test]
fn contract_says_ending_the_turn_is_the_wait() {
    let text = interval_contract(300);
    assert!(text.contains("clud waits 5m between iterations"), "{text}");
    assert!(text.contains("ending the turn IS the wait"), "{text}");
    assert!(text.contains("Do not spin"), "{text}");
}
