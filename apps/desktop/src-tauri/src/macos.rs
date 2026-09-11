/// Install a native macOS event monitor that intercepts fn+F (Globe+F) and
/// toggles fullscreen.  The fn/Globe modifier sets NSEventModifierFlagFunction
/// on the NSEvent, which JavaScript cannot detect — so we must handle it here,
/// before the event reaches WKWebView / xterm.js.
///
/// After toggling fullscreen we schedule a focus-restore: WKWebView loses
/// first-responder status during the exit animation, and calling
/// `element.focus()` via `evaluateJavaScript:` triggers `becomeFirstResponder`
/// on modern WebKit (Bug 143482 fix, 2015).
#[cfg(target_os = "macos")]
pub(crate) fn setup_fn_f_fullscreen(app: tauri::AppHandle) {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ffi::{c_char, CStr};
    use std::ptr::{self, NonNull};
    use tauri::Manager;

    let block = block2::RcBlock::new(move |event: NonNull<AnyObject>| -> *mut AnyObject {
        unsafe {
            let flags: usize = msg_send![event.as_ref(), modifierFlags];

            // fn/Globe (bit 23) pressed, without Cmd/Ctrl/Option
            let fn_only = (flags & (1 << 23)) != 0
                && (flags & (1 << 20)) == 0
                && (flags & (1 << 18)) == 0
                && (flags & (1 << 19)) == 0;

            if fn_only {
                let chars: Option<Retained<AnyObject>> = msg_send![event.as_ref(), characters];
                if let Some(chars) = chars {
                    let utf8: *const c_char = msg_send![&*chars, UTF8String];
                    if !utf8.is_null() {
                        if let Ok(s) = CStr::from_ptr(utf8).to_str() {
                            if s.eq_ignore_ascii_case("f") {
                                if let Some(ns_app_cls) = AnyClass::get(c"NSApplication") {
                                    let ns_app: Option<Retained<AnyObject>> =
                                        msg_send![ns_app_cls, sharedApplication];
                                    if let Some(ns_app) = ns_app {
                                        let win: Option<Retained<AnyObject>> =
                                            msg_send![&*ns_app, keyWindow];
                                        if let Some(win) = win {
                                            let _: () = msg_send![
                                                &*win,
                                                toggleFullScreen: ptr::null::<AnyObject>()
                                            ];
                                        }
                                    }
                                }
                                // Schedule focus restoration after the ~700ms animation.
                                // Webview::set_focus() calls wry's focus() which does
                                // [window makeFirstResponder:webview] — restoring
                                // keyboard event delivery to the WKWebView.
                                // NOTE: WebviewWindow::set_focus() only focuses the
                                // NSWindow; we need AsRef<Webview>::set_focus() to
                                // reach the WKWebView's makeFirstResponder.
                                let app_clone = app.clone();
                                std::thread::spawn(move || {
                                    std::thread::sleep(std::time::Duration::from_secs(1));
                                    if let Some(w) = app_clone.get_webview_window("main") {
                                        let wv: &tauri::Webview<_> = w.as_ref();
                                        let _ = wv.set_focus();
                                        let _ = wv.eval("window.__kannaRestoreFocus?.()");
                                    }
                                });
                                return ptr::null_mut(); // consume the event
                            }
                        }
                    }
                }
            }

            event.as_ptr() // pass through
        }
    });

    unsafe {
        let Some(ns_event) = AnyClass::get(c"NSEvent") else {
            eprintln!("[macos] NSEvent class not found, fn+F shortcut unavailable");
            return;
        };
        let mask: u64 = 1 << 10; // NSEventMaskKeyDown
        let monitor: Option<Retained<AnyObject>> = msg_send![
            ns_event,
            addLocalMonitorForEventsMatchingMask: mask,
            handler: &*block
        ];
        // Keep the monitor alive for the lifetime of the app
        if let Some(m) = monitor {
            std::mem::forget(m);
        }
    }
}

/// The env var an E2E harness sets to keep its app instances out of the
/// foreground. Nothing else sets it, so `kd dev up` and shipped builds keep the
/// regular, activating launch.
#[cfg(target_os = "macos")]
pub(crate) const NO_ACTIVATE_ENV: &str = "KANNA_E2E_NO_ACTIVATE";

/// Activation policy this process should adopt, or `None` to keep Tauri's default.
///
/// A Tauri app launches as a regular macOS app: it gets a Dock icon and, in
/// `applicationDidFinishLaunching`, tao calls `activateIgnoringOtherApps:` on it.
/// That is right for Kanna and wrong for the desktop E2E harness, which starts one
/// or two real app instances per run and took the owner's keyboard focus with each
/// one.
///
/// `Prohibited` is the policy that actually fixes it: tao applies the policy just
/// before that activate call, and a prohibited app cannot be activated, so the call
/// becomes a no-op. `Accessory` would only drop the Dock icon — an accessory app is
/// still activatable, so it would still steal focus at launch. The window is still
/// created and rendered, and WebDriver still drives it: `tauri-plugin-webdriver`
/// reaches the WKWebView through `evaluateJavaScript:` and snapshots it with
/// `takeSnapshotWithConfiguration:`, neither of which needs the app to be active.
#[cfg(target_os = "macos")]
pub(crate) fn requested_activation_policy(value: Option<&str>) -> Option<tauri::ActivationPolicy> {
    match value {
        Some("1") => Some(tauri::ActivationPolicy::Prohibited),
        _ => None,
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct E2eForegroundActivation {
    pub target_pid: u32,
    pub target_active_before: bool,
    pub target_active_after: bool,
    pub frontmost_pid_before: Option<i32>,
    pub frontmost_name_before: Option<String>,
    pub frontmost_pid_after: Option<i32>,
    pub frontmost_name_after: Option<String>,
}

#[cfg(target_os = "macos")]
unsafe fn frontmost_application() -> (Option<i32>, Option<String>) {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ffi::CStr;

    let Some(workspace_class) = AnyClass::get(c"NSWorkspace") else {
        return (None, None);
    };
    let workspace: Option<Retained<AnyObject>> = msg_send![workspace_class, sharedWorkspace];
    let Some(workspace) = workspace else {
        return (None, None);
    };
    let application: Option<Retained<AnyObject>> = msg_send![&*workspace, frontmostApplication];
    let Some(application) = application else {
        return (None, None);
    };
    let pid: i32 = msg_send![&*application, processIdentifier];
    let name: Option<Retained<AnyObject>> = msg_send![&*application, localizedName];
    let name = name.and_then(|name| {
        let utf8: *const std::ffi::c_char = msg_send![&*name, UTF8String];
        (!utf8.is_null()).then(|| CStr::from_ptr(utf8).to_string_lossy().into_owned())
    });
    (Some(pid), name)
}

/// Activates only this process for the foreground E2E fixture. This is not a
/// production focus policy: the command is refused unless the isolated test
/// target explicitly requested normal activation.
#[cfg(target_os = "macos")]
#[tauri::command]
pub(crate) fn e2e_activate_current_app(
    app: tauri::AppHandle,
) -> Result<E2eForegroundActivation, String> {
    if std::env::var(NO_ACTIVATE_ENV).ok().as_deref() != Some("0") {
        return Err("foreground E2E activation requires KANNA_E2E_NO_ACTIVATE=0".to_string());
    }

    let (send, receive) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        use objc2::msg_send;
        use objc2::rc::Retained;
        use objc2::runtime::{AnyClass, AnyObject};

        let result = unsafe {
            match AnyClass::get(c"NSApplication") {
                Some(application_class) => {
                    let application: Option<Retained<AnyObject>> =
                        msg_send![application_class, sharedApplication];
                    match application {
                        Some(application) => {
                            let (frontmost_pid_before, frontmost_name_before) =
                                frontmost_application();
                            let target_active_before: bool = msg_send![&*application, isActive];
                            let _: () = msg_send![&*application, activateIgnoringOtherApps: true];
                            let target_active_after: bool = msg_send![&*application, isActive];
                            let (frontmost_pid_after, frontmost_name_after) =
                                frontmost_application();
                            Ok(E2eForegroundActivation {
                                target_pid: std::process::id(),
                                target_active_before,
                                target_active_after,
                                frontmost_pid_before,
                                frontmost_name_before,
                                frontmost_pid_after,
                                frontmost_name_after,
                            })
                        }
                        None => Err("NSApplication shared instance unavailable".to_string()),
                    }
                }
                None => Err("NSApplication class unavailable".to_string()),
            }
        };
        let _ = send.send(result);
    })
    .map_err(|error| format!("could not schedule foreground E2E activation: {error}"))?;
    receive
        .recv()
        .map_err(|_| "foreground E2E activation did not return from the main thread".to_string())?
}

/// Foreground activation is intentionally a macOS-only E2E affordance. Keep a
/// command-shaped non-macOS implementation because the desktop command table
/// is compiled on every supported target; returning an explicit unsupported
/// result is safer than making the generated handler reference a missing
/// symbol.
#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub(crate) fn e2e_activate_current_app() -> Result<E2eForegroundActivation, String> {
    Err("foreground E2E activation is only supported on macOS".to_string())
}

/// Resolve the user's full PATH from their interactive login shell.
/// macOS apps launched from Finder/Spotlight inherit a minimal PATH
/// (/usr/bin:/bin:/usr/sbin:/sbin) that doesn't include tools like
/// claude, bun, or homebrew binaries. This runs the user's shell once
/// at startup to get the real PATH and sets it on our process so all
/// children (daemon, PTY sessions) inherit it.
#[cfg(target_os = "macos")]
pub(crate) fn fix_path_from_shell() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    match std::process::Command::new(&shell)
        .args(["-ilc", "printf '%s' \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout);
            if !path.is_empty() {
                eprintln!(
                    "[path] resolved shell PATH ({} entries)",
                    path.matches(':').count() + 1
                );
                std::env::set_var("PATH", path.as_ref());
            }
        }
        Ok(output) => {
            eprintln!(
                "[path] shell exited with {}, keeping default PATH",
                output.status
            );
        }
        Err(e) => {
            eprintln!("[path] failed to run {}: {}", shell, e);
        }
    }
}

#[cfg(unix)]
const TARGET_SOFT_NOFILE_LIMIT: libc::rlim_t = 4096;

#[cfg(unix)]
fn desired_child_nofile_soft_limit(current_soft: libc::rlim_t, hard: libc::rlim_t) -> libc::rlim_t {
    let target = if hard == libc::RLIM_INFINITY {
        TARGET_SOFT_NOFILE_LIMIT
    } else {
        std::cmp::min(TARGET_SOFT_NOFILE_LIMIT, hard)
    };
    std::cmp::max(current_soft, target)
}

#[cfg(unix)]
pub(crate) fn raise_child_nofile_limit() {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    let get_result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) };
    if get_result != 0 {
        return;
    }

    let mut limit = unsafe { limit.assume_init() };
    let desired_soft = desired_child_nofile_soft_limit(limit.rlim_cur, limit.rlim_max);
    if limit.rlim_cur >= desired_soft {
        return;
    }

    limit.rlim_cur = desired_soft;
    let _ = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::desired_child_nofile_soft_limit;

    #[test]
    fn child_nofile_limit_raises_low_gui_soft_limit_without_exceeding_hard_limit() {
        assert_eq!(desired_child_nofile_soft_limit(256, 8192), 4096);
        assert_eq!(desired_child_nofile_soft_limit(256, 1024), 1024);
        assert_eq!(desired_child_nofile_soft_limit(8192, 8192), 8192);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn activation_policy_is_prohibited_only_when_the_e2e_flag_is_set() {
        use super::requested_activation_policy;

        assert!(matches!(
            requested_activation_policy(Some("1")),
            Some(tauri::ActivationPolicy::Prohibited)
        ));
        // An unset, empty or explicitly disabled flag must leave a normal launch alone.
        assert!(requested_activation_policy(None).is_none());
        assert!(requested_activation_policy(Some("")).is_none());
        assert!(requested_activation_policy(Some("0")).is_none());
    }
}
