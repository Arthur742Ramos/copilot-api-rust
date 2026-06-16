use std::time::{SystemTime, UNIX_EPOCH};

/// Mirrors src/lib/request-context.ts. The TS code uses Node's AsyncLocalStorage
/// to carry per-request metadata through the async call tree. The Rust analogue
/// is a tokio task-local; `request_context_store()` reads the current value.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub trace_id: String,
    pub start_time: u128,
    pub user_agent: String,
    pub session_affinity: Option<String>,
    pub parent_session_id: Option<String>,
}

const TRACE_ID_MAX_LENGTH: usize = 64;

tokio::task_local! {
    static REQUEST_CONTEXT: RequestContext;
}

/// Run `fut` with `context` installed as the current request context.
pub async fn run_with_context<F, T>(context: RequestContext, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    REQUEST_CONTEXT.scope(context, fut).await
}

/// Equivalent of `requestContext.getStore()` — returns a clone of the current
/// context if one is installed for this task.
pub fn request_context_store() -> Option<RequestContext> {
    REQUEST_CONTEXT.try_with(|ctx| ctx.clone()).ok()
}

pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn to_base36(mut n: u128) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(ALPHABET[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

pub fn generate_trace_id() -> String {
    let timestamp = to_base36(now_millis());
    // Mirror Math.random().toString(36).slice(2, 8): six base36 chars.
    let random: u64 = rand::random();
    let mut random_part = to_base36(random as u128);
    random_part.truncate(6);
    while random_part.len() < 6 {
        random_part.push('0');
    }
    format!("{timestamp}-{random_part}")
}

/// `\w[\w.-]*` — first char is a word char, rest are word/`.`/`-`.
fn is_valid_trace_id(candidate: &str) -> bool {
    let mut chars = candidate.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

pub fn resolve_trace_id(trace_id: Option<&str>) -> String {
    let candidate = trace_id.map(|s| s.trim()).unwrap_or("");
    if candidate.is_empty()
        || candidate.len() > TRACE_ID_MAX_LENGTH
        || !is_valid_trace_id(candidate)
    {
        return generate_trace_id();
    }
    candidate.to_string()
}
