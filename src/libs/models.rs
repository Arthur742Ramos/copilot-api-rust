use crate::libs::state;
use crate::services::copilot::get_models::Model;

// Mirrors src/lib/models.ts.

/// The bracketed suffix the `/v1/models` route advertises on the 1M-context
/// variant of a model (e.g. `claude-opus-4.8[1m]`). It is non-standard: the
/// upstream only understands the base id, so resolution strips it. Its presence
/// signals that the `context-1m-2025-08-07` beta must be enabled.
pub const CONTEXT_1M_SUFFIX: &str = "[1m]";

/// Whether `model_id` carries the `[1m]` 1M-context variant suffix.
pub fn is_context_1m_model(model_id: &str) -> bool {
    model_id.ends_with(CONTEXT_1M_SUFFIX)
}

/// Strips a trailing `[1m]` variant suffix, returning the base model id. A model
/// id without the suffix is returned unchanged.
pub fn strip_context_1m_suffix(model_id: &str) -> &str {
    model_id.strip_suffix(CONTEXT_1M_SUFFIX).unwrap_or(model_id)
}

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
    // The `/v1/models` route advertises the 1M-context variant with a `[1m]`
    // suffix; the upstream catalogue only knows the base id, so resolve against
    // that. The beta header carrying the variant is injected separately.
    let sdk_model_id = strip_context_1m_suffix(sdk_model_id);

    let models = state::with_state(|s| {
        s.models
            .as_ref()
            .map(|m| m.data.clone())
            .unwrap_or_default()
    });

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_strips_context_1m_suffix() {
        assert!(is_context_1m_model("claude-opus-4.8[1m]"));
        assert!(!is_context_1m_model("claude-opus-4.8"));
        assert_eq!(
            strip_context_1m_suffix("claude-opus-4.8[1m]"),
            "claude-opus-4.8"
        );
        assert_eq!(
            strip_context_1m_suffix("claude-opus-4.8"),
            "claude-opus-4.8"
        );
    }

    #[test]
    fn bracketed_1m_resolves_to_base_for_client_id() {
        // to_client_model_id strips the date suffix etc. via normalize; the [1m]
        // variant must normalize the same as its base after the suffix is gone.
        assert_eq!(
            to_client_model_id(strip_context_1m_suffix("claude-opus-4.8[1m]")),
            to_client_model_id("claude-opus-4.8")
        );
    }

    #[test]
    fn normalize_resolves_base_after_suffix_strip() {
        let n = normalize_sdk_model_id(strip_context_1m_suffix("claude-opus-4.8[1m]")).unwrap();
        assert_eq!(n.family, "opus");
        assert_eq!(n.version, "4.8");
    }
}
