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
