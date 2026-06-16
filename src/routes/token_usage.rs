use axum::extract::Query;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::libs::token_usage::{
    get_token_usage_daily_summary, get_token_usage_events_page, get_token_usage_summary,
};

const DEFAULT_EVENTS_PAGE_SIZE: i64 = 20;

/// Mirrors routes/token-usage/route.ts. Each handler reads the `period` query
/// param (default "day"), with `/events` adding `page` and `page_size`.
pub fn router() -> Router {
    Router::new()
        .route("/", get(get_summary))
        .route("/daily", get(get_daily))
        .route("/events", get(get_events))
}

#[derive(Debug, Deserialize)]
struct PeriodQuery {
    period: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    period: Option<String>,
    page: Option<String>,
    page_size: Option<String>,
}

/// Mirrors `parsePeriod`: only day/week/month are valid, else "day".
fn parse_period(value: Option<&str>) -> String {
    match value {
        Some("day") | Some("week") | Some("month") => value.unwrap().to_string(),
        _ => "day".to_string(),
    }
}

/// Mirrors `parsePositiveInt`: parse base-10, keep if finite and > 0, else
/// fallback.
fn parse_positive_int(value: Option<&str>, fallback: i64) -> i64 {
    match value.and_then(|v| v.trim().parse::<i64>().ok()) {
        Some(parsed) if parsed > 0 => parsed,
        _ => fallback,
    }
}

async fn get_summary(Query(query): Query<PeriodQuery>) -> Response {
    let period = parse_period(query.period.as_deref());
    Json(get_token_usage_summary(&period)).into_response()
}

async fn get_daily(Query(query): Query<PeriodQuery>) -> Response {
    let period = parse_period(query.period.as_deref());
    Json(get_token_usage_daily_summary(&period)).into_response()
}

async fn get_events(Query(query): Query<EventsQuery>) -> Response {
    let period = parse_period(query.period.as_deref());
    let page = parse_positive_int(query.page.as_deref(), 1);
    let page_size = parse_positive_int(query.page_size.as_deref(), DEFAULT_EVENTS_PAGE_SIZE);
    Json(get_token_usage_events_page(page, page_size, &period)).into_response()
}
