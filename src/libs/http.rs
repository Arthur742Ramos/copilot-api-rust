//! Shared `reqwest` HTTP client used for all upstream calls, plus the
//! `--proxy-env` opt-in gate for honoring proxy environment variables.

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Whether to honor proxy environment variables (HTTP_PROXY/HTTPS_PROXY/...).
/// The TS port only installs an env-based proxy dispatcher when `--proxy-env`
/// is passed (src/lib/proxy.ts initProxyFromEnv); without the flag, requests go
/// direct. reqwest would otherwise read those env vars unconditionally, so we
/// gate it here. Must be set before the first `client()` call.
static PROXY_FROM_ENV: AtomicBool = AtomicBool::new(false);

pub fn set_proxy_from_env(enabled: bool) {
    PROXY_FROM_ENV.store(enabled, Ordering::SeqCst);
}

/// Whether proxy-from-env is enabled. Exposed so the provider forwarding client
/// (which is built separately, with redirects disabled) can mirror the same
/// proxy gating as the shared `client()`.
pub fn proxy_from_env_enabled() -> bool {
    PROXY_FROM_ENV.load(Ordering::SeqCst)
}

/// Shared reqwest client. The TS code uses a global monkey-patched `fetch`
/// (electron-fetch / undici with system CA). Here we use a single reqwest
/// client configured with native roots and no global timeout (streaming).
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90));
    if !PROXY_FROM_ENV.load(Ordering::SeqCst) {
        builder = builder.no_proxy();
    }
    builder.build().expect("failed to build reqwest client")
});

pub fn client() -> &'static reqwest::Client {
    &CLIENT
}
