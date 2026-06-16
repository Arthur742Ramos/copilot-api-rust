use crate::libs::state;
use crate::services::copilot::get_models::Model;

/// Mirrors src/lib/models.ts.

pub struct NormalizedSdkModelId {
    pub family: String,
    pub version: String,
}

/// Converts a Copilot upstream model ID to a client-friendly ID. Non-Claude
/// models are returned unchanged.
pub fn to_client_model_id(model_id: &str) -> String {
    match normalize_sdk_model_id(model_id) {
        Some(n) => {
            let version_hyphenated = n.version.replace('.', "-");
            format!("claude-{}-{}", n.family, version_hyphenated)
        }
        None => model_id.to_string(),
    }
}

pub fn find_endpoint_model(sdk_model_id: &str) -> Option<Model> {
    let models = state::with_state(|s| s.models.as_ref().map(|m| m.data.clone()).unwrap_or_default());

    if let Some(exact) = models.iter().find(|m| m.id == sdk_model_id) {
        return Some(exact.clone());
    }

    let normalized = normalize_sdk_model_id(sdk_model_id)?;
    let model_name = format!("claude-{}-{}", normalized.family, normalized.version);
    models.into_iter().find(|m| m.id == model_name)
}

fn strip_date_suffix(lower: &str) -> &str {
    // Strip a trailing `-` followed by exactly 8 digits.
    if let Some(idx) = lower.rfind('-') {
        let suffix = &lower[idx + 1..];
        if suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_digit()) {
            return &lower[..idx];
        }
    }
    lower
}

/// Normalizes an SDK model ID to extract family + version. Mirrors the five
/// regex patterns in `normalizeSdkModelId`.
pub fn normalize_sdk_model_id(sdk_model_id: &str) -> Option<NormalizedSdkModelId> {
    let lower = sdk_model_id.to_lowercase();
    let without_date = strip_date_suffix(&lower);

    // Pattern 1: claude-{family}-{major}.{minor}
    if let Some(c) = regex_match(without_date, r"^claude-(\w+)-(\d+)\.(\d+)$") {
        return Some(NormalizedSdkModelId {
            family: c[0].clone(),
            version: format!("{}.{}", c[1], c[2]),
        });
    }
    // Pattern 2: claude-{family}-{major}-{minor}
    if let Some(c) = regex_match(without_date, r"^claude-(\w+)-(\d+)-(\d+)$") {
        return Some(NormalizedSdkModelId {
            family: c[0].clone(),
            version: format!("{}.{}", c[1], c[2]),
        });
    }
    // Pattern 3: claude-{major}-{minor}-{family}
    if let Some(c) = regex_match(without_date, r"^claude-(\d+)-(\d+)-(\w+)$") {
        return Some(NormalizedSdkModelId {
            family: c[2].clone(),
            version: format!("{}.{}", c[0], c[1]),
        });
    }
    // Pattern 4: claude-{family}-{major}
    if let Some(c) = regex_match(without_date, r"^claude-(\w+)-(\d+)$") {
        return Some(NormalizedSdkModelId {
            family: c[0].clone(),
            version: c[1].clone(),
        });
    }
    // Pattern 5: claude-{major}-{family}
    if let Some(c) = regex_match(without_date, r"^claude-(\d+)-(\w+)$") {
        return Some(NormalizedSdkModelId {
            family: c[1].clone(),
            version: c[0].clone(),
        });
    }
    None
}

fn regex_match(haystack: &str, pattern: &str) -> Option<Vec<String>> {
    let re = regex::Regex::new(pattern).ok()?;
    let caps = re.captures(haystack)?;
    Some(
        caps.iter()
            .skip(1)
            .map(|m| m.map(|x| x.as_str().to_string()).unwrap_or_default())
            .collect(),
    )
}
