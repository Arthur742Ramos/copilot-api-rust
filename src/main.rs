// The crate's modules live in the `copilot_api` library (src/lib.rs) so that
// integration tests can link against them. The binary just drives the CLI.
use copilot_api::{debug, doctor, libs, mcp, server, services};

use clap::{Args, Parser, Subcommand};
use std::num::NonZeroUsize;

use crate::libs::config::merge_config_with_defaults;
use crate::libs::opencode::init_opencode_version;
use crate::libs::paths::{ensure_paths, PATHS};
use crate::libs::state;
use crate::libs::token::{log_user, setup_copilot_token, setup_github_token};
use crate::libs::utils::{
    cache_mac_machine_id, cache_models, cache_vscode_device_id, cache_vscode_session_id,
    cache_vscode_version,
};

/// A wrapper around GitHub Copilot API to make it OpenAI compatible.
#[derive(Parser, Debug)]
#[command(name = "copilot-api", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Path to the API home directory.
    #[arg(long = "api-home", global = true)]
    api_home: Option<String>,
    /// OAuth app identifier.
    #[arg(long = "oauth-app", global = true)]
    oauth_app: Option<String>,
    /// Enterprise URL for GitHub.
    #[arg(long = "enterprise-url", global = true)]
    enterprise_url: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the Copilot API server
    Start(StartArgs),
    /// Run authentication flows without running the server
    Auth(AuthArgs),
    /// Show current GitHub Copilot usage/quota information
    CheckUsage,
    /// Print environment, provider, and path diagnostics
    Debug {
        /// Emit diagnostics as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Run preflight checks on config, auth, and providers (exits non-zero on FAIL)
    Doctor {
        /// Emit the report as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Start the MCP tool_search bridge server over stdio
    Mcp,
    /// Update copilot-api to the latest released version
    Update(UpdateArgs),
}

#[derive(Args, Debug)]
struct StartArgs {
    /// Port to listen on
    #[arg(short = 'p', long, default_value = "4141", env = "COPILOT_API_PORT")]
    port: u16,
    /// Host/interface to bind to. Defaults to loopback (127.0.0.1) to limit the
    /// blast radius; pass 0.0.0.0 to expose the gateway on the LAN.
    #[arg(
        short = 'H',
        long,
        default_value = "127.0.0.1",
        env = "COPILOT_API_HOST"
    )]
    host: String,
    /// Enable verbose logging
    #[arg(short = 'v', long, default_value_t = false)]
    verbose: bool,
    /// Account type to use (individual, business, enterprise)
    #[arg(
        short = 'a',
        long = "account-type",
        default_value = "individual",
        env = "COPILOT_API_ACCOUNT_TYPE",
        value_parser = ["individual", "business", "enterprise"]
    )]
    account_type: String,
    /// Enable manual request approval
    #[arg(long, default_value_t = false, env = "COPILOT_API_MANUAL")]
    manual: bool,
    /// Rate limit in seconds between requests
    #[arg(short = 'r', long = "rate-limit", env = "COPILOT_API_RATE_LIMIT")]
    rate_limit: Option<u64>,
    /// Optional cap on concurrent upstream-facing proxy requests. Excess work
    /// gets 503 instead of queuing. Unset is unlimited; desktop recommendation: 64.
    #[arg(
        long = "max-concurrent-requests",
        env = "COPILOT_API_MAX_CONCURRENT_REQUESTS"
    )]
    max_concurrent_requests: Option<NonZeroUsize>,
    /// Wait instead of error when rate limit is hit
    #[arg(
        short = 'w',
        long = "wait",
        default_value_t = false,
        env = "COPILOT_API_WAIT"
    )]
    wait: bool,
    /// Provide GitHub token directly (generated via the `auth` subcommand)
    #[arg(short = 'g', long = "github-token", env = "COPILOT_API_GITHUB_TOKEN")]
    github_token: Option<String>,
    /// Generate a command to launch Claude Code with Copilot API config
    #[arg(short = 'c', long = "claude-code", default_value_t = false)]
    claude_code: bool,
    /// Show GitHub and Copilot tokens on fetch and refresh
    #[arg(long = "show-token", default_value_t = false)]
    show_token: bool,
    /// Initialize proxy from environment variables
    #[arg(long = "proxy-env", default_value_t = false)]
    proxy_env: bool,
    /// Allow binding to a non-loopback address when no API keys are configured.
    /// Without this flag, binding to a non-loopback interface with no keys is
    /// a fatal error — an unauthenticated proxy reachable from the network is
    /// almost certainly a misconfiguration.
    #[arg(
        long = "allow-remote-no-key",
        default_value_t = false,
        env = "COPILOT_API_ALLOW_REMOTE_NO_KEY"
    )]
    allow_remote_no_key: bool,
    /// Run in provider-only mode, bypassing GitHub/Copilot authentication and
    /// forwarding all requests directly to the named provider.
    #[arg(long = "provider-only", env = "COPILOT_API_PROVIDER_ONLY")]
    provider_only: Option<String>,
}

#[derive(Args, Debug)]
struct AuthArgs {
    /// Provider to log in with (copilot or codex)
    #[arg(long)]
    provider: Option<String>,
    /// Enable verbose logging
    #[arg(short = 'v', long, default_value_t = false)]
    verbose: bool,
    /// Show provider access token on auth
    #[arg(long = "show-token", default_value_t = false)]
    show_token: bool,
}

#[derive(Args, Debug)]
struct UpdateArgs {
    /// Only check whether a newer release exists; do not download or install.
    #[arg(long, default_value_t = false)]
    check: bool,
    /// Skip the confirmation prompt and update immediately if newer.
    #[arg(short = 'y', long, default_value_t = false)]
    yes: bool,
}

fn apply_global_env(cli: &Cli) {
    if let Some(v) = &cli.api_home {
        std::env::set_var("COPILOT_API_HOME", v);
    }
    if let Some(v) = &cli.oauth_app {
        std::env::set_var("COPILOT_API_OAUTH_APP", v);
    }
    if let Some(v) = &cli.enterprise_url {
        std::env::set_var("COPILOT_API_ENTERPRISE_URL", v);
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    apply_global_env(&cli);

    let verbose = matches!(&cli.command, Command::Start(a) if a.verbose)
        || matches!(&cli.command, Command::Auth(a) if a.verbose);
    // In MCP mode stdout is the JSON-RPC transport, so all logs must go to
    // stderr to avoid corrupting the protocol stream. Likewise, subcommands that
    // print a machine-readable JSON report to stdout (`doctor --json`,
    // `debug --json`) must keep logs off stdout so a stray config warning can't
    // interleave with and break the JSON.
    let mcp_mode = matches!(&cli.command, Command::Mcp);
    let json_report_to_stdout = matches!(
        &cli.command,
        Command::Doctor { json: true } | Command::Debug { json: true }
    );
    init_tracing(verbose, mcp_mode || json_report_to_stdout);

    let result = match cli.command {
        Command::Start(args) => run_server(args).await,
        Command::Auth(args) => run_auth(args).await,
        Command::CheckUsage => run_check_usage().await,
        Command::Debug { json } => {
            debug::run_debug(json).await;
            Ok(())
        }
        Command::Doctor { json } => {
            // The doctor command owns its own exit code (non-zero when any check
            // FAILs) so it can gate a CI / preflight step. Exit directly rather
            // than folding into the shared `result` path below.
            let code = doctor::run_doctor(json).await;
            std::process::exit(code);
        }
        Command::Mcp => mcp::run_mcp_server().await,
        Command::Update(args) => run_update(args).await,
    };

    if let Err(e) = result {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

fn init_tracing(verbose: bool, to_stderr: bool) {
    let default = if verbose { "debug" } else { "info" };
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    // Opt into structured JSON logs via COPILOT_API_LOG_FORMAT=json; default to
    // the existing human-readable format otherwise.
    let json = std::env::var("COPILOT_API_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    match (json, to_stderr) {
        (true, true) => tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .json()
            .with_writer(std::io::stderr)
            .init(),
        (true, false) => tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .json()
            .init(),
        (false, true) => tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init(),
        (false, false) => tracing_subscriber::fmt().with_env_filter(env_filter).init(),
    }
}

async fn run_server(options: StartArgs) -> anyhow::Result<()> {
    let max_concurrent_requests = resolve_max_concurrent_requests(options.max_concurrent_requests)?;
    raise_server_nofile_limit();
    crate::libs::http::set_proxy_from_env(options.proxy_env);
    if options.proxy_env {
        tracing::debug!("HTTP proxy configured from environment (per-URL)");
    }
    merge_config_with_defaults()?;
    init_opencode_version().await;

    state::with_state_mut(|s| {
        s.verbose = options.verbose;
        s.account_type = options.account_type.clone();
        s.manual_approve = options.manual;
        s.rate_limit_seconds = options.rate_limit;
        s.rate_limit_wait = options.wait;
        s.show_token = options.show_token;
        s.provider_only = options.provider_only.clone();
    });

    if options.verbose {
        tracing::info!("Verbose logging enabled");
    }
    if options.account_type != "individual" {
        tracing::info!("Using {} plan GitHub account", options.account_type);
    }

    ensure_paths().await?;
    cache_vscode_version().await;
    cache_mac_machine_id();
    cache_vscode_session_id();
    cache_vscode_device_id().await;

    if let Some(ref provider_name) = options.provider_only {
        // Provider-only mode: validate the named provider exists in config, then
        // skip GitHub auth, Copilot token exchange, and model caching.
        let providers = crate::libs::config::list_enabled_providers();
        if !providers.iter().any(|p| p == provider_name) {
            let known = providers.join(", ");
            anyhow::bail!(
                "Provider '{}' not found in config (enabled providers: {}). \
                 Check your config file and ensure the provider is configured and enabled.",
                provider_name,
                if known.is_empty() {
                    "<none>".to_string()
                } else {
                    known
                }
            );
        }
        tracing::info!(
            "Provider-only mode: forwarding to '{}' — GitHub/Copilot auth skipped.",
            provider_name
        );
    } else if let Some(token) = options.github_token {
        state::with_state_mut(|s| s.github_token = Some(token));
        tracing::info!("Using provided GitHub token");
        log_user().await?;
    } else {
        setup_github_token(false).await?;
    }

    if options.provider_only.is_none() {
        setup_copilot_token().await?;
        cache_models().await?;
    }

    if options.provider_only.is_none() {
        let model_list = state::with_state(|s| {
            s.models
                .as_ref()
                .map(|m| {
                    m.data
                        .iter()
                        .map(|model| format!("- {}", model.id))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        });
        let model_count =
            state::with_state(|s| s.models.as_ref().map(|m| m.data.len()).unwrap_or(0));
        // Full list at debug so it doesn't bury the ready banner; a count at info.
        tracing::info!("Loaded {model_count} models (run with -v to list them).");
        tracing::debug!("Available models: \n{model_list}");
    }

    // Resolve the bind host once into an IpAddr (single source of truth for both
    // the advertised URL and the listen SocketAddr below). Accepts an IP literal
    // (e.g. 127.0.0.1, ::1, 0.0.0.0) and defaults to loopback to limit exposure.
    // We parse to an IpAddr rather than DNS-resolving so the listen interface is
    // unambiguous; `localhost` is accepted as a friendly alias for 127.0.0.1
    // since it's a common invocation that would otherwise hard-fail.
    let host = options.host.trim();
    let ip: std::net::IpAddr = if host.eq_ignore_ascii_case("localhost") {
        std::net::Ipv4Addr::LOCALHOST.into()
    } else {
        host.parse().map_err(|_| {
            anyhow::anyhow!(
                "Invalid --host '{}': expected an IP address such as 127.0.0.1 or 0.0.0.0 (or 'localhost')",
                options.host
            )
        })?
    };

    // Derive the advertised URL from the actual bind IP. Unspecified binds
    // (0.0.0.0 / ::) aren't directly reachable, so advertise a concrete loopback
    // address of the matching family: 127.0.0.1 for IPv4 `0.0.0.0`, [::1] for
    // IPv6 `::` (advertising the name "localhost" could resolve to the wrong
    // family — e.g. 127.0.0.1 for a v6-only `::` socket on Windows). Concrete
    // binds advertise themselves (bracketing IPv6 literals).
    let server_host = if ip.is_unspecified() {
        if ip.is_ipv6() {
            "[::1]".to_string()
        } else {
            "127.0.0.1".to_string()
        }
    } else if ip.is_ipv6() {
        format!("[{ip}]")
    } else {
        ip.to_string()
    };
    let server_url = format!("http://{server_host}:{}", options.port);
    tracing::info!("Usage Viewer: {server_url}/usage-viewer?endpoint={server_url}/usage");

    if crate::libs::config::get_anthropic_api_key().is_some() {
        tracing::info!(
            "Token counting: using the Anthropic count_tokens API for exact counts on Claude models."
        );
    } else {
        tracing::info!(
            "Token counting: estimating with a tokenizer approximation. Set anthropicApiKey in config for exact Claude token counts."
        );
    }

    crate::libs::metrics::init_build_info();
    crate::libs::http::preregister_retry_metrics();
    crate::libs::premium_interactions::preregister_premium_interactions_metrics();
    let admission = crate::libs::admission::AdmissionController::new(max_concurrent_requests);
    if let Some(limit) = admission.limit() {
        tracing::info!(
            max_concurrent_requests = limit,
            "Upstream concurrency admission enabled"
        );
    } else {
        tracing::warn!(
            recommended_max_concurrent_requests =
                crate::libs::admission::RECOMMENDED_MAX_CONCURRENT_REQUESTS,
            "Upstream concurrency is unlimited; configure --max-concurrent-requests to enable load shedding"
        );
    }

    let app = server::build_router_with_admission(admission);
    crate::libs::premium_interactions::start_premium_interactions_refresh_loop();
    spawn_token_usage_retention();
    let addr = std::net::SocketAddr::new(ip, options.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    if !ip.is_loopback() {
        // Fail-closed: binding on a network interface without API keys means any
        // host on the LAN can use the proxy without any credential. Refuse unless
        // the operator explicitly opts in with --allow-remote-no-key.
        let api_keys = crate::libs::config::get_config()
            .auth
            .as_ref()
            .and_then(|a| a.api_keys.as_ref())
            .map(|keys| keys.len())
            .unwrap_or(0);
        let no_keys = api_keys == 0;
        if no_keys && !options.allow_remote_no_key {
            anyhow::bail!(
                "Binding to non-loopback host {ip} with no API keys configured is a \
                 security risk: any network client can use the proxy without credentials. \
                 Configure auth.apiKeys, or pass --allow-remote-no-key to suppress this check."
            );
        }
        if no_keys {
            tracing::warn!(
                "Binding to non-loopback host {ip} with no API keys (--allow-remote-no-key is set). \
                 The proxy is reachable from other machines without any credential."
            );
        } else {
            tracing::info!(
                "Binding to non-loopback host {ip}: API keys are configured — \
                 /token, /metrics and proxy routes require a key."
            );
        }
    }
    tracing::info!("Listening on {addr} ({server_url})");

    // The --claude-code clipboard/setup flow blocks on interactive model prompts,
    // so run it BEFORE the ready banner (and before axum::serve) — otherwise the
    // banner would announce "ready" while the prompt is still blocking and the
    // server is not yet accepting requests. It runs after the listener bound, so
    // the command it generates points at a server that actually bound.
    if options.claude_code {
        run_claude_code_setup(&server_url).await;
    }

    print_ready_banner(&server_url);
    // Emitted after the ready banner so this Windows-only advisory doesn't lead
    // the startup output and read as something being wrong on a single-user box.
    crate::libs::paths::warn_if_file_perms_unrestricted();

    // Run the server, but flush the token-usage WAL on the way out whether serve
    // returns Ok (graceful shutdown) or Err — otherwise a serve error would skip
    // the checkpoint exactly when something went wrong.
    let serve_result = axum::serve(listener, app)
        // Streaming responses consist of tiny token frames. Disable Nagle on
        // accepted client sockets so those frames are written immediately rather
        // than waiting to coalesce behind delayed ACKs.
        .tcp_nodelay(true)
        .with_graceful_shutdown(shutdown_signal())
        .await;

    if crate::libs::token_usage::is_token_usage_storage_enabled() {
        crate::libs::sqlite::with_usage_conn(|conn| {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        });
    }

    serve_result?;
    Ok(())
}

/// Default retention window (days) for `token_usage_events`. Comfortably beyond
/// the widest queryable window (~30 days) so no reachable row is ever pruned.
const DEFAULT_TOKEN_USAGE_RETENTION_DAYS: i64 = 45;

/// Spawn a background task that prunes old `token_usage_events` rows so the table
/// can't grow without bound on a long-lived proxy. Runs once at startup, then on
/// a ~12h interval. The window is read from `COPILOT_API_TOKEN_USAGE_RETENTION_DAYS`
/// (default 45); a value `<= 0` disables pruning. The blocking SQLite delete runs
/// on a blocking thread so it never sits on an async worker.
fn spawn_token_usage_retention() {
    if !crate::libs::token_usage::is_token_usage_storage_enabled() {
        return;
    }
    let retention_days = std::env::var("COPILOT_API_TOKEN_USAGE_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(DEFAULT_TOKEN_USAGE_RETENTION_DAYS);
    if retention_days <= 0 {
        tracing::debug!("Token-usage retention disabled (retention_days <= 0)");
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(12 * 60 * 60));
        // If a tick is delayed (long checkpoint, paused runtime), skip the missed
        // ticks instead of firing several prunes back-to-back.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let join = tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::libs::token_usage::prune_token_usage_events(retention_days) {
                    tracing::warn!("Token-usage retention prune failed: {e}");
                }
            })
            .await;
            if let Err(e) = join {
                // Surface a panic/cancellation in the blocking prune rather than
                // silently dropping it.
                tracing::warn!("Token-usage retention task did not complete: {e}");
            }
        }
    });
}

/// Compact, copy-pasteable startup banner so a new user immediately knows the
/// server is up and how to point a client at it. Emitted once after the listener
/// binds. Reuses the configured-API-keys check to describe the auth requirement.
fn print_ready_banner(server_url: &str) {
    let unauthenticated = crate::libs::request_auth::get_configured_api_keys().is_empty();
    // The token shown in the Anthropic example must actually be accepted: any
    // value (even none) works when no keys are configured, but a configured
    // deployment rejects "dummy", so show a placeholder there instead.
    let (auth_line, anthropic_token) = if unauthenticated {
        (
            "  Auth:    OPEN — no API key required. Set auth.apiKeys in config.json to require one.",
            "dummy",
        )
    } else {
        (
            "  Auth:    API key required — send one of your auth.apiKeys as x-api-key / Bearer.",
            "<your-api-key>",
        )
    };
    tracing::info!(
        "\n\
         ============================================================\n\
         copilot-api is ready.\n\
         ------------------------------------------------------------\n\
         OpenAI clients:    base_url = {server_url}/v1\n\
         Anthropic / Claude Code:\n\
           ANTHROPIC_BASE_URL={server_url}\n\
           ANTHROPIC_AUTH_TOKEN={anthropic_token}\n\
         {auth_line}\n\
         ============================================================"
    );
}

/// Resolves when the process receives a Ctrl-C, or (on unix) a SIGTERM, so the
/// server can drain in-flight requests before exiting.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutdown signal received, stopping server");
}

async fn run_update(args: UpdateArgs) -> anyhow::Result<()> {
    use crate::libs::update::{apply_update, check_for_update};

    let info = check_for_update().await?;

    tracing::info!("current version: {}", info.current);
    tracing::info!("latest release:  {} ({})", info.latest, info.tag);

    if !info.is_newer {
        tracing::info!("Already up to date.");
        return Ok(());
    }

    tracing::info!(
        "An update is available: {} -> {}",
        info.current,
        info.latest
    );

    if args.check {
        // --check is a dry run: report availability and exit without changing anything.
        return Ok(());
    }

    if !args.yes {
        print_prompt(&format!("Update to {} now? [y/N]", info.latest));
        let answer = read_line().await;
        let yes = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
        if !yes {
            tracing::info!("Update cancelled.");
            return Ok(());
        }
    }

    tracing::info!("Downloading {}...", info.asset_name);
    apply_update(&info).await?;
    tracing::info!(
        "Updated to {}. Restart copilot-api for the new version to take effect.",
        info.latest
    );
    Ok(())
}

fn resolve_max_concurrent_requests(
    configured: Option<NonZeroUsize>,
) -> anyhow::Result<Option<NonZeroUsize>> {
    if configured.is_some() {
        return Ok(configured);
    }

    let Some(raw) = std::env::var("COPILOT_API_MAX_IN_FLIGHT").ok() else {
        return Ok(None);
    };
    let value = raw.trim().parse::<usize>().map_err(|_| {
        anyhow::anyhow!(
            "Invalid COPILOT_API_MAX_IN_FLIGHT value '{raw}': expected a non-negative integer"
        )
    })?;
    let legacy = NonZeroUsize::new(value);
    tracing::warn!(
        "COPILOT_API_MAX_IN_FLIGHT is deprecated; use COPILOT_API_MAX_CONCURRENT_REQUESTS"
    );
    Ok(legacy)
}

fn raise_server_nofile_limit() {
    let target = crate::libs::resource_limits::TARGET_NOFILE_SOFT_LIMIT;
    match crate::libs::resource_limits::raise_nofile_soft_limit(target) {
        Ok(Some(limit)) if limit.raised() => tracing::info!(
            previous_soft_limit = limit.before,
            soft_limit = limit.after,
            hard_limit = ?limit.hard,
            "Raised process file-descriptor limit"
        ),
        Ok(Some(limit)) if limit.after < target => tracing::warn!(
            soft_limit = limit.after,
            hard_limit = ?limit.hard,
            target,
            "Process file-descriptor hard limit prevents the requested soft-limit increase"
        ),
        Ok(Some(limit)) => tracing::debug!(
            soft_limit = limit.after,
            hard_limit = ?limit.hard,
            "Process file-descriptor limit already sufficient"
        ),
        Ok(None) => {}
        Err(error) => tracing::warn!(
            %error,
            target,
            "Failed to raise process file-descriptor soft limit"
        ),
    }
}

async fn run_auth(options: AuthArgs) -> anyhow::Result<()> {
    state::with_state_mut(|s| s.show_token = options.show_token);
    ensure_paths().await?;

    let provider = options.provider.unwrap_or_else(|| "copilot".to_string());
    let provider = provider.trim();
    match provider {
        "copilot" => {
            setup_github_token(true).await?;
            tracing::info!(
                "GitHub token written to {}",
                PATHS.github_token_path.display()
            );
        }
        "codex" => {
            run_codex_login().await?;
        }
        other => {
            anyhow::bail!("Unknown provider '{other}'. Expected one of: copilot, codex");
        }
    }
    Ok(())
}

async fn run_codex_login() -> anyhow::Result<()> {
    use crate::libs::oauth::codex::{login_codex, CodexAuthInfo};
    use crate::libs::token::persist_codex_credentials;

    let credentials = login_codex(
        |info: CodexAuthInfo| {
            tracing::info!("Open the following URL to authenticate with Codex:");
            tracing::info!("{}", info.url);
            if let Some(instructions) = &info.instructions {
                tracing::info!("{instructions}");
            }
        },
        |message: String| async move {
            print_prompt(&message);
            read_line().await
        },
    )
    .await?;

    persist_codex_credentials(&credentials, true).await?;
    tracing::info!(
        "Codex provider config written to {} and credentials written to {}",
        PATHS.config_path.display(),
        PATHS.codex_credential_path.display()
    );
    Ok(())
}

fn print_prompt(message: &str) {
    use std::io::Write;
    print!("{message}: ");
    let _ = std::io::stdout().flush();
}

async fn read_line() -> String {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_string()
    })
    .await
    .unwrap_or_default()
}

fn claude_code_env_vars<'a>(
    server_url: &'a str,
    selected_model: &'a str,
    selected_small_model: &'a str,
) -> [(&'static str, &'a str); 12] {
    [
        ("ANTHROPIC_BASE_URL", server_url),
        ("ANTHROPIC_AUTH_TOKEN", "dummy"),
        ("ANTHROPIC_MODEL", selected_model),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", selected_model),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", selected_model),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", selected_small_model),
        ("DISABLE_NON_ESSENTIAL_MODEL_CALLS", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("CLAUDE_CODE_ATTRIBUTION_HEADER", "0"),
        ("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", "false"),
        ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "true"),
        ("CLAUDE_CODE_ENABLE_AWAY_SUMMARY", "0"),
    ]
}

/// Prints the setup tip, prompts for a primary + small model, builds the
/// env-setup command, and copies it to the clipboard.
async fn run_claude_code_setup(server_url: &str) {
    use crate::libs::shell::generate_env_script;

    tracing::info!(
        "\n💡 Tip: The --claude-code flag simply generates a clipboard command for launching Claude Code. \n\
         All models remain fully accessible without this flag, just configure the model ID directly in your settings.json file."
    );

    let model_ids: Vec<String> = state::with_state(|s| {
        s.models
            .as_ref()
            .map(|m| m.data.iter().map(|model| model.id.clone()).collect())
            .unwrap_or_default()
    });

    if model_ids.is_empty() {
        tracing::warn!("No models are loaded; skipping Claude Code command generation.");
        return;
    }

    let selected_model = select_model("Select a model to use with Claude Code", &model_ids).await;
    let selected_small_model =
        select_model("Select a small model to use with Claude Code", &model_ids).await;

    let env_vars = claude_code_env_vars(server_url, &selected_model, &selected_small_model);

    let command = generate_env_script(&env_vars, "claude");

    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(command.clone())) {
        Ok(()) => tracing::info!("Copied Claude Code command to clipboard!"),
        Err(_) => {
            tracing::warn!("Failed to copy to clipboard. Here is the Claude Code command:");
            tracing::info!("{command}");
        }
    }
}

/// Numbered-list replacement for consola's arrow-key `select` prompt. Defaults
/// to the first option on blank or out-of-range input.
async fn select_model(label: &str, options: &[String]) -> String {
    for (index, id) in options.iter().enumerate() {
        tracing::info!("  {}) {id}", index + 1);
    }
    print_prompt(&format!("{label} [1-{}]", options.len()));
    let answer = read_line().await;
    let selected = answer
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1 && *n <= options.len())
        .map(|n| n - 1)
        .unwrap_or(0);
    options[selected].clone()
}

async fn run_check_usage() -> anyhow::Result<()> {
    use crate::services::github::get_copilot_usage::QuotaDetail;

    ensure_paths().await?;
    setup_github_token(false).await?;

    let snapshot = state::snapshot();
    let usage = crate::services::github::get_copilot_usage::get_copilot_usage(&snapshot, None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch Copilot usage: {}", e.message))?;

    let snapshots = usage.quota_snapshots.unwrap_or_else(|| {
        crate::services::github::get_copilot_usage::QuotaSnapshots {
            chat: None,
            completions: None,
            premium_interactions: None,
            extra: Default::default(),
        }
    });

    let premium = snapshots
        .premium_interactions
        .unwrap_or_else(default_quota_detail);
    let premium_total = premium.entitlement;
    let premium_used = premium_total - premium.remaining;
    let premium_percent_used = if premium_total > 0.0 {
        premium_used / premium_total * 100.0
    } else {
        0.0
    };

    let summarize = |name: &str, snap: Option<&QuotaDetail>| match snap {
        None => format!("{name}: N/A"),
        Some(snap) => {
            let total = snap.entitlement;
            let used = total - snap.remaining;
            let percent_used = if total > 0.0 {
                used / total * 100.0
            } else {
                0.0
            };
            format!(
                "{name}: {}/{} used ({:.1}% used, {:.1}% remaining)",
                used, total, percent_used, snap.percent_remaining
            )
        }
    };

    let plan = usage.copilot_plan.clone().unwrap_or_default();
    tracing::info!(
        "Copilot Usage (plan: {plan})\nQuota resets: {}\n\nQuotas:\n  Premium: {}/{} used ({:.1}% used, {:.1}% remaining)\n  {}\n  {}",
        usage.quota_reset_date.clone().unwrap_or_default(),
        premium_used,
        premium_total,
        premium_percent_used,
        premium.percent_remaining,
        summarize("Chat", snapshots.chat.as_ref()),
        summarize("Completions", snapshots.completions.as_ref()),
    );
    Ok(())
}

fn default_quota_detail() -> crate::services::github::get_copilot_usage::QuotaDetail {
    crate::services::github::get_copilot_usage::QuotaDetail {
        entitlement: 0.0,
        overage_count: 0.0,
        overage_permitted: false,
        percent_remaining: 0.0,
        quota_id: String::new(),
        quota_remaining: 0.0,
        remaining: 0.0,
        unlimited: false,
        extra: Default::default(),
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn max_concurrent_requests_flag_accepts_positive_values() {
        let cli = Cli::try_parse_from(["copilot-api", "start", "--max-concurrent-requests", "7"])
            .expect("positive limit should parse");
        let Command::Start(args) = cli.command else {
            panic!("expected start command");
        };
        assert_eq!(args.max_concurrent_requests.map(NonZeroUsize::get), Some(7));
    }

    #[test]
    fn max_concurrent_requests_flag_rejects_zero() {
        assert!(
            Cli::try_parse_from(["copilot-api", "start", "--max-concurrent-requests", "0",])
                .is_err()
        );
    }

    #[test]
    fn claude_code_setup_pins_every_model_tier() {
        let env_vars = claude_code_env_vars(
            "http://127.0.0.1:4141",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
        );
        let env: std::collections::BTreeMap<_, _> = env_vars.into_iter().collect();

        assert_eq!(
            env.get("ANTHROPIC_MODEL").copied(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_OPUS_MODEL").copied(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_SONNET_MODEL").copied(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL").copied(),
            Some("claude-haiku-4-5")
        );
        assert!(!env.contains_key("CLAUDE_PLUGIN_ENABLE_QUESTION_RULES"));
    }
}
