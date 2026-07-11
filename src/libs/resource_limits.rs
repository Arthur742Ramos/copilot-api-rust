//! Best-effort process resource-limit tuning.

/// Target soft file-descriptor limit for the server process. The operating
/// system's hard limit remains authoritative.
pub const TARGET_NOFILE_SOFT_LIMIT: u64 = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NofileLimit {
    pub before: u64,
    pub after: u64,
    pub hard: Option<u64>,
}

impl NofileLimit {
    pub fn raised(self) -> bool {
        self.after > self.before
    }
}

#[cfg(unix)]
pub fn raise_nofile_soft_limit(target: u64) -> std::io::Result<Option<NofileLimit>> {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    // SAFETY: `limits` points to writable storage for exactly one `rlimit`
    // value, and RLIMIT_NOFILE is valid on every Unix target supported by libc.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let before = limits.rlim_cur;
    let hard = (limits.rlim_max != libc::RLIM_INFINITY).then_some(limits.rlim_max);
    let after = desired_soft_limit(before, hard, target);

    if after > before {
        let updated = libc::rlimit {
            rlim_cur: after,
            rlim_max: limits.rlim_max,
        };
        // SAFETY: `updated` is initialized, preserves the existing hard limit,
        // and sets the soft limit no higher than that hard limit.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &updated) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(Some(NofileLimit {
        before,
        after,
        hard,
    }))
}

#[cfg(not(unix))]
pub fn raise_nofile_soft_limit(_target: u64) -> std::io::Result<Option<NofileLimit>> {
    Ok(None)
}

fn desired_soft_limit(current: u64, hard: Option<u64>, target: u64) -> u64 {
    let allowed = hard.map_or(target, |hard| target.min(hard));
    current.max(allowed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_limit_never_lowers_or_exceeds_hard_limit() {
        assert_eq!(desired_soft_limit(256, Some(8_192), 4_096), 4_096);
        assert_eq!(desired_soft_limit(256, Some(1_024), 4_096), 1_024);
        assert_eq!(desired_soft_limit(8_192, Some(8_192), 4_096), 8_192);
        assert_eq!(desired_soft_limit(256, None, 4_096), 4_096);
    }
}
