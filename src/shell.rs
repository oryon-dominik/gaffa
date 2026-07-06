use std::sync::OnceLock;

/// How Procfile command lines are executed: `<program> <args...> <command-line>`.
///
/// gaffa never parses the command line itself — the shell does, so pipes,
/// `&&`, redirects, environment expansion, and PATH lookups (including `.cmd`
/// shims like npm on Windows) behave exactly as they would when typed into
/// that shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Cmd,
    PowerShell,
    Posix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: String,
    pub args: Vec<String>,
    kind: ShellKind,
}

impl Shell {
    /// Build a shell spec from a program name or path.
    ///
    /// Known shells get their command-string flag; anything else is assumed
    /// to be POSIX-compatible and gets `-c`.
    #[must_use]
    pub fn from_program(program: &str) -> Self {
        let stem = std::path::Path::new(program)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(program)
            .to_ascii_lowercase();

        let (kind, args) = match stem.as_str() {
            "cmd" => (ShellKind::Cmd, vec!["/C".to_string()]),
            "pwsh" | "powershell" => (
                ShellKind::PowerShell,
                vec!["-NoProfile".to_string(), "-Command".to_string()],
            ),
            _ => (ShellKind::Posix, vec!["-c".to_string()]),
        };

        Self {
            program: program.to_string(),
            args,
            kind,
        }
    }

    /// Prepare a command line for this shell so the script text arrives
    /// verbatim.
    ///
    /// PowerShell's `-Command` consumes the command line through argv
    /// tokenization, which strips unescaped double quotes — they must be
    /// MSVCRT-escaped (`\"`, with preceding backslashes doubled) to survive.
    /// `cmd /C` and POSIX `sh -c` receive the line verbatim already.
    #[must_use]
    pub fn prepare_command<'a>(&self, command: &'a str) -> std::borrow::Cow<'a, str> {
        if self.kind != ShellKind::PowerShell || !command.contains('"') {
            return std::borrow::Cow::Borrowed(command);
        }

        let mut out = String::with_capacity(command.len() + 8);
        let mut backslashes = 0usize;
        for ch in command.chars() {
            match ch {
                '\\' => {
                    backslashes += 1;
                    out.push('\\');
                }
                '"' => {
                    // Double the backslashes preceding the quote, then escape it.
                    out.extend(std::iter::repeat_n('\\', backslashes + 1));
                    out.push('"');
                    backslashes = 0;
                }
                _ => {
                    backslashes = 0;
                    out.push(ch);
                }
            }
        }
        std::borrow::Cow::Owned(out)
    }

    /// Resolve the shell to use: explicit choice > `GAFFA_SHELL` > platform default.
    #[must_use]
    pub fn resolve(explicit: Option<&str>) -> Self {
        if let Some(program) = explicit {
            return Self::from_program(program);
        }
        if let Ok(program) = std::env::var("GAFFA_SHELL")
            && !program.trim().is_empty()
        {
            return Self::from_program(program.trim());
        }
        Self::platform_default().clone()
    }

    /// Platform default: `pwsh` (fallback `cmd`) on Windows, `sh` on Unix.
    /// The PATH lookup runs once per process.
    pub fn platform_default() -> &'static Self {
        static DEFAULT: OnceLock<Shell> = OnceLock::new();
        DEFAULT.get_or_init(|| {
            #[cfg(target_os = "windows")]
            {
                if find_on_path("pwsh.exe") {
                    Self::from_program("pwsh")
                } else {
                    Self::from_program("cmd")
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                Self::from_program("sh")
            }
        })
    }
}

#[cfg(target_os = "windows")]
fn find_on_path(exe: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(exe).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_program_cmd() {
        let shell = Shell::from_program("cmd");
        assert_eq!(shell.program, "cmd");
        assert_eq!(shell.args, vec!["/C"]);
    }

    #[test]
    fn test_from_program_pwsh_variants() {
        for program in [
            "pwsh",
            "powershell",
            r"C:\Program Files\PowerShell\7\pwsh.exe",
        ] {
            let shell = Shell::from_program(program);
            assert_eq!(shell.args, vec!["-NoProfile", "-Command"]);
        }
    }

    #[test]
    fn test_from_program_posix_fallback() {
        for program in ["sh", "bash", "zsh", "/usr/bin/fish"] {
            let shell = Shell::from_program(program);
            assert_eq!(shell.args, vec!["-c"]);
        }
    }

    #[test]
    fn test_resolve_explicit_wins() {
        let shell = Shell::resolve(Some("cmd"));
        assert_eq!(shell.program, "cmd");
    }

    #[test]
    fn test_prepare_command_passthrough() {
        // No escaping for cmd/POSIX shells or quote-free commands.
        let sh = Shell::from_program("sh");
        assert_eq!(sh.prepare_command(r#"echo "hi""#), r#"echo "hi""#);
        let cmd = Shell::from_program("cmd");
        assert_eq!(cmd.prepare_command(r#"echo "hi""#), r#"echo "hi""#);
        let pwsh = Shell::from_program("pwsh");
        assert_eq!(pwsh.prepare_command("echo hi"), "echo hi");
    }

    #[test]
    fn test_prepare_command_escapes_quotes_for_powershell() {
        let pwsh = Shell::from_program("pwsh");
        assert_eq!(
            pwsh.prepare_command(r#"Write-Host ("count " + $_)"#),
            r#"Write-Host (\"count \" + $_)"#
        );
        // Backslashes before a quote are doubled (MSVCRT rules).
        assert_eq!(pwsh.prepare_command(r#"echo "a\""#), r#"echo \"a\\\""#);
    }

    #[test]
    fn test_platform_default_is_known_shell() {
        let shell = Shell::platform_default();
        assert!(!shell.program.is_empty());
        assert!(!shell.args.is_empty());
    }
}
