//! Self-update for the `copilot-api` binary.
//!
//! Queries the GitHub Releases API for the latest published release, selects
//! the asset matching the running platform's target triple (the names produced
//! by `.github/workflows/release.yml`), downloads it, and atomically swaps the
//! currently-running executable via the `self-replace` crate. All HTTP goes
//! through the shared rustls [`crate::libs::http::client`].

use anyhow::Context;
use serde::Deserialize;

use crate::libs::api_config::get_github_api_base_url;
use crate::libs::http;

/// `owner/repo` for the GitHub Releases lookup. Kept in lockstep with the
/// `repository` field in Cargo.toml.
const REPO_SLUG: &str = "Arthur742Ramos/copilot-api-rust";

/// Outcome of a latest-release lookup, enough for the caller to print a summary,
/// decide whether to apply, and then download.
pub struct ReleaseInfo {
    /// The running binary's version (`CARGO_PKG_VERSION`).
    pub current: String,
    /// The latest release version, with any leading `v` stripped.
    pub latest: String,
    /// The raw release tag (e.g. `v1.13.0`), as GitHub reports it.
    pub tag: String,
    /// The platform asset selected for download.
    pub asset_name: String,
    /// Direct download URL for [`Self::asset_name`].
    pub download_url: String,
    /// Whether [`Self::latest`] is strictly newer than [`Self::current`].
    pub is_newer: bool,
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// A self-descriptive User-Agent. The GitHub REST API rejects requests without
/// one, so this must always be sent.
fn user_agent() -> String {
    format!("copilot-api/{}", env!("CARGO_PKG_VERSION"))
}

/// The release-asset filename for the platform this binary was built for. These
/// must match the `asset` names in `.github/workflows/release.yml` exactly.
/// Returns an error on any target the release workflow does not build.
fn current_asset_name() -> anyhow::Result<&'static str> {
    Ok(if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "copilot-api-x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "copilot-api-aarch64-apple-darwin"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "copilot-api-x86_64-pc-windows-msvc.exe"
    } else {
        anyhow::bail!(
            "self-update has no prebuilt binary for this platform ({}/{}). \
                 Download a binary manually from https://github.com/{REPO_SLUG}/releases \
                 or rebuild from source.",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    })
}

/// Parse a `MAJOR.MINOR.PATCH` version, tolerating a leading `v` and ignoring any
/// `-prerelease`/`+build` suffix. Returns `None` if the core triple is malformed.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Whether `latest_tag` represents a strictly newer version than `current`.
/// Falls back to a string inequality when either side is unparseable, so a
/// non-numeric tag still surfaces as "an update is available" rather than being
/// silently treated as up-to-date.
fn is_newer(latest_tag: &str, current: &str) -> bool {
    match (parse_version(latest_tag), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => latest_tag.trim_start_matches('v') != current,
    }
}

/// Query GitHub for the latest published release and resolve the asset for this
/// platform. Errors if no release exists, the API is unreachable, or the release
/// is missing an asset for this target triple.
pub async fn check_for_update() -> anyhow::Result<ReleaseInfo> {
    let asset_name = current_asset_name()?;
    let url = format!(
        "{}/repos/{REPO_SLUG}/releases/latest",
        get_github_api_base_url()
    );

    let response = http::client()
        .get(&url)
        .header(reqwest::header::USER_AGENT, user_agent())
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .with_context(|| format!("querying GitHub releases ({url})"))?;

    if !response.status().is_success() {
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "no published release found for {REPO_SLUG} (GitHub returned 404). \
                 Tag a release (e.g. `git tag v{} && git push origin v{}`) so an \
                 updatable binary exists.",
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_VERSION"),
            );
        }
        anyhow::bail!("GitHub releases API returned HTTP {status}");
    }

    let release: LatestRelease = response
        .json()
        .await
        .context("parsing the GitHub release response")?;

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} does not include an asset named '{asset_name}' for this platform",
                release.tag_name
            )
        })?;

    let current = env!("CARGO_PKG_VERSION").to_string();
    let is_newer = is_newer(&release.tag_name, &current);

    Ok(ReleaseInfo {
        current,
        latest: release.tag_name.trim_start_matches('v').to_string(),
        tag: release.tag_name.clone(),
        asset_name: asset_name.to_string(),
        download_url: asset.browser_download_url.clone(),
        is_newer,
    })
}

/// Download the resolved asset and atomically replace the running executable.
/// The download is read fully into memory, then staged and swapped on a blocking
/// thread (the filesystem work and `self_replace` are synchronous).
pub async fn apply_update(info: &ReleaseInfo) -> anyhow::Result<()> {
    let bytes = download(&info.download_url).await?;
    tokio::task::spawn_blocking(move || stage_and_replace(&bytes))
        .await
        .context("self-update task panicked")??;
    Ok(())
}

/// Fetch the asset body as raw bytes through the shared client (which follows the
/// CDN redirect on `browser_download_url`).
async fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    let response = http::client()
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent())
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;

    if !response.status().is_success() {
        anyhow::bail!(
            "downloading the release asset failed: HTTP {}",
            response.status()
        );
    }

    // Cap binary download at 256 MiB so a compromised CDN can't exhaust memory.
    const MAX_BINARY_DOWNLOAD_BYTES: usize = 256 * 1024 * 1024;
    let bytes = crate::libs::http::read_bytes_capped_with_max(response, MAX_BINARY_DOWNLOAD_BYTES)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(bytes.to_vec())
}

/// Write `bytes` to a temp file next to the current executable (same volume, so
/// `self-replace`'s internal rename is never cross-device), make it executable on
/// unix, then swap it in. The staged file is removed afterward because
/// `self_replace` copies rather than moves it.
fn stage_and_replace(bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;

    let exe = std::env::current_exe().context("locating the current executable")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("the current executable has no parent directory"))?;
    let tmp_path = dir.join(format!(".copilot-api-update-{}", uuid::Uuid::new_v4()));

    {
        let mut file = std::fs::File::create(&tmp_path)
            .with_context(|| format!("creating staging file {}", tmp_path.display()))?;
        file.write_all(bytes)
            .and_then(|_| file.flush())
            .with_context(|| format!("writing staging file {}", tmp_path.display()))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("setting permissions on {}", tmp_path.display()))?;
    }

    let result = self_replace::self_replace(&tmp_path)
        .context("replacing the running executable with the downloaded binary");
    // self_replace copies the source into place, so the staged file is leftover
    // regardless of success; remove it best-effort either way.
    let _ = std::fs::remove_file(&tmp_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_with_and_without_prefix() {
        assert_eq!(parse_version("1.12.5"), Some((1, 12, 5)));
        assert_eq!(parse_version("v1.13.0"), Some((1, 13, 0)));
        assert_eq!(parse_version("v2.0.0-rc.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn detects_newer_versions() {
        assert!(is_newer("v1.13.0", "1.12.5"));
        assert!(is_newer("v2.0.0", "1.12.5"));
        assert!(is_newer("v1.12.6", "1.12.5"));
        assert!(!is_newer("v1.12.5", "1.12.5"));
        assert!(!is_newer("v1.12.4", "1.12.5"));
        assert!(!is_newer("v1.11.9", "1.12.5"));
    }

    #[test]
    fn unparseable_tag_falls_back_to_string_diff() {
        assert!(is_newer("nightly", "1.12.5"));
        assert!(!is_newer("1.12.5", "1.12.5"));
    }
}
