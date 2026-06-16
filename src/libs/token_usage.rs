use chrono::{Duration, Local, NaiveDate, SecondsFormat, TimeZone, Utc};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::Value;
use tracing::warn;

use crate::libs::request_context::{generate_trace_id, request_context_store};
use crate::libs::sqlite::usage_db;
use crate::libs::state;

/// Mirrors `UsageTokens` in src/lib/token-usage/store.ts.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTokens {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<i64>,
}

pub type TokenUsageEndpoint = &'static str; // chat_completions | embeddings | messages | provider_messages | responses
pub type TokenUsageSource = &'static str; // copilot | provider

fn normalize_token(value: Option<f64>) -> i64 {
    match value {
        Some(v) if v.is_finite() => v.floor().max(0.0) as i64,
        _ => 0,
    }
}

fn normalize_optional_token(value: Option<f64>) -> Option<i64> {
    value.map(|v| {
        if v.is_finite() {
            v.floor().max(0.0) as i64
        } else {
            0
        }
    })
}

fn nested_f64(usage: Option<&Value>, parent: &str, key: &str) -> Option<f64> {
    usage?.get(parent)?.get(key)?.as_f64()
}

fn top_f64(usage: Option<&Value>, key: &str) -> Option<f64> {
    usage?.get(key)?.as_f64()
}

pub fn has_any_token(tokens: &UsageTokens) -> bool {
    normalize_token(tokens.input_tokens.map(|v| v as f64)) > 0
        || normalize_token(tokens.output_tokens.map(|v| v as f64)) > 0
        || normalize_token(tokens.cache_read_input_tokens.map(|v| v as f64)) > 0
        || normalize_token(tokens.cache_creation_input_tokens.map(|v| v as f64)) > 0
        || normalize_token(tokens.total_tokens.map(|v| v as f64)) > 0
}

/// Mirrors `normalizeOpenAIUsage`.
pub fn normalize_openai_usage(usage: Option<&Value>) -> UsageTokens {
    let cached = normalize_token(nested_f64(usage, "prompt_tokens_details", "cached_tokens"));
    let cache_creation = normalize_token(nested_f64(
        usage,
        "prompt_tokens_details",
        "cache_creation_input_tokens",
    ));
    let prompt = normalize_token(top_f64(usage, "prompt_tokens"));
    UsageTokens {
        cache_creation_input_tokens: Some(cache_creation),
        cache_read_input_tokens: Some(cached),
        input_tokens: Some((prompt - cached - cache_creation).max(0)),
        output_tokens: Some(normalize_token(top_f64(usage, "completion_tokens"))),
        total_tokens: normalize_optional_token(top_f64(usage, "total_tokens")),
    }
}

/// Mirrors `normalizeResponsesUsage`.
pub fn normalize_responses_usage(usage: Option<&Value>) -> UsageTokens {
    let cached = normalize_token(nested_f64(usage, "input_tokens_details", "cached_tokens"));
    let input = normalize_token(top_f64(usage, "input_tokens"));
    UsageTokens {
        cache_creation_input_tokens: None,
        cache_read_input_tokens: Some(cached),
        input_tokens: Some((input - cached).max(0)),
        output_tokens: Some(normalize_token(top_f64(usage, "output_tokens"))),
        total_tokens: normalize_optional_token(top_f64(usage, "total_tokens")),
    }
}

/// Mirrors `normalizeAnthropicUsage`.
pub fn normalize_anthropic_usage(usage: Option<&Value>) -> UsageTokens {
    UsageTokens {
        cache_creation_input_tokens: normalize_optional_token(top_f64(
            usage,
            "cache_creation_input_tokens",
        )),
        cache_read_input_tokens: normalize_optional_token(top_f64(
            usage,
            "cache_read_input_tokens",
        )),
        input_tokens: normalize_optional_token(top_f64(usage, "input_tokens")),
        output_tokens: normalize_optional_token(top_f64(usage, "output_tokens")),
        total_tokens: normalize_optional_token(top_f64(usage, "total_tokens")),
    }
}

/// Mirrors `mergeAnthropicUsage` (next overrides current when present).
pub fn merge_anthropic_usage(current: UsageTokens, next: UsageTokens) -> UsageTokens {
    UsageTokens {
        cache_creation_input_tokens: next
            .cache_creation_input_tokens
            .or(current.cache_creation_input_tokens),
        cache_read_input_tokens: next
            .cache_read_input_tokens
            .or(current.cache_read_input_tokens),
        input_tokens: next.input_tokens.or(current.input_tokens),
        output_tokens: next.output_tokens.or(current.output_tokens),
        total_tokens: next.total_tokens.or(current.total_tokens),
    }
}

pub fn resolve_token_usage_session_id(
    session_id: Option<&str>,
    fallback_session_id: Option<&str>,
) -> String {
    if let Some(affinity) = request_context_store().and_then(|c| c.session_affinity) {
        let trimmed = affinity.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(s) = session_id {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(s) = fallback_session_id {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    String::new()
}

/// SQLite-backed usage storage is available in this build. Mirrors
/// `isTokenUsageStorageEnabled`, which gates on SQLite runtime support; the Rust
/// port bundles rusqlite so it is always supported.
pub fn is_token_usage_storage_enabled() -> bool {
    true
}

/// Mirrors the closure returned by `createCopilotTokenUsageRecorder` /
/// `createProviderTokenUsageRecorder`. Holds the recorder options and records a
/// usage event when invoked.
pub struct TokenUsageRecorder {
    pub endpoint: TokenUsageEndpoint,
    pub source: TokenUsageSource,
    pub model: String,
    pub provider_name: Option<String>,
    pub session_id: Option<String>,
    pub fallback_session_id: Option<String>,
}

impl TokenUsageRecorder {
    pub fn record(&self, usage: UsageTokens) {
        if !is_token_usage_storage_enabled() {
            return;
        }
        let event = match to_persisted_event(
            self.endpoint,
            self.source,
            &self.model,
            self.provider_name.as_deref(),
            self.session_id.as_deref(),
            self.fallback_session_id.as_deref(),
            None,
            &usage,
        ) {
            Some(event) => event,
            None => return,
        };
        if let Err(error) = write_token_usage_event(&event) {
            warn!("Failed to record token usage: {error}");
        }
    }
}

pub fn create_copilot_token_usage_recorder(
    endpoint: TokenUsageEndpoint,
    model: impl Into<String>,
    fallback_session_id: Option<String>,
) -> TokenUsageRecorder {
    TokenUsageRecorder {
        endpoint,
        source: "copilot",
        model: model.into(),
        provider_name: None,
        session_id: None,
        fallback_session_id,
    }
}

pub fn create_provider_token_usage_recorder(
    endpoint: TokenUsageEndpoint,
    model: impl Into<String>,
    provider_name: impl Into<String>,
    fallback_session_id: Option<String>,
) -> TokenUsageRecorder {
    TokenUsageRecorder {
        endpoint,
        source: "provider",
        model: model.into(),
        provider_name: Some(provider_name.into()),
        session_id: None,
        fallback_session_id,
    }
}

// ---------------------------------------------------------------------------
// SQLite persistence: structs, schema, writes, aggregation queries.
// Mirrors src/lib/token-usage/store.ts. JSON struct field order matches the TS
// object literals (alphabetical), and serde_json has preserve_order enabled so
// the rendered output is byte-compatible.
// ---------------------------------------------------------------------------

pub type TokenUsagePeriod = String; // "day" | "week" | "month"

/// Mirrors `PersistedTokenUsageEvent`.
#[derive(Debug, Clone, Serialize)]
pub struct PersistedTokenUsageEvent {
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub created_at_ms: i64,
    pub created_at_utc: String,
    pub endpoint: TokenUsageEndpoint,
    pub input_tokens: i64,
    pub model: String,
    pub output_tokens: i64,
    pub provider_name: Option<String>,
    pub session_id: String,
    pub source: TokenUsageSource,
    pub total_tokens: i64,
    pub trace_id: String,
    pub user_id: String,
}

/// Mirrors `TokenUsageEventRecord` (a persisted event plus its row id).
#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageEventRecord {
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub created_at_ms: i64,
    pub created_at_utc: String,
    pub endpoint: String,
    pub id: i64,
    pub input_tokens: i64,
    pub model: String,
    pub output_tokens: i64,
    pub provider_name: Option<String>,
    pub session_id: String,
    pub source: String,
    pub total_tokens: i64,
    pub trace_id: String,
    pub user_id: String,
}

/// Mirrors `TokenUsageTotals`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenUsageTotals {
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub request_count: i64,
    pub total_tokens: i64,
}

/// Mirrors `TokenUsageModelSummary` (`{ ...totals, model }`).
#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageModelSummary {
    #[serde(flatten)]
    pub totals: TokenUsageTotals,
    pub model: String,
}

/// Mirrors the inline `range` object on the summary shapes.
#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageRange {
    pub end_ms: i64,
    pub end_utc: String,
    pub start_ms: i64,
    pub start_utc: String,
}

/// Mirrors `TokenUsageSummary`.
#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageSummary {
    #[serde(rename = "byModel")]
    pub by_model: Vec<TokenUsageModelSummary>,
    pub period: TokenUsagePeriod,
    pub range: TokenUsageRange,
    pub totals: TokenUsageTotals,
}

/// Mirrors `TokenUsageDailyBucket`.
#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageDailyBucket {
    #[serde(rename = "byModel")]
    pub by_model: Vec<TokenUsageModelSummary>,
    pub date: String,
    pub end_ms: i64,
    pub start_ms: i64,
    pub totals: TokenUsageTotals,
}

/// Mirrors `TokenUsageDailySummary`.
#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageDailySummary {
    #[serde(rename = "byModel")]
    pub by_model: Vec<TokenUsageModelSummary>,
    pub days: Vec<TokenUsageDailyBucket>,
    pub period: TokenUsagePeriod,
    pub range: TokenUsageRange,
    pub totals: TokenUsageTotals,
}

/// Mirrors `TokenUsageEventsPage`.
#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageEventsPage {
    pub items: Vec<TokenUsageEventRecord>,
    pub page: i64,
    pub page_size: i64,
    pub period: TokenUsagePeriod,
    pub range: TokenUsageRange,
    pub total: i64,
    pub total_pages: i64,
}

/// Mirrors `resolveTotalTokens`: explicit total when present, otherwise the sum
/// of the four component counters.
pub fn resolve_total_tokens(input: &UsageTokens) -> i64 {
    if let Some(total) = normalize_optional_token(input.total_tokens.map(|v| v as f64)) {
        return total;
    }
    normalize_token(input.input_tokens.map(|v| v as f64))
        + normalize_token(input.output_tokens.map(|v| v as f64))
        + normalize_token(input.cache_read_input_tokens.map(|v| v as f64))
        + normalize_token(input.cache_creation_input_tokens.map(|v| v as f64))
}

/// Mirrors `resolveTraceId` in src/lib/token-usage/index.ts: trim the supplied
/// id, else fall back to the request-context trace id, else generate one.
fn resolve_token_usage_trace_id(trace_id: Option<&str>) -> String {
    if let Some(t) = trace_id {
        let trimmed = t.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(ctx) = request_context_store() {
        if !ctx.trace_id.is_empty() {
            return ctx.trace_id;
        }
    }
    generate_trace_id()
}

/// Mirrors `resolveUserId`.
fn resolve_user_id(source: TokenUsageSource, provider_name: Option<&str>) -> String {
    if source == "provider" {
        provider_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
    } else {
        state::with_state(|s| {
            s.user_name
                .as_deref()
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_default()
        })
    }
}

/// Mirrors `toPersistedEvent`: returns `None` when the usage carries no tokens.
#[allow(clippy::too_many_arguments)]
pub fn to_persisted_event(
    endpoint: TokenUsageEndpoint,
    source: TokenUsageSource,
    model: &str,
    provider_name: Option<&str>,
    session_id: Option<&str>,
    fallback_session_id: Option<&str>,
    trace_id: Option<&str>,
    usage: &UsageTokens,
) -> Option<PersistedTokenUsageEvent> {
    if !has_any_token(usage) {
        return None;
    }

    let now = Utc::now();
    let trimmed_model = model.trim();
    let model = if trimmed_model.is_empty() {
        "unknown".to_string()
    } else {
        trimmed_model.to_string()
    };
    let provider_name = provider_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    Some(PersistedTokenUsageEvent {
        cache_creation_input_tokens: normalize_token(
            usage.cache_creation_input_tokens.map(|v| v as f64),
        ),
        cache_read_input_tokens: normalize_token(usage.cache_read_input_tokens.map(|v| v as f64)),
        created_at_ms: now.timestamp_millis(),
        created_at_utc: now.to_rfc3339_opts(SecondsFormat::Millis, true),
        endpoint,
        input_tokens: normalize_token(usage.input_tokens.map(|v| v as f64)),
        model,
        output_tokens: normalize_token(usage.output_tokens.map(|v| v as f64)),
        provider_name: provider_name.clone(),
        session_id: resolve_token_usage_session_id(session_id, fallback_session_id),
        source,
        total_tokens: resolve_total_tokens(usage),
        trace_id: resolve_token_usage_trace_id(trace_id),
        user_id: resolve_user_id(source, provider_name.as_deref()),
    })
}

/// Mirrors `initializeTokenUsageDb`'s schema work (the PRAGMAs are issued by
/// sqlite.rs at connection open). Creates the table, runs the column migrations
/// and builds the indexes.
pub fn initialize_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS token_usage_events (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          created_at_ms INTEGER NOT NULL,
          created_at_utc TEXT NOT NULL,
          trace_id TEXT NOT NULL,
          session_id TEXT NOT NULL DEFAULT '',
          user_id TEXT NOT NULL DEFAULT '',
          source TEXT NOT NULL,
          endpoint TEXT NOT NULL,
          provider_name TEXT,
          model TEXT NOT NULL,
          input_tokens INTEGER NOT NULL DEFAULT 0,
          output_tokens INTEGER NOT NULL DEFAULT 0,
          cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
          cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
          total_tokens INTEGER NOT NULL DEFAULT 0
        )
        "#,
    )?;
    ensure_column(conn, "user_id", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "total_tokens", "INTEGER NOT NULL DEFAULT 0")?;
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_token_usage_events_created_at_ms
          ON token_usage_events(created_at_ms);
        CREATE INDEX IF NOT EXISTS idx_token_usage_events_model
          ON token_usage_events(model);
        CREATE INDEX IF NOT EXISTS idx_token_usage_events_trace_id
          ON token_usage_events(trace_id);
        CREATE INDEX IF NOT EXISTS idx_token_usage_events_session_id
          ON token_usage_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_token_usage_events_user_id
          ON token_usage_events(user_id);
        "#,
    )?;
    Ok(())
}

/// Mirrors `ensureColumn`: PRAGMA table_info + ALTER TABLE ADD COLUMN if absent.
fn ensure_column(conn: &Connection, name: &str, definition: &str) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(token_usage_events)")?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    if !existing.iter().any(|col| col == name) {
        conn.execute_batch(&format!(
            "ALTER TABLE token_usage_events ADD COLUMN {name} {definition}"
        ))?;
    }
    Ok(())
}

/// Mirrors `writeTokenUsageEvent` but synchronous, under the usage_db() mutex
/// (the async write-queue from TS is dropped).
pub fn write_token_usage_event(event: &PersistedTokenUsageEvent) -> rusqlite::Result<()> {
    let conn = usage_db().lock().expect("usage db mutex poisoned");
    conn.execute(
        r#"
        INSERT INTO token_usage_events (
          created_at_ms,
          created_at_utc,
          trace_id,
          session_id,
          user_id,
          source,
          endpoint,
          provider_name,
          model,
          input_tokens,
          output_tokens,
          cache_read_input_tokens,
          cache_creation_input_tokens,
          total_tokens
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        params![
            event.created_at_ms,
            event.created_at_utc,
            event.trace_id,
            event.session_id,
            event.user_id,
            event.source,
            event.endpoint,
            event.provider_name,
            event.model,
            event.input_tokens,
            event.output_tokens,
            event.cache_read_input_tokens,
            event.cache_creation_input_tokens,
            event.total_tokens,
        ],
    )?;
    Ok(())
}

// --- Range math (local time, mirroring the Date-based logic in store.ts) ---

struct DailyInterval {
    date: String,
    start_ms: i64,
    end_ms: i64,
}

fn local_from_millis(ms: i64) -> chrono::DateTime<Local> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_millis_opt(0).single().unwrap())
        .with_timezone(&Local)
}

fn local_midnight_ms(date: NaiveDate) -> i64 {
    let naive = date.and_hms_opt(0, 0, 0).unwrap_or_default();
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|| Utc.from_utc_datetime(&naive).timestamp_millis())
}

fn utc_iso(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_millis_opt(0).single().unwrap())
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Mirrors `getPeriodRange`: start = local midnight today (shifted back 6/29 days
/// for week/month); end = start + 1/7/30 days.
fn get_period_range(period: &str, now_ms: i64) -> (i64, i64) {
    let today = local_from_millis(now_ms).date_naive();
    let start_date = match period {
        "week" => today - Duration::days(6),
        "month" => today - Duration::days(29),
        _ => today,
    };
    let end_date = match period {
        "week" => start_date + Duration::days(7),
        "month" => start_date + Duration::days(30),
        _ => start_date + Duration::days(1),
    };
    (local_midnight_ms(start_date), local_midnight_ms(end_date))
}

/// Mirrors `formatLocalDate`.
fn format_local_date(ms: i64) -> String {
    local_from_millis(ms).format("%Y-%m-%d").to_string()
}

/// Mirrors `createDailyIntervals`: walk day-by-day in local time, clamping the
/// final interval to the range end.
fn create_daily_intervals(start_ms: i64, end_ms: i64) -> Vec<DailyInterval> {
    let mut intervals = Vec::new();
    let mut cursor = start_ms;
    while cursor < end_ms {
        let date = format_local_date(cursor);
        let next_date = local_from_millis(cursor).date_naive() + Duration::days(1);
        let next_ms = local_midnight_ms(next_date).min(end_ms);
        intervals.push(DailyInterval {
            date,
            start_ms: cursor,
            end_ms: next_ms,
        });
        cursor = next_ms;
    }
    intervals
}

fn range_payload(start_ms: i64, end_ms: i64) -> TokenUsageRange {
    TokenUsageRange {
        end_ms,
        end_utc: utc_iso(end_ms),
        start_ms,
        start_utc: utc_iso(start_ms),
    }
}

// --- Aggregation queries ---

fn get_totals_row(conn: &Connection, start_ms: i64, end_ms: i64) -> rusqlite::Result<TokenUsageTotals> {
    conn.query_row(
        r#"
        SELECT
          COUNT(*) AS request_count,
          COALESCE(SUM(input_tokens), 0) AS input_tokens,
          COALESCE(SUM(output_tokens), 0) AS output_tokens,
          COALESCE(SUM(cache_read_input_tokens), 0) AS cache_read_input_tokens,
          COALESCE(SUM(cache_creation_input_tokens), 0) AS cache_creation_input_tokens,
          COALESCE(SUM(total_tokens), 0) AS total_tokens
        FROM token_usage_events
        WHERE created_at_ms >= ? AND created_at_ms < ?
        "#,
        params![start_ms, end_ms],
        |row| {
            Ok(TokenUsageTotals {
                cache_creation_input_tokens: row.get("cache_creation_input_tokens")?,
                cache_read_input_tokens: row.get("cache_read_input_tokens")?,
                input_tokens: row.get("input_tokens")?,
                output_tokens: row.get("output_tokens")?,
                request_count: row.get("request_count")?,
                total_tokens: row.get("total_tokens")?,
            })
        },
    )
}

fn get_model_rows(
    conn: &Connection,
    start_ms: i64,
    end_ms: i64,
) -> rusqlite::Result<Vec<TokenUsageModelSummary>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
          model,
          COUNT(*) AS request_count,
          COALESCE(SUM(input_tokens), 0) AS input_tokens,
          COALESCE(SUM(output_tokens), 0) AS output_tokens,
          COALESCE(SUM(cache_read_input_tokens), 0) AS cache_read_input_tokens,
          COALESCE(SUM(cache_creation_input_tokens), 0) AS cache_creation_input_tokens,
          COALESCE(SUM(total_tokens), 0) AS total_tokens
        FROM token_usage_events
        WHERE created_at_ms >= ? AND created_at_ms < ?
        GROUP BY model
        ORDER BY total_tokens DESC, model ASC
        "#,
    )?;
    let rows = stmt
        .query_map(params![start_ms, end_ms], |row| {
            let model: Option<String> = row.get("model")?;
            Ok(TokenUsageModelSummary {
                totals: TokenUsageTotals {
                    cache_creation_input_tokens: row.get("cache_creation_input_tokens")?,
                    cache_read_input_tokens: row.get("cache_read_input_tokens")?,
                    input_tokens: row.get("input_tokens")?,
                    output_tokens: row.get("output_tokens")?,
                    request_count: row.get("request_count")?,
                    total_tokens: row.get("total_tokens")?,
                },
                model: model.unwrap_or_else(|| "unknown".to_string()),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn sum_model_totals(models: &[TokenUsageModelSummary]) -> TokenUsageTotals {
    let mut totals = TokenUsageTotals::default();
    for model in models {
        totals.cache_creation_input_tokens += model.totals.cache_creation_input_tokens;
        totals.cache_read_input_tokens += model.totals.cache_read_input_tokens;
        totals.input_tokens += model.totals.input_tokens;
        totals.output_tokens += model.totals.output_tokens;
        totals.request_count += model.totals.request_count;
        totals.total_tokens += model.totals.total_tokens;
    }
    totals
}

// --- Empty (well-formed) shapes for the DB-error / disabled paths ---

fn create_empty_summary(period: &str) -> TokenUsageSummary {
    let (start_ms, end_ms) = get_period_range(period, Utc::now().timestamp_millis());
    TokenUsageSummary {
        by_model: Vec::new(),
        period: period.to_string(),
        range: range_payload(start_ms, end_ms),
        totals: TokenUsageTotals::default(),
    }
}

fn create_empty_daily_summary(period: &str) -> TokenUsageDailySummary {
    let (start_ms, end_ms) = get_period_range(period, Utc::now().timestamp_millis());
    let days = create_daily_intervals(start_ms, end_ms)
        .into_iter()
        .map(|interval| TokenUsageDailyBucket {
            by_model: Vec::new(),
            date: interval.date,
            end_ms: interval.end_ms,
            start_ms: interval.start_ms,
            totals: TokenUsageTotals::default(),
        })
        .collect();
    TokenUsageDailySummary {
        by_model: Vec::new(),
        days,
        period: period.to_string(),
        range: range_payload(start_ms, end_ms),
        totals: TokenUsageTotals::default(),
    }
}

fn create_empty_events_page(page: i64, page_size: i64, period: &str) -> TokenUsageEventsPage {
    let (start_ms, end_ms) = get_period_range(period, Utc::now().timestamp_millis());
    TokenUsageEventsPage {
        items: Vec::new(),
        page: page.max(1),
        page_size: page_size.clamp(1, 100),
        period: period.to_string(),
        range: range_payload(start_ms, end_ms),
        total: 0,
        total_pages: 1,
    }
}

// --- Public read API (mirrors the getTokenUsage* functions) ---

pub fn get_token_usage_summary(period: &str) -> TokenUsageSummary {
    if !is_token_usage_storage_enabled() {
        return create_empty_summary(period);
    }
    let (start_ms, end_ms) = get_period_range(period, Utc::now().timestamp_millis());
    match summary_inner(period, start_ms, end_ms) {
        Ok(summary) => summary,
        Err(error) => {
            warn!("Failed to read token usage summary: {error}");
            create_empty_summary(period)
        }
    }
}

fn summary_inner(period: &str, start_ms: i64, end_ms: i64) -> rusqlite::Result<TokenUsageSummary> {
    let conn = usage_db().lock().expect("usage db mutex poisoned");
    let totals = get_totals_row(&conn, start_ms, end_ms)?;
    let by_model = get_model_rows(&conn, start_ms, end_ms)?;
    Ok(TokenUsageSummary {
        by_model,
        period: period.to_string(),
        range: range_payload(start_ms, end_ms),
        totals,
    })
}

pub fn get_token_usage_daily_summary(period: &str) -> TokenUsageDailySummary {
    if !is_token_usage_storage_enabled() {
        return create_empty_daily_summary(period);
    }
    let (start_ms, end_ms) = get_period_range(period, Utc::now().timestamp_millis());
    match daily_summary_inner(period, start_ms, end_ms) {
        Ok(summary) => summary,
        Err(error) => {
            warn!("Failed to read token usage daily summary: {error}");
            create_empty_daily_summary(period)
        }
    }
}

fn daily_summary_inner(
    period: &str,
    start_ms: i64,
    end_ms: i64,
) -> rusqlite::Result<TokenUsageDailySummary> {
    let conn = usage_db().lock().expect("usage db mutex poisoned");
    let totals = get_totals_row(&conn, start_ms, end_ms)?;
    let by_model = get_model_rows(&conn, start_ms, end_ms)?;
    let mut days = Vec::new();
    for interval in create_daily_intervals(start_ms, end_ms) {
        let models = get_model_rows(&conn, interval.start_ms, interval.end_ms)?;
        let bucket_totals = sum_model_totals(&models);
        days.push(TokenUsageDailyBucket {
            by_model: models,
            date: interval.date,
            end_ms: interval.end_ms,
            start_ms: interval.start_ms,
            totals: bucket_totals,
        });
    }
    Ok(TokenUsageDailySummary {
        by_model,
        days,
        period: period.to_string(),
        range: range_payload(start_ms, end_ms),
        totals,
    })
}

pub fn get_token_usage_events_page(page: i64, page_size: i64, period: &str) -> TokenUsageEventsPage {
    let page = page.max(1);
    let page_size = page_size.clamp(1, 100);
    if !is_token_usage_storage_enabled() {
        return create_empty_events_page(page, page_size, period);
    }
    let (start_ms, end_ms) = get_period_range(period, Utc::now().timestamp_millis());
    match events_page_inner(period, page, page_size, start_ms, end_ms) {
        Ok(page_result) => page_result,
        Err(error) => {
            warn!("Failed to read token usage events page: {error}");
            create_empty_events_page(page, page_size, period)
        }
    }
}

fn events_page_inner(
    period: &str,
    page: i64,
    page_size: i64,
    start_ms: i64,
    end_ms: i64,
) -> rusqlite::Result<TokenUsageEventsPage> {
    let offset = (page - 1) * page_size;
    let conn = usage_db().lock().expect("usage db mutex poisoned");

    let total: i64 = conn.query_row(
        r#"
        SELECT COUNT(*) AS total
        FROM token_usage_events
        WHERE created_at_ms >= ? AND created_at_ms < ?
        "#,
        params![start_ms, end_ms],
        |row| row.get(0),
    )?;

    let mut stmt = conn.prepare(
        r#"
        SELECT
          id,
          created_at_ms,
          created_at_utc,
          trace_id,
          session_id,
          user_id,
          source,
          endpoint,
          provider_name,
          model,
          input_tokens,
          output_tokens,
          cache_read_input_tokens,
          cache_creation_input_tokens,
          total_tokens
        FROM token_usage_events
        WHERE created_at_ms >= ? AND created_at_ms < ?
        ORDER BY created_at_ms DESC, id DESC
        LIMIT ? OFFSET ?
        "#,
    )?;
    let items = stmt
        .query_map(params![start_ms, end_ms, page_size, offset], |row| {
            let model: Option<String> = row.get("model")?;
            Ok(TokenUsageEventRecord {
                cache_creation_input_tokens: row.get("cache_creation_input_tokens")?,
                cache_read_input_tokens: row.get("cache_read_input_tokens")?,
                created_at_ms: row.get("created_at_ms")?,
                created_at_utc: row.get("created_at_utc")?,
                endpoint: row.get("endpoint")?,
                id: row.get("id")?,
                input_tokens: row.get("input_tokens")?,
                model: model.filter(|m| !m.is_empty()).unwrap_or_else(|| "unknown".to_string()),
                output_tokens: row.get("output_tokens")?,
                provider_name: row.get("provider_name")?,
                session_id: row.get("session_id")?,
                source: row.get("source")?,
                total_tokens: row.get("total_tokens")?,
                trace_id: row.get("trace_id")?,
                user_id: row.get("user_id")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let total_pages = if page_size > 0 {
        ((total + page_size - 1) / page_size).max(1)
    } else {
        1
    };

    Ok(TokenUsageEventsPage {
        items,
        page,
        page_size,
        period: period.to_string(),
        range: range_payload(start_ms, end_ms),
        total,
        total_pages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_persisted_event_returns_none_without_tokens() {
        let usage = UsageTokens::default();
        let event = to_persisted_event(
            "chat_completions",
            "copilot",
            "gpt-4o",
            None,
            None,
            None,
            None,
            &usage,
        );
        assert!(event.is_none());
    }

    #[test]
    fn to_persisted_event_populates_fields() {
        let usage = UsageTokens {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..Default::default()
        };
        let event = to_persisted_event(
            "chat_completions",
            "provider",
            "  my-model  ",
            Some("  acme  "),
            Some("sess-1"),
            None,
            Some("trace-123"),
            &usage,
        )
        .expect("event should be produced when tokens are present");
        assert_eq!(event.model, "my-model");
        assert_eq!(event.provider_name.as_deref(), Some("acme"));
        assert_eq!(event.user_id, "acme");
        assert_eq!(event.trace_id, "trace-123");
        assert_eq!(event.total_tokens, 15);
        assert_eq!(event.endpoint, "chat_completions");
    }

    #[test]
    fn insert_then_summary_roundtrip() {
        // Point the usage DB at a fresh temp file via the env override. This test
        // owns the process-global connection, so it must run before any other
        // test touches usage_db(); cargo runs tests in one binary, so we guard by
        // only asserting on rows we insert within our own time range.
        let dir = std::env::temp_dir().join(format!("copilot-api-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("usage.sqlite");
        std::env::set_var("COPILOT_API_SQLITE_DB_PATH", &db_path);

        let usage = UsageTokens {
            input_tokens: Some(100),
            output_tokens: Some(40),
            ..Default::default()
        };
        let event = to_persisted_event(
            "chat_completions",
            "copilot",
            "roundtrip-model",
            None,
            None,
            None,
            Some("trace-roundtrip"),
            &usage,
        )
        .expect("event present");

        // Only run the DB roundtrip if this test is the one that initialized the
        // global connection at our temp path.
        let path_matches = {
            let conn = usage_db().lock().unwrap();
            conn.path().map(|p| std::path::Path::new(p) == db_path).unwrap_or(false)
        };
        if !path_matches {
            return;
        }

        write_token_usage_event(&event).expect("write succeeds");

        let summary = get_token_usage_summary("day");
        assert!(summary.totals.request_count >= 1);
        assert!(summary
            .by_model
            .iter()
            .any(|m| m.model == "roundtrip-model" && m.totals.total_tokens >= 140));

        let events = get_token_usage_events_page(1, 20, "day");
        assert!(events.total >= 1);
        assert!(events.items.iter().any(|e| e.model == "roundtrip-model"));
    }
}
