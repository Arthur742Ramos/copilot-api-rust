//! `copilot-api doctor` — a one-shot preflight that reports config / auth /
//! provider problems and exits non-zero when any check FAILs, so it can gate a
//! CI step or a deployment script.
//!
//! It reuses the same startup primitives the server uses (paths, GitHub/Copilot
//! token setup, the model cache) plus the live provider health probe, then runs
//! a fixed set of named checks. Each check reports `OK | WARN | FAIL` with a
//! short, secret-free message. The command never prints tokens or apiKeys.

use std::collections::HashSet;

use serde::Serialize;

use crate::libs::config::{self, AppConfig};
use crate::libs::credential_store::read_codex_credentials;
use crate::libs::models::{strip_context_1m_suffix, to_client_model_id};
use crate::libs::paths::ensure_paths;
use crate::libs::state;
use crate::services::providers::provider_proxy::{probe_provider_models, ProbeOutcome};

/// The outcome level of a single doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

/// A single named check result. `message` is always secret-free.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub message: String,
}

impl Check {
    fn new(name: impl Into<String>, status: Status, message: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            status,
            message: message.into(),
        }
    }
}

/// Roll-up counts across all checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
}

/// Pure roll-up: count statuses and derive the process exit code. The exit code
/// is non-zero iff at least one check FAILed (WARNs never fail the preflight, so
/// advisory drift can't break CI). Kept pure and free of I/O so it is unit
/// testable.
pub fn summarize(checks: &[Check]) -> (Counts, i32) {
    let mut counts = Counts::default();
    for check in checks {
        match check.status {
            Status::Ok => counts.ok += 1,
            Status::Warn => counts.warn += 1,
            Status::Fail => counts.fail += 1,
        }
    }
    let exit_code = if counts.fail > 0 { 1 } else { 0 };
    (counts, exit_code)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorReport {
    checks: Vec<Check>,
    summary: Counts,
    exit_code: i32,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// --- Tokens / auth ---------------------------------------------------------

/// Check GitHub + Copilot token health and (when configured) Codex credentials.
/// Also leaves a warm Copilot token and model cache in `state` for the later
/// drift check. Never triggers the interactive device-code login flow: a missing
/// GitHub token is reported as FAIL rather than blocking on a prompt.
async fn check_auth() -> Vec<Check> {
    let mut checks = Vec::new();

    // GitHub token: probe presence WITHOUT calling setup_github_token when the
    // token is absent, because setup_github_token would otherwise start the
    // interactive device-code flow (wrong for a non-interactive preflight).
    let github_token = match crate::libs::credential_store::read_github_token().await {
        Ok(token) => token.filter(|t| !t.trim().is_empty()),
        Err(e) => {
            checks.push(Check::new(
                "auth.github",
                Status::Fail,
                format!("Could not read GitHub token: {e}"),
            ));
            None
        }
    };

    let github_ok = match github_token {
        None => {
            checks.push(Check::new(
                "auth.github",
                Status::Fail,
                "GitHub token missing. Run `copilot-api auth` to log in.",
            ));
            false
        }
        Some(_) => {
            // Token present: validate it is actually usable (setup_github_token
            // with force=false reuses the on-disk token and calls log_user,
            // which hits the GitHub API and fails on a revoked/expired token).
            match crate::libs::token::setup_github_token(false).await {
                Ok(()) => {
                    let user = state::with_state(|s| s.user_name.clone());
                    let who = user
                        .map(|u| format!(" (logged in as {u})"))
                        .unwrap_or_default();
                    checks.push(Check::new(
                        "auth.github",
                        Status::Ok,
                        format!("GitHub token present and usable{who}."),
                    ));
                    true
                }
                Err(e) => {
                    checks.push(Check::new(
                        "auth.github",
                        Status::Fail,
                        format!("GitHub token present but unusable: {e}"),
                    ));
                    false
                }
            }
        }
    };

    // Copilot token freshness — only attempt when GitHub auth worked, since the
    // Copilot token is exchanged from the GitHub token.
    if github_ok {
        match crate::libs::token::setup_copilot_token().await {
            Ok(()) => {
                let token = state::with_state(|s| s.copilot_token.clone());
                match token {
                    Some(token) if !token.is_empty() => {
                        if crate::routes::token::copilot_token_is_fresh(&token, now_secs()) {
                            checks.push(Check::new(
                                "auth.copilot",
                                Status::Ok,
                                "Copilot token obtained and fresh.",
                            ));
                        } else {
                            checks.push(Check::new(
                                "auth.copilot",
                                Status::Warn,
                                "Copilot token is stale (past its expiry); it refreshes on use.",
                            ));
                        }
                    }
                    _ => checks.push(Check::new(
                        "auth.copilot",
                        Status::Fail,
                        "No Copilot token is held after setup.",
                    )),
                }
            }
            Err(e) => checks.push(Check::new(
                "auth.copilot",
                Status::Fail,
                format!("Could not obtain a Copilot token: {e}"),
            )),
        }
    }

    // Codex credentials — only relevant when a `codex` provider is configured.
    if config::get_raw_provider_config("codex").is_some() {
        match read_codex_credentials().await {
            Ok(Some(creds)) => {
                if crate::libs::oauth::codex::is_codex_credentials_expired(creds.expires_at, None) {
                    checks.push(Check::new(
                        "auth.codex",
                        Status::Warn,
                        "Codex credentials are expired; they refresh on next use.",
                    ));
                } else {
                    checks.push(Check::new(
                        "auth.codex",
                        Status::Ok,
                        "Codex credentials present and valid.",
                    ));
                }
            }
            Ok(None) => checks.push(Check::new(
                "auth.codex",
                Status::Fail,
                "Codex provider is configured but credentials are missing. \
                 Run `copilot-api auth --provider codex`.",
            )),
            Err(e) => checks.push(Check::new(
                "auth.codex",
                Status::Fail,
                format!("Codex provider is configured but credentials could not be read: {e}"),
            )),
        }
    }

    checks
}

// --- Providers -------------------------------------------------------------

/// Map a single provider probe outcome to a check. 200/404 are reachable+OK
/// (404 just means the upstream lacks `/v1/models`), 401/403 FAIL as a rejected
/// key, an SSRF-blocked base URL or a connect/timeout failure FAILs, and any
/// other HTTP status is a soft WARN.
fn provider_check(name: &str, outcome: &ProbeOutcome, latency_ms: u128) -> Check {
    let check_name = format!("provider.{name}");
    match outcome {
        ProbeOutcome::Status(200) | ProbeOutcome::Status(404) => Check::new(
            check_name,
            Status::Ok,
            format!("Reachable (status {}, {latency_ms}ms).", status_of(outcome)),
        ),
        ProbeOutcome::Status(401) | ProbeOutcome::Status(403) => Check::new(
            check_name,
            Status::Fail,
            format!(
                "Authentication rejected (status {}). Check the provider apiKey.",
                status_of(outcome)
            ),
        ),
        ProbeOutcome::Status(status) => Check::new(
            check_name,
            Status::Warn,
            format!("Reachable but returned status {status} ({latency_ms}ms)."),
        ),
        ProbeOutcome::Unreachable(reason) if reason == "blocked" => Check::new(
            check_name,
            Status::Fail,
            "Base URL is blocked by the SSRF guard (loopback/link-local/private).".to_string(),
        ),
        ProbeOutcome::Unreachable(reason) => Check::new(
            check_name,
            Status::Fail,
            format!("Unreachable ({reason}, {latency_ms}ms)."),
        ),
    }
}

fn status_of(outcome: &ProbeOutcome) -> u16 {
    match outcome {
        ProbeOutcome::Status(s) => *s,
        ProbeOutcome::Unreachable(_) => 0,
    }
}

/// Actively probe every ENABLED third-party provider. Disabled / misconfigured
/// providers are skipped (`list_enabled_providers` already filters them).
async fn check_providers() -> Vec<Check> {
    let configs: Vec<_> = config::list_enabled_providers()
        .into_iter()
        .filter_map(|name| config::get_provider_config(&name))
        .collect();

    let probes = configs.iter().map(|cfg| async move {
        let (outcome, latency_ms) = probe_provider_models(cfg).await;
        provider_check(&cfg.name, &outcome, latency_ms)
    });
    futures::future::join_all(probes).await
}

// --- Config model-id drift -------------------------------------------------

/// Catalogs the doctor cross-checks config model ids against.
pub struct ModelCatalogs {
    /// Client-facing model ids the gateway can serve a chat / messages request
    /// for: the live Copilot catalog (state.models, normalized via
    /// `to_client_model_id`) unioned with the static Codex catalog. Small-model,
    /// reasoning-effort, extra-prompt and mapping-target lookups resolve against
    /// whichever model is invoked (Copilot OR Codex transport), so the
    /// verification set is the union — checking only the Copilot list would flag
    /// the default Codex-keyed entries (gpt-5.4, gpt-5.5, …) as false drift.
    pub general: HashSet<String>,
    /// The Codex/image catalog that backs `imageChatModel` over the Codex
    /// transport.
    pub codex: HashSet<String>,
}

impl ModelCatalogs {
    /// Build the catalogs from process state: the cached Copilot catalog (loaded
    /// by `cache_models`) plus the static Codex catalog. Safe to call even when
    /// `cache_models` failed — `general` is then just the Codex set.
    fn from_state() -> Self {
        let mut general = HashSet::new();
        let mut codex = HashSet::new();

        state::with_state(|s| {
            if let Some(models) = s.models.as_ref() {
                for model in &models.data {
                    general.insert(model.id.clone());
                    general.insert(to_client_model_id(&model.id));
                }
            }
        });

        for model in crate::services::codex::get_models::get_codex_models().data {
            general.insert(model.id.clone());
            codex.insert(model.id);
        }

        ModelCatalogs { general, codex }
    }
}

/// Whether a configured model id is present in `catalog`. An empty/whitespace
/// value is treated as "unset" (known) so an absent optional field is not drift.
/// Tolerates the `[1m]` context variant suffix.
fn model_known(catalog: &HashSet<String>, value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return true;
    }
    catalog.contains(v) || catalog.contains(strip_context_1m_suffix(v))
}

/// Build a single OK/WARN check for one scalar model-id config field. Drift is a
/// WARN (never FAIL): a dangling id silently no-ops at runtime but should not
/// break a CI preflight, and the catalog the doctor loads is not always complete.
fn scalar_drift_check(
    field: &str,
    value: Option<&String>,
    catalog: &HashSet<String>,
) -> Option<Check> {
    let value = value.map(|v| v.trim()).filter(|v| !v.is_empty())?;
    if model_known(catalog, value) {
        Some(Check::new(
            format!("config.{field}"),
            Status::Ok,
            format!("'{value}' is a known model."),
        ))
    } else {
        Some(Check::new(
            format!("config.{field}"),
            Status::Warn,
            format!("'{value}' is not in the model catalog; it will be silently ignored."),
        ))
    }
}

/// Build a single OK/WARN check for a map-keyed-by-model-id field, reporting any
/// keys (or, for `modelMappings`, any TARGETS) that no longer resolve to a known
/// model. `entries` is `(label, model_id)` pairs already extracted by the caller.
fn map_drift_check(field: &str, entries: &[(String, String)], catalog: &HashSet<String>) -> Check {
    let drifted: Vec<String> = entries
        .iter()
        .filter(|(_, model)| !model_known(catalog, model))
        .map(|(label, _)| label.clone())
        .collect();

    if drifted.is_empty() {
        Check::new(
            format!("config.{field}"),
            Status::Ok,
            format!("All {} entr{} reference known models.", entries.len(), {
                if entries.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            }),
        )
    } else {
        Check::new(
            format!("config.{field}"),
            Status::Warn,
            format!(
                "{} entr{} reference unknown models (silently ignored): {}.",
                drifted.len(),
                if drifted.len() == 1 { "y" } else { "ies" },
                drifted.join(", ")
            ),
        )
    }
}

/// Pure model-id drift classification. Cross-checks each config field against the
/// catalog it belongs to and returns one check per populated field. Kept pure
/// (no state / network) so it can be unit tested against a synthetic catalog.
///
/// `modelMappings` KEYS are arbitrary client aliases, so only the TARGETS are
/// checked. `imageChatModel` is scoped to the Codex catalog; `imageModel` is the
/// image-generation model (no catalog the doctor can load) and is intentionally
/// not checked here.
pub fn classify_model_drift(catalogs: &ModelCatalogs, cfg: &AppConfig) -> Vec<Check> {
    let mut checks = Vec::new();

    if let Some(c) = scalar_drift_check("smallModel", cfg.small_model.as_ref(), &catalogs.general) {
        checks.push(c);
    }
    if let Some(c) = scalar_drift_check(
        "messageApiWebSearchModel",
        cfg.message_api_web_search_model.as_ref(),
        &catalogs.general,
    ) {
        checks.push(c);
    }
    if let Some(c) = scalar_drift_check(
        "imageChatModel",
        cfg.image_chat_model.as_ref(),
        &catalogs.codex,
    ) {
        checks.push(c);
    }

    // modelMappings: only TARGETS are real model ids; KEYS are client aliases.
    if let Some(mappings) = cfg.model_mappings.as_ref() {
        let entries: Vec<(String, String)> = mappings
            .iter()
            .filter_map(|(source, target)| match target {
                serde_json::Value::String(t) if !t.trim().is_empty() => {
                    Some((format!("{source} -> {t}"), t.clone()))
                }
                _ => None,
            })
            .collect();
        if !entries.is_empty() {
            checks.push(map_drift_check(
                "modelMappings",
                &entries,
                &catalogs.general,
            ));
        }
    }

    if let Some(efforts) = cfg.model_reasoning_efforts.as_ref() {
        let entries: Vec<(String, String)> = efforts
            .keys()
            .map(|k| (k.clone(), k.clone()))
            .filter(|(_, m)| !m.trim().is_empty())
            .collect();
        if !entries.is_empty() {
            checks.push(map_drift_check(
                "modelReasoningEfforts",
                &entries,
                &catalogs.general,
            ));
        }
    }

    if let Some(prompts) = cfg.extra_prompts.as_ref() {
        let entries: Vec<(String, String)> = prompts
            .keys()
            .map(|k| (k.clone(), k.clone()))
            .filter(|(_, m)| !m.trim().is_empty())
            .collect();
        if !entries.is_empty() {
            checks.push(map_drift_check("extraPrompts", &entries, &catalogs.general));
        }
    }

    checks
}

async fn check_model_drift() -> Vec<Check> {
    let catalogs = ModelCatalogs::from_state();
    let cfg = config::get_config();
    classify_model_drift(&catalogs, &cfg)
}

// --- Output ----------------------------------------------------------------

fn print_plain(checks: &[Check], counts: &Counts, exit_code: i32) {
    let mut out = String::from("copilot-api doctor\n\n");
    for check in checks {
        out.push_str(&format!(
            "[{:>4}] {}: {}\n",
            check.status.label(),
            check.name,
            check.message
        ));
    }
    out.push_str(&format!(
        "\nSummary: {} OK, {} WARN, {} FAIL (exit {exit_code})",
        counts.ok, counts.warn, counts.fail
    ));
    println!("{out}");
}

fn print_json(report: &DoctorReport) {
    match serde_json::to_string_pretty(report) {
        Ok(s) => println!("{s}"),
        Err(e) => tracing::error!("Failed to serialize doctor report: {e}"),
    }
}

/// Run the full preflight and return the process exit code (0 when no FAIL, 1
/// otherwise). All check messages are secret-free.
pub async fn run_doctor(json: bool) -> i32 {
    // Load config up front so provider + drift checks see the merged runtime
    // config. A malformed config.json surfaces as a FAIL rather than aborting.
    let mut checks = Vec::new();
    if let Err(e) = config::merge_config_with_defaults() {
        checks.push(Check::new(
            "config.load",
            Status::Fail,
            format!("Failed to load config: {e}"),
        ));
    }

    if let Err(e) = ensure_paths().await {
        checks.push(Check::new(
            "paths",
            Status::Fail,
            format!("Failed to ensure app paths: {e}"),
        ));
    }

    checks.extend(check_auth().await);

    // Warm the Copilot catalog for the drift check. Failure is non-fatal here —
    // the Copilot-token check above already reports the underlying auth problem,
    // and the drift check degrades to the Codex-only catalog.
    if let Err(e) = crate::libs::utils::cache_models().await {
        tracing::debug!("doctor: cache_models failed: {e}");
    }

    checks.extend(check_providers().await);
    checks.extend(check_model_drift().await);

    let (counts, exit_code) = summarize(&checks);

    if json {
        let report = DoctorReport {
            checks,
            summary: counts,
            exit_code,
        };
        print_json(&report);
    } else {
        print_plain(&checks, &counts, exit_code);
    }

    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn check(name: &str, status: Status) -> Check {
        Check::new(name, status, "")
    }

    #[test]
    fn summarize_counts_and_exit_code() {
        let checks = vec![
            check("a", Status::Ok),
            check("b", Status::Ok),
            check("c", Status::Warn),
        ];
        let (counts, exit) = summarize(&checks);
        assert_eq!(
            counts,
            Counts {
                ok: 2,
                warn: 1,
                fail: 0
            }
        );
        // WARNs never fail the preflight.
        assert_eq!(exit, 0);
    }

    #[test]
    fn summarize_fails_when_any_fail() {
        let checks = vec![
            check("a", Status::Ok),
            check("b", Status::Fail),
            check("c", Status::Warn),
        ];
        let (counts, exit) = summarize(&checks);
        assert_eq!(
            counts,
            Counts {
                ok: 1,
                warn: 1,
                fail: 1
            }
        );
        assert_eq!(exit, 1);
    }

    #[test]
    fn summarize_empty_is_ok() {
        let (counts, exit) = summarize(&[]);
        assert_eq!(counts, Counts::default());
        assert_eq!(exit, 0);
    }

    fn catalogs(general: &[&str], codex: &[&str]) -> ModelCatalogs {
        ModelCatalogs {
            general: general.iter().map(|s| s.to_string()).collect(),
            codex: codex.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn status_for(checks: &[Check], name: &str) -> Option<Status> {
        checks.iter().find(|c| c.name == name).map(|c| c.status)
    }

    #[test]
    fn drift_classifies_scalar_and_map_fields() {
        let cats = catalogs(&["gpt-5-mini", "claude-opus-4-8"], &["gpt-5.5"]);

        let mut efforts = BTreeMap::new();
        efforts.insert("gpt-5-mini".to_string(), "low".to_string());
        efforts.insert("ghost-model".to_string(), "high".to_string());

        let mut mappings = BTreeMap::new();
        mappings.insert(
            "alias-key".to_string(),
            serde_json::Value::String("claude-opus-4-8".to_string()),
        );

        let cfg = AppConfig {
            small_model: Some("gpt-5-mini".to_string()),
            message_api_web_search_model: Some("does-not-exist".to_string()),
            image_chat_model: Some("gpt-5.5".to_string()),
            model_reasoning_efforts: Some(efforts),
            model_mappings: Some(mappings),
            ..Default::default()
        };

        let checks = classify_model_drift(&cats, &cfg);

        // Scalar: known -> OK, unknown -> WARN, codex-scoped known -> OK.
        assert_eq!(status_for(&checks, "config.smallModel"), Some(Status::Ok));
        assert_eq!(
            status_for(&checks, "config.messageApiWebSearchModel"),
            Some(Status::Warn)
        );
        assert_eq!(
            status_for(&checks, "config.imageChatModel"),
            Some(Status::Ok)
        );

        // Map with one drifted key -> WARN; only the TARGET of a mapping is
        // checked, and its arbitrary alias KEY is ignored.
        assert_eq!(
            status_for(&checks, "config.modelReasoningEfforts"),
            Some(Status::Warn)
        );
        assert_eq!(
            status_for(&checks, "config.modelMappings"),
            Some(Status::Ok)
        );

        // No drift status is ever FAIL.
        assert!(checks.iter().all(|c| c.status != Status::Fail));
    }

    #[test]
    fn drift_mapping_checks_target_not_key() {
        // The alias KEY is gibberish but the TARGET is valid -> OK (keys are
        // arbitrary client aliases and must not be flagged).
        let cats = catalogs(&["claude-opus-4-8"], &[]);
        let mut mappings = BTreeMap::new();
        mappings.insert(
            "totally/arbitrary-alias".to_string(),
            serde_json::Value::String("claude-opus-4-8".to_string()),
        );
        let cfg = AppConfig {
            model_mappings: Some(mappings),
            ..Default::default()
        };
        let checks = classify_model_drift(&cats, &cfg);
        assert_eq!(
            status_for(&checks, "config.modelMappings"),
            Some(Status::Ok)
        );
    }

    #[test]
    fn drift_image_model_field_is_not_checked() {
        // imageModel has no catalog the doctor loads; it must not emit a check.
        let cats = catalogs(&[], &[]);
        let cfg = AppConfig {
            image_model: Some("gpt-image-2".to_string()),
            ..Default::default()
        };
        let checks = classify_model_drift(&cats, &cfg);
        assert!(checks.iter().all(|c| c.name != "config.imageModel"));
    }
}
