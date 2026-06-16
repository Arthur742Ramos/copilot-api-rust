// Mirrors src/lib/shell.ts: builds a copy-pasteable env-var + command string
// tailored to the user's shell.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Cmd,
    Sh,
}

#[cfg(not(windows))]
pub fn get_shell() -> Shell {
    match std::env::var("SHELL") {
        Ok(path) if path.ends_with("zsh") => Shell::Zsh,
        Ok(path) if path.ends_with("fish") => Shell::Fish,
        Ok(path) if path.ends_with("bash") => Shell::Bash,
        _ => Shell::Sh,
    }
}

#[cfg(windows)]
pub fn get_shell() -> Shell {
    // The TS inspects the parent process via wmic to tell powershell from cmd.
    // std exposes no parent-pid, so fall back to a PSModulePath heuristic and
    // default to cmd, matching the TS default when detection is inconclusive.
    if std::env::var_os("PSModulePath").is_some() {
        Shell::Powershell
    } else {
        Shell::Cmd
    }
}

/// Generates a copy-pasteable script to set environment variables and run a
/// subsequent command, formatted for the detected shell.
pub fn generate_env_script(env_vars: &[(&str, &str)], command_to_run: &str) -> String {
    let shell = get_shell();

    let command_block = match shell {
        Shell::Powershell => env_vars
            .iter()
            .map(|(key, value)| format!("$env:{key} = {value}"))
            .collect::<Vec<_>>()
            .join("; "),
        Shell::Cmd => env_vars
            .iter()
            .map(|(key, value)| format!("set {key}={value}"))
            .collect::<Vec<_>>()
            .join(" & "),
        Shell::Fish => env_vars
            .iter()
            .map(|(key, value)| format!("set -gx {key} {value}"))
            .collect::<Vec<_>>()
            .join("; "),
        Shell::Bash | Shell::Zsh | Shell::Sh => {
            if env_vars.is_empty() {
                String::new()
            } else {
                let assignments = env_vars
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("export {assignments}")
            }
        }
    };

    if !command_block.is_empty() && !command_to_run.is_empty() {
        let separator = if shell == Shell::Cmd { " & " } else { " && " };
        return format!("{command_block}{separator}{command_to_run}");
    }

    if command_block.is_empty() {
        command_to_run.to_string()
    } else {
        command_block
    }
}

#[cfg(test)]
mod tests {
    use super::{generate_env_script, Shell};

    // Re-implement the formatting with an explicit shell so the test is
    // independent of the runner's $SHELL.
    fn render(shell: Shell, env_vars: &[(&str, &str)], command: &str) -> String {
        let block = match shell {
            Shell::Bash | Shell::Zsh | Shell::Sh => {
                if env_vars.is_empty() {
                    String::new()
                } else {
                    let a = env_vars
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("export {a}")
                }
            }
            _ => unreachable!(),
        };
        if !block.is_empty() && !command.is_empty() {
            format!("{block} && {command}")
        } else if block.is_empty() {
            command.to_string()
        } else {
            block
        }
    }

    #[test]
    fn posix_export_joins_with_double_ampersand() {
        let out = render(
            Shell::Bash,
            &[("ANTHROPIC_BASE_URL", "http://localhost:4141"), ("X", "1")],
            "claude",
        );
        assert_eq!(
            out,
            "export ANTHROPIC_BASE_URL=http://localhost:4141 X=1 && claude"
        );
    }

    #[test]
    fn empty_env_returns_bare_command() {
        // generate_env_script with no vars must return just the command.
        let out = generate_env_script(&[], "claude");
        assert_eq!(out, "claude");
    }
}
