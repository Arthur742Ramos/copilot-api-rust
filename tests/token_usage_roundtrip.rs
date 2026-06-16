//! Ports token-usage.test.ts + sqlite.test.ts intent: exercise the SQLite-backed
//! token-usage store directly (no router). We point the global usage DB at a
//! fresh temp file via `COPILOT_API_SQLITE_DB_PATH` (read in sqlite.rs) before
//! the connection is first opened.
//!
//! These tests are NON-async (`#[test]`): with no Tokio runtime present,
//! `TokenUsageRecorder::record` writes synchronously, which makes the roundtrip
//! deterministic. They are `#[serial]` because they share the process-global
//! connection and a single time range.

use std::sync::Once;

use copilot_api::libs::token_usage::{
    create_copilot_token_usage_recorder, get_token_usage_events_page, get_token_usage_summary,
    is_token_usage_storage_enabled, UsageTokens,
};

static INIT_DB_PATH: Once = Once::new();

/// Point the usage DB at a unique temp file. Must run before usage_db() opens.
fn init_db_path() {
    INIT_DB_PATH.call_once(|| {
        let dir =
            std::env::temp_dir().join(format!("copilot-api-itest-usage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("usage.sqlite");
        std::env::set_var("COPILOT_API_SQLITE_DB_PATH", &db_path);
    });
}

#[test]
fn storage_is_enabled() {
    // The Rust port bundles rusqlite, so storage is always available.
    assert!(is_token_usage_storage_enabled());
}

#[test]
#[serial_test::serial]
fn recorder_writes_events_and_summary_totals_are_correct() {
    init_db_path();
    let model = "itest-roundtrip-model";

    let recorder = create_copilot_token_usage_recorder("chat_completions", model, None);
    recorder.record(UsageTokens {
        input_tokens: Some(100),
        output_tokens: Some(40),
        ..Default::default()
    });
    recorder.record(UsageTokens {
        input_tokens: Some(10),
        output_tokens: Some(5),
        ..Default::default()
    });

    let summary = get_token_usage_summary("day");
    let model_summary = summary
        .by_model
        .iter()
        .find(|m| m.model == model)
        .expect("our model should appear in the summary");

    // Two events: 145 + 15 input, 45 output total; total_tokens summed per event.
    assert_eq!(model_summary.totals.request_count, 2);
    assert_eq!(model_summary.totals.input_tokens, 110);
    assert_eq!(model_summary.totals.output_tokens, 45);
    assert_eq!(model_summary.totals.total_tokens, 155);
}

#[test]
#[serial_test::serial]
fn zero_token_events_are_skipped() {
    init_db_path();
    let model = "itest-zero-token-model";

    let before = get_token_usage_summary("day")
        .by_model
        .iter()
        .find(|m| m.model == model)
        .map(|m| m.totals.request_count)
        .unwrap_or(0);

    let recorder = create_copilot_token_usage_recorder("chat_completions", model, None);
    // All-zero usage must not be persisted (to_persisted_event returns None).
    recorder.record(UsageTokens::default());
    recorder.record(UsageTokens {
        input_tokens: Some(0),
        output_tokens: Some(0),
        ..Default::default()
    });

    let after = get_token_usage_summary("day")
        .by_model
        .iter()
        .find(|m| m.model == model)
        .map(|m| m.totals.request_count)
        .unwrap_or(0);

    assert_eq!(before, after, "zero-token events should not be recorded");
}

#[test]
#[serial_test::serial]
fn events_page_is_paginated() {
    init_db_path();
    let model = "itest-pagination-model";

    let recorder = create_copilot_token_usage_recorder("messages", model, None);
    for i in 1..=5 {
        recorder.record(UsageTokens {
            input_tokens: Some(i * 10),
            output_tokens: Some(i),
            ..Default::default()
        });
    }

    // Page size 2 -> at least 3 pages for >=5 of our events (other tests may add
    // more, so assert with >=).
    let page1 = get_token_usage_events_page(1, 2, "day");
    assert_eq!(page1.page, 1);
    assert_eq!(page1.page_size, 2);
    assert_eq!(page1.items.len(), 2);
    assert!(page1.total >= 5);
    assert!(page1.total_pages >= 3);

    let our_on_page1 = page1.items.iter().filter(|e| e.model == model).count();
    let page2 = get_token_usage_events_page(2, 2, "day");
    assert_eq!(page2.items.len(), 2);
    // The two pages must not return identical rows.
    let ids1: Vec<i64> = page1.items.iter().map(|e| e.id).collect();
    let ids2: Vec<i64> = page2.items.iter().map(|e| e.id).collect();
    assert!(ids1.iter().all(|id| !ids2.contains(id)), "pages overlap");
    assert!(our_on_page1 <= 2);
}

#[test]
#[serial_test::serial]
fn page_size_is_clamped() {
    init_db_path();
    // page_size clamps to [1, 100]; page clamps to >= 1.
    let page = get_token_usage_events_page(0, 1000, "day");
    assert_eq!(page.page, 1);
    assert_eq!(page.page_size, 100);
}
