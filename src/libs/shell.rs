// Builds a copy-pasteable env-var + command string tailored to the user's shell.

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
    generate_env_script_for_shell(get_shell(), env_vars, command_to_run)
}

fn generate_env_script_for_shell(
    shell: Shell,
    env_vars: &[(&str, &str)],
    command_to_run: &str,
) -> String {
    let command_block = match shell {
        Shell::Powershell => env_vars
            .iter()
            .map(|(key, value)| {
                let value = quote_powershell(value);
                format!("$env:{key} = {value}")
            })
            .collect::<Vec<_>>()
            .join("; "),
        Shell::Cmd => env_vars
            .iter()
            .map(|(key, value)| format!("set \"{key}={value}\""))
            .collect::<Vec<_>>()
            .join(" && "),
        Shell::Fish => env_vars
            .iter()
            .map(|(key, value)| {
                let value = quote_fish(value);
                format!("set -gx {key} {value}")
            })
            .collect::<Vec<_>>()
            .join("; "),
        Shell::Bash | Shell::Zsh | Shell::Sh => {
            if env_vars.is_empty() {
                String::new()
            } else {
                let assignments = env_vars
                    .iter()
                    .map(|(key, value)| {
                        let value = quote_posix(value);
                        format!("{key}={value}")
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("export {assignments}")
            }
        }
    };

    if !command_block.is_empty() && !command_to_run.is_empty() {
        let separator = match shell {
            Shell::Powershell => "; ",
            Shell::Fish => "; and ",
            Shell::Bash | Shell::Zsh | Shell::Cmd | Shell::Sh => " && ",
        };
        return format!("{command_block}{separator}{command_to_run}");
    }

    if command_block.is_empty() {
        command_to_run.to_string()
    } else {
        command_block
    }
}

fn quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn quote_fish(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn quote_powershell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::{generate_env_script, generate_env_script_for_shell, Shell};

    #[test]
    fn posix_export_quotes_values() {
        let out = generate_env_script_for_shell(
            Shell::Bash,
            &[
                ("ANTHROPIC_BASE_URL", "http://localhost:4141"),
                ("LABEL", "can't stop"),
            ],
            "claude",
        );
        assert_eq!(
            out,
            "export ANTHROPIC_BASE_URL='http://localhost:4141' LABEL='can'\"'\"'t stop' && claude"
        );
    }

    #[test]
    fn fish_export_quotes_values() {
        let out = generate_env_script_for_shell(
            Shell::Fish,
            &[("MODEL", r"team\model's preview")],
            "claude",
        );
        assert_eq!(out, r"set -gx MODEL 'team\\model\'s preview'; and claude");
    }

    #[test]
    fn powershell_export_uses_verbatim_strings_and_legacy_separator() {
        let out = generate_env_script_for_shell(
            Shell::Powershell,
            &[
                ("ANTHROPIC_BASE_URL", "http://localhost:4141"),
                ("LABEL", "can't stop"),
            ],
            "claude",
        );
        assert_eq!(
            out,
            "$env:ANTHROPIC_BASE_URL = 'http://localhost:4141'; $env:LABEL = 'can''t stop'; claude"
        );
    }

    #[test]
    fn cmd_export_quotes_set_expressions() {
        let out = generate_env_script_for_shell(
            Shell::Cmd,
            &[
                ("ANTHROPIC_BASE_URL", "http://localhost:4141"),
                ("LABEL", "a & b"),
            ],
            "claude",
        );
        assert_eq!(
            out,
            "set \"ANTHROPIC_BASE_URL=http://localhost:4141\" && set \"LABEL=a & b\" && claude"
        );
    }

    #[test]
    fn empty_env_returns_bare_command() {
        let out = generate_env_script(&[], "claude");
        assert_eq!(out, "claude");
    }
}
