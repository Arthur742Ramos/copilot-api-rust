// Deferred subsystems (provider routing, Codex OAuth, MCP, usage store) are
// translated and wired but not yet reachable from the core proxy spine, so
// their items read as dead code until those features are turned on.
#![allow(dead_code)]

// The crate's modules live in the `copilot_api` library (src/lib.rs) so that
// integration tests can link against them. The binary just drives the CLI.
use copilot_api::{debug, libs, mcp, server, services};

use clap::{Args, Parser, Subcommand};

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
    /// Start the MCP tool_search bridge server over stdio
    Mcp,
}

#[derive(Args, Debug)]
struct StartArgs {
    /// Port to listen on
    #[arg(short = 'p', long, default_value = "4141")]
    port: u16,
    /// Enable verbose logging
    #[arg(short = 'v', long, default_value_t = false)]
    verbose: bool,
    /// Account type to use (individual, business, enterprise)
    #[arg(short = 'a', long = "account-type", default_value = "individual")]
    account_type: String,
    /// Enable manual request approval
    #[arg(long, default_value_t = false)]
    manual: bool,
    /// Rate limit in seconds between requests
    #[arg(short = 'r', long = "rate-limit")]
    rate_limit: Option<u64>,
    /// Wait instead of error when rate limit is hit
    #[arg(short = 'w', long = "wait", default_value_t = false)]
    wait: bool,
    /// Provide GitHub token directly (generated via the `auth` subcommand)
    #[arg(short = 'g', long = "github-token")]
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
    // stderr to avoid corrupting the protocol stream.
    let mcp_mode = matches!(&cli.command, Command::Mcp);
    init_tracing(verbose, mcp_mode);

    let result = match cli.command {
        Command::Start(args) => run_server(args).await,
        Command::Auth(args) => run_auth(args).await,
        Command::CheckUsage => run_check_usage().await,
        Command::Debug { json } => {
            debug::run_debug(json).await;
            Ok(())
        }
        Command::Mcp => mcp::run_mcp_server().await,
    };

    if let Err(e) = result {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

fn init_tracing(verbose: bool, to_stderr: bool) {
    let default = if verbose { "debug" } else { "info" };
    let builder = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default)),
    );
    if to_stderr {
        builder.with_writer(std::io::stderr).init();
    } else {
        builder.init();
    }
}

async fn run_server(options: StartArgs) -> anyhow::Result<()> {
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

    if let Some(token) = options.github_token {
        state::with_state_mut(|s| s.github_token = Some(token));
        tracing::info!("Using provided GitHub token");
        log_user().await?;
    } else {
        setup_github_token(false).await?;
    }

    setup_copilot_token().await?;
    cache_models().await?;

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
    tracing::info!("Available models: \n{model_list}");

    let server_url = format!("http://localhost:{}", options.port);
    if options.claude_code {
        run_claude_code_setup(&server_url).await;
    }
    tracing::info!("Usage Viewer: {server_url}/usage-viewer?endpoint={server_url}/usage");

    let app = server::build_router();
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], options.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Listening on {server_url}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Flush the token-usage WAL on a clean shutdown.
    if crate::libs::token_usage::is_token_usage_storage_enabled() {
        if let Ok(conn) = crate::libs::sqlite::usage_db().lock() {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
    }

    Ok(())
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

/// Mirrors the `--claude-code` branch of start.ts: prints the tip, prompts for
/// a primary + small model, builds the env-setup command, and copies it to the
/// clipboard (falling back to printing it on clipboard failure).
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

    let env_vars: [(&str, &str); 12] = [
        ("ANTHROPIC_BASE_URL", server_url),
        ("ANTHROPIC_AUTH_TOKEN", "dummy"),
        ("ANTHROPIC_MODEL", &selected_model),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", &selected_model),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", &selected_small_model),
        ("DISABLE_NON_ESSENTIAL_MODEL_CALLS", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("CLAUDE_CODE_ATTRIBUTION_HEADER", "0"),
        ("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", "false"),
        ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "true"),
        ("CLAUDE_CODE_ENABLE_AWAY_SUMMARY", "0"),
        ("CLAUDE_PLUGIN_ENABLE_QUESTION_RULES", "true"),
    ];

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
