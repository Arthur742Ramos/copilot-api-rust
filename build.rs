//! Build script that captures the current git SHA and a build timestamp and
//! exposes them to the crate as compile-time environment variables (`GIT_SHA`
//! and `BUILD_TIMESTAMP`). The `/version` endpoint reads these via `env!`.
//!
//! Both values fall back to `"unknown"` when unavailable (e.g. building from a
//! source tarball with no git, or without a working clock), so the build never
//! fails just because metadata could not be gathered.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let git_sha = git_sha().unwrap_or_else(|| "unknown".to_string());
    let build_timestamp = build_timestamp();

    println!("cargo:rustc-env=GIT_SHA={git_sha}");
    println!("cargo:rustc-env=BUILD_TIMESTAMP={build_timestamp}");

    // Re-run the build script when HEAD moves so the embedded SHA stays fresh.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}

/// Returns the short git commit SHA, or `None` if git is unavailable or this is
/// not a git checkout.
fn git_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Returns an RFC 3339-ish UTC build timestamp. Honors `SOURCE_DATE_EPOCH` for
/// reproducible builds; otherwise uses the current wall-clock time.
fn build_timestamp() -> String {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs())
        });

    match secs {
        Some(secs) => format_utc(secs),
        None => "unknown".to_string(),
    }
}

/// Formats a Unix timestamp (seconds) as `YYYY-MM-DDTHH:MM:SSZ` without pulling
/// in a date library at build time.
fn format_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Civil-from-days algorithm (Howard Hinnant), epoch 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}
