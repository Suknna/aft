//! Shared Windows shell selection for foreground and background bash commands.
//!
//! Mirrors OpenCode's resolver: prefer modern PowerShell (`pwsh.exe`), fall
//! back to Windows PowerShell (`powershell.exe`), then to `cmd.exe`.
//!
//! Compiled on all platforms so the cross-platform retry-decision unit
//! tests in `commands::bash::try_spawn_with_fallback` can run on macOS/Linux
//! dev machines. Production callers (`commands::bash::spawn_shell_command`
//! and `bash_background::registry::detached_shell_command_for`) are
//! `#[cfg(windows)]`.

#![cfg_attr(not(windows), allow(dead_code))]

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowsShell {
    /// PowerShell 7+ (cross-platform). Supports `&&` pipeline operator.
    Pwsh,
    /// Windows PowerShell 5.1 (legacy, still default on most Windows desktops
    /// but **absent on Windows 11 IoT Enterprise LTSC SKUs** — issue #27).
    /// Does NOT support `&&` in pipelines (PS 7+ only feature).
    Powershell,
    /// `cmd.exe` — the universal fallback. Present on every Windows SKU.
    /// Supports `&&` and `||` natively. Lacks PowerShell's piping/cmdlets but
    /// handles bash-style chained shell invocations correctly.
    Cmd,
}

impl WindowsShell {
    /// Binary name to spawn. Caller relies on PATH lookup.
    pub(crate) fn binary(self) -> &'static str {
        match self {
            WindowsShell::Pwsh => "pwsh.exe",
            WindowsShell::Powershell => "powershell.exe",
            WindowsShell::Cmd => "cmd.exe",
        }
    }

    /// Argument vector to pass alongside the user's command string.
    /// PowerShell variants take `-Command <string>`; cmd takes `/D /C <string>`
    /// (`/D` disables AutoRun macros that could otherwise inject env-trust
    /// behavior into our isolated invocation).
    pub(crate) fn args<'a>(self, command: &'a str) -> Vec<&'a str> {
        match self {
            WindowsShell::Pwsh | WindowsShell::Powershell => vec![
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            WindowsShell::Cmd => vec!["/D", "/C", command],
        }
    }

    pub(crate) fn command(self, command: &str) -> Command {
        let mut cmd = Command::new(self.binary());
        cmd.args(self.args(command));
        cmd
    }

    /// Build a `Command` that runs the background wrapper script.
    ///
    /// For `Cmd`, this enables delayed environment-variable expansion via
    /// `/V:ON` so the wrapper's `!ERRORLEVEL!` captures the **real** exit
    /// code of the user command at run-time. Without this, `cmd.exe` parses
    /// the whole compound line at spawn time, expands `%ERRORLEVEL%` to its
    /// pre-execution value (typically 0 from cmd's startup), and the exit
    /// marker permanently records that stale value rather than the user
    /// command's actual exit code. PowerShell variants don't need this —
    /// PowerShell evaluates `$LASTEXITCODE` lazily at use-site by design.
    ///
    /// For foreground bash, callers should use [`Self::command`] instead;
    /// `/V:ON` would change the semantics of user commands containing `!`
    /// (which would otherwise be passed through literally to the user).
    pub(crate) fn bg_command(self, wrapper: &str) -> Command {
        let mut cmd = Command::new(self.binary());
        match self {
            WindowsShell::Pwsh | WindowsShell::Powershell => {
                cmd.args(self.args(wrapper));
            }
            WindowsShell::Cmd => {
                // /V:ON enables delayed expansion (!VAR!) for this cmd
                // invocation only — does not pollute the user command,
                // which sees its own subshell semantics anyway.
                cmd.args(["/V:ON", "/D", "/C", wrapper]);
            }
        }
        cmd
    }

    /// Wrap a background command so shell termination writes an exit marker.
    /// The marker is written via temp-file + rename for PowerShell variants and
    /// via `move /Y` for cmd.exe, matching the Unix background wrapper contract.
    pub(crate) fn wrapper_script(self, command: &str, exit_path: &Path) -> String {
        match self {
            WindowsShell::Pwsh | WindowsShell::Powershell => {
                let exit_path = powershell_single_quote(&exit_path.display().to_string());
                let binary = powershell_single_quote(self.binary());
                let command = powershell_single_quote(command);
                format!(
                    concat!(
                        "$exitPath = {exit_path}; ",
                        "$tmpPath = \"$exitPath.tmp.$PID\"; ",
                        "$global:LASTEXITCODE = $null; ",
                        "& {binary} -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command {command}; ",
                        "$success = $?; ",
                        "$nativeCode = $global:LASTEXITCODE; ",
                        "if ($null -ne $nativeCode) {{ $code = [int]$nativeCode }} ",
                        "elseif ($success) {{ $code = 0 }} ",
                        "else {{ $code = 1 }}; ",
                        "[System.IO.File]::WriteAllText($tmpPath, [string]$code); ",
                        "Move-Item -LiteralPath $tmpPath -Destination $exitPath -Force; ",
                        "exit $code"
                    ),
                    exit_path = exit_path,
                    binary = binary,
                    command = command
                )
            }
            WindowsShell::Cmd => {
                // CRITICAL: This wrapper MUST be invoked via `bg_command()`,
                // which prepends `/V:ON` to enable delayed expansion. Without
                // /V:ON, `cmd.exe` would parse the entire compound line at
                // spawn time and expand `%ERRORLEVEL%` to its pre-execution
                // value (typically 0 from cmd's startup), permanently
                // recording a stale exit code in the marker file regardless
                // of what the user command actually returned. With /V:ON,
                // `!ERRORLEVEL!` is evaluated each time it's referenced,
                // capturing the real run-time exit code after `{command}`
                // completes.
                //
                // `move /Y ... > nul` suppresses the "1 file(s) moved." line
                // that cmd would otherwise emit to the user's stdout.
                let tmp_path = format!("{}.tmp", exit_path.display());
                format!(
                    "{command} & echo !ERRORLEVEL! > {tmp} & move /Y {tmp} {exit} > nul",
                    command = command,
                    tmp = cmd_quote(&tmp_path),
                    exit = cmd_quote(&exit_path.display().to_string())
                )
            }
        }
    }
}

/// Resolve which Windows shell to use for `bash` invocations.
///
/// Cached after the first resolve to avoid repeated PATH probes — the user's
/// installed shells don't change mid-session, so a static cache is safe and
/// keeps bash dispatch fast.
///
/// **Note:** PATH probe via `which::which` can disagree with what
/// `Command::spawn` actually sees at runtime — antivirus / AppLocker rules,
/// PATH inheritance gaps in the spawning host, or sandbox flags can make
/// a binary "exist" to `which` but fail to spawn with NotFound. Foreground
/// bash uses [`shell_candidates`] + runtime retry to defend against this;
/// callers that take this single-result API are accepting the cached
/// outcome at face value.
pub(crate) fn resolve_windows_shell() -> WindowsShell {
    shell_candidates()
        .first()
        .copied()
        .unwrap_or(WindowsShell::Cmd)
}

/// All Windows shells that the PATH probe believes are reachable, returned
/// in priority order (pwsh > powershell > cmd). Always non-empty on Windows
/// because cmd.exe is always added as the floor.
///
/// Used by the foreground bash spawn site to retry with the next candidate
/// if the first one fails to spawn at runtime. Cached after the first
/// resolve.
pub(crate) fn shell_candidates() -> Vec<WindowsShell> {
    static CACHED: OnceLock<Vec<WindowsShell>> = OnceLock::new();
    CACHED
        .get_or_init(|| shell_candidates_with(|binary| which::which(binary).is_ok()))
        .clone()
}

pub(crate) fn shell_candidates_with<F>(exists: F) -> Vec<WindowsShell>
where
    F: Fn(&str) -> bool,
{
    let mut candidates = Vec::with_capacity(3);
    if exists("pwsh.exe") {
        log::info!("[aft] bash candidate: pwsh.exe (PowerShell 7+; supports && pipeline operator)");
        candidates.push(WindowsShell::Pwsh);
    }
    if exists("powershell.exe") {
        log::info!(
            "[aft] bash candidate: powershell.exe (Windows PowerShell 5.1; && in pipelines unsupported, will surface as parse error)"
        );
        candidates.push(WindowsShell::Powershell);
    }
    // cmd.exe is always added as the floor, regardless of PATH probe result.
    // It lives in a Windows search-path location that PATH inheritance issues,
    // ASR rules, and sandboxing generally cannot remove. Without this floor,
    // foreground bash retry would have nowhere to fall back to when both
    // PowerShell variants fail to spawn at runtime.
    candidates.push(WindowsShell::Cmd);
    if candidates.len() == 1 {
        log::warn!(
            "[aft] PowerShell (pwsh.exe / powershell.exe) is not reachable from \
             this aft process — using cmd.exe only. This can occur even \
             when PowerShell is installed if PATH inheritance is restricted, \
             antivirus / AppLocker / Defender ASR rules block PowerShell as a \
             child process, or you're on a stripped Windows SKU. Bash-style \
             commands using && and || still work; PowerShell-only cmdlets will \
             not. Details: https://github.com/cortexkit/aft/issues/27"
        );
    }
    candidates
}

/// Single-result variant of [`shell_candidates_with`] — kept for tests
/// and as a future hook for the background bash path (which currently
/// uses cached `resolve_windows_shell()` because the wrapper script
/// embeds the shell name and a retry would require regenerating the
/// script plus re-cloning stdout/stderr handles).
///
/// Returns the highest-priority reachable shell. cmd.exe is the floor.
#[allow(dead_code)] // Used by `#[cfg(windows)] #[test]` in bash_background::registry.
pub(crate) fn resolve_windows_shell_with<F>(exists: F) -> WindowsShell
where
    F: Fn(&str) -> bool,
{
    let mut candidates = shell_candidates_with(exists);
    // shell_candidates_with always pushes cmd.exe at minimum, so this is
    // guaranteed to be non-empty.
    candidates.remove(0)
}

fn powershell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// Used by `wrapper_script` for `WindowsShell::Cmd`; that wrapper is
// only invoked from `bash_background::registry::detached_shell_command_for`
// which is `#[cfg(windows)]`. The function compiles on all platforms so
// `wrapper_script` stays cross-platform-testable.
#[cfg_attr(not(windows), allow(dead_code))]
fn cmd_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
