use std::collections::HashMap;
use std::process::Command;

use kanna_runtime_defaults::login_shell::login_shell;

/// How the desktop should launch a shell, resolved once by the shared policy
/// in `kanna-runtime-defaults`.
///
/// The frontend used to answer this itself, with a `/bin/zsh` literal in
/// `stores/sessions.ts`. That was a second source of truth for something the
/// server already decided, and on a machine with no zsh it was simply wrong.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellLaunch {
    /// Absolute path to the shell binary.
    pub executable: String,
    /// The shell's own name, e.g. `zsh`, `bash`, `sh`.
    pub name: String,
    /// How this shell spells "login": `--login`, or `-l` for POSIX `sh`.
    pub login_arg: String,
}

#[tauri::command]
pub fn shell_launch() -> ShellLaunch {
    let shell = login_shell();
    ShellLaunch {
        executable: shell.path().to_string(),
        name: shell.name().to_string(),
        login_arg: shell.login_arg().to_string(),
    }
}

/// Ensure the Kanna zsh init directory exists with proxy rc files.
///
/// Returns the path to the directory (suitable for ZDOTDIR), or `None` when
/// the resolved shell is not zsh. ZDOTDIR means nothing to bash or dash, and
/// these proxy files are zsh syntax: exporting the directory to a bash shell
/// tab would be inert at best. Linux resolves to bash on a stock image, so
/// this is the ordinary path there, not an edge case.
#[tauri::command]
pub fn ensure_term_init() -> Result<Option<String>, String> {
    if login_shell().name() != "zsh" {
        return Ok(None);
    }
    let dir = crate::daemon_data_dir().join("zsh");
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create zsh init dir: {e}"))?;

    let zshenv = r#"# Kanna terminal — proxy to user's .zshenv
_kanna_home="${KANNA_ORIG_ZDOTDIR:-$HOME}"
[[ -f "$_kanna_home/.zshenv" ]] && source "$_kanna_home/.zshenv"
"#;

    let zprofile = r#"# Kanna terminal — proxy to user's .zprofile
_kanna_home="${KANNA_ORIG_ZDOTDIR:-$HOME}"
[[ -f "$_kanna_home/.zprofile" ]] && source "$_kanna_home/.zprofile"
"#;

    let zshrc = r#"# Kanna terminal defaults — user's .zshrc runs after and can override
bindkey -e  # emacs keybindings (prevents vi-mode Escape toggling)

# Restore ZDOTDIR and source user's .zshrc
_kanna_home="${KANNA_ORIG_ZDOTDIR:-$HOME}"
ZDOTDIR="$_kanna_home"
[[ -f "$_kanna_home/.zshrc" ]] && source "$_kanna_home/.zshrc"
unset _kanna_home
"#;

    let zlogin = r#"# Kanna terminal — proxy to user's .zlogin
_kanna_home="${KANNA_ORIG_ZDOTDIR:-$HOME}"
[[ -f "$_kanna_home/.zlogin" ]] && source "$_kanna_home/.zlogin"
unset _kanna_home KANNA_ORIG_ZDOTDIR
"#;

    for (name, content) in [
        (".zshenv", zshenv),
        (".zprofile", zprofile),
        (".zshrc", zshrc),
        (".zlogin", zlogin),
    ] {
        std::fs::write(dir.join(name), content)
            .map_err(|e| format!("failed to write {name}: {e}"))?;
    }

    dir.to_str()
        .map(|s| Some(s.to_string()))
        .ok_or_else(|| "non-UTF-8 path".to_string())
}

#[tauri::command]
pub async fn run_script(
    script: String,
    cwd: String,
    env: HashMap<String, String>,
) -> Result<String, String> {
    run_script_sync(&script, &cwd, env)
}

fn run_script_sync(
    script: &str,
    cwd: &str,
    env: HashMap<String, String>,
) -> Result<String, String> {
    // The policy already reads $SHELL, and rejects the values that cannot run
    // a Kanna script -- a shell that is not there, or one whose rc semantics
    // this is not written for. macOS still resolves to `/bin/zsh` with `-l`,
    // which is what this call site has always used.
    let shell = login_shell();
    let mut command = Command::new(shell.path());
    crate::subprocess_env::apply_child_env(&mut command, env);
    let output = command
        // `-l` deliberately, not `login_arg()`: it is what this call site has
        // always passed on macOS, and all three policy shells accept it.
        .arg("-l")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run script: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        Err(format!(
            "script exited with status {}: {}{}",
            output.status, stderr, stdout
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::run_script_sync;
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::sync::Mutex;

    fn env_lock() -> &'static Mutex<()> {
        crate::test_env_lock()
    }

    unsafe fn set_env_var(key: &str, value: &str) {
        let key = CString::new(key).expect("env key should be valid");
        let value = CString::new(value).expect("env value should be valid");
        assert_eq!(libc::setenv(key.as_ptr(), value.as_ptr(), 1), 0);
    }

    unsafe fn unset_env_var(key: &str) {
        let key = CString::new(key).expect("env key should be valid");
        assert_eq!(libc::unsetenv(key.as_ptr()), 0);
    }

    #[test]
    fn run_script_does_not_inherit_kanna_control_plane_env() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        unsafe {
            set_env_var("KANNA_TMUX_SESSION", "leaked-session");
            set_env_var("KANNA_DB_NAME", "leaked.db");
            set_env_var("TAURI_WEBDRIVER_PORT", "4555");
        }

        let output = run_script_sync(
            "printf '%s|%s|%s' \"${KANNA_TMUX_SESSION:-}\" \"${KANNA_DB_NAME:-}\" \"${TAURI_WEBDRIVER_PORT:-}\"",
            "/",
            HashMap::new(),
        )
        .expect("script should succeed");

        unsafe {
            unset_env_var("KANNA_TMUX_SESSION");
            unset_env_var("KANNA_DB_NAME");
            unset_env_var("TAURI_WEBDRIVER_PORT");
        }

        assert_eq!(output, "||");
    }

    #[test]
    fn run_script_preserves_explicit_kanna_env_over_scrubbed_parent_values() {
        let _guard = env_lock().lock().expect("env lock should not be poisoned");
        unsafe {
            set_env_var("KANNA_WORKTREE", "0");
            set_env_var("KANNA_TMUX_SESSION", "leaked-session");
        }

        let output = run_script_sync(
            "printf '%s|%s' \"${KANNA_WORKTREE:-}\" \"${KANNA_TMUX_SESSION:-}\"",
            "/",
            HashMap::from([("KANNA_WORKTREE".to_string(), "1".to_string())]),
        )
        .expect("script should succeed");

        unsafe {
            unset_env_var("KANNA_WORKTREE");
            unset_env_var("KANNA_TMUX_SESSION");
        }

        assert_eq!(output, "1|");
    }
}
