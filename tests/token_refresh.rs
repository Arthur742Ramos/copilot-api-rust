//! Ports token-refresh.test.ts: pure-function tests of the refresh-deadline math
//! in `copilot_api::libs::token`. Both functions are deterministic given an
//! explicit `now_ms`, so no globals are involved and no serialization is needed.

use copilot_api::libs::token::{get_refresh_deadline_ms, get_refresh_poll_delay_ms};

#[test]
fn builds_refresh_deadline_from_refresh_in() {
    let now_ms = 1_000_000;
    // refresh_in 1800s -> 1_800_000ms minus the 60_000ms early buffer.
    assert_eq!(get_refresh_deadline_ms(1_800, now_ms), now_ms + 1_740_000);
}

#[test]
fn clamps_refresh_deadline_to_avoid_hot_loop() {
    let now_ms = 1_000_000;
    // refresh_in 30s -> 30_000 - 60_000 = -30_000, clamped up to MIN (1_000ms).
    assert_eq!(get_refresh_deadline_ms(30, now_ms), now_ms + 1_000);
}

#[test]
fn zero_or_missing_refresh_in_uses_default_not_hot_loop() {
    let now_ms = 1_000_000;
    // refresh_in 0 (omitted/malformed upstream) must NOT yield a ~1s deadline.
    // It substitutes the 1500s default: 1_500_000 - 60_000 = 1_440_000ms.
    assert_eq!(get_refresh_deadline_ms(0, now_ms), now_ms + 1_440_000);
    // A negative value is treated the same way.
    assert_eq!(get_refresh_deadline_ms(-5, now_ms), now_ms + 1_440_000);
}

#[test]
fn pathological_refresh_in_is_clamped_not_overflowed() {
    let now_ms = 1_000_000;
    // A huge refresh_in must not overflow `refresh_in * 1000` (which would panic
    // in debug or wrap to a ~1s hot loop in release). It is clamped to 24h:
    // 86_400 * 1000 - 60_000 = 86_340_000ms in the future.
    let deadline = get_refresh_deadline_ms(i64::MAX, now_ms);
    assert_eq!(deadline, now_ms + 86_340_000);
    // Far future, definitely not the ~1s hot-loop floor.
    assert!(deadline > now_ms + 60_000_000);
}

#[test]
fn caps_poll_delay_at_15_seconds() {
    let now_ms = 1_000_000;
    assert_eq!(get_refresh_poll_delay_ms(now_ms + 120_000, now_ms), 15_000);
}

#[test]
fn uses_remaining_delay_when_refresh_is_close() {
    let now_ms = 1_000_000;
    assert_eq!(get_refresh_poll_delay_ms(now_ms + 8_000, now_ms), 8_000);
}

#[test]
fn returns_zero_when_refresh_is_already_due() {
    let now_ms = 1_000_000;
    assert_eq!(get_refresh_poll_delay_ms(now_ms - 1, now_ms), 0);
}
