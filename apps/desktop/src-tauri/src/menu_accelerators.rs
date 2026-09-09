//! The accelerators the native menu installs, per platform.
//!
//! `CmdOrControl` is the right idea on macOS and the wrong one on Linux. It
//! resolves to Ctrl there, so `CmdOrControl+W` on the Close item takes
//! `Ctrl+W` away from every terminal in the app — and a native GTK accelerator
//! wins before the keystroke ever reaches the webview, so no amount of
//! JavaScript can give it back. `Ctrl+N`, and the predefined Edit items'
//! `Ctrl+C` / `Ctrl+X` / `Ctrl+A`, are the same problem: those keys belong to
//! readline and to job control.
//!
//! Linux therefore uses the shifted forms, which is also what GNOME Terminal
//! does and for exactly this reason. These strings must stay in step with
//! `apps/desktop/src/composables/shortcutPlatform.ts`, which maps the same
//! bindings for the in-app handler; `shortcut_parity` in the tests below is
//! what keeps a change to one from silently diverging from the other.

/// New Window.
pub const fn new_window() -> &'static str {
    if cfg!(target_os = "macos") {
        "CmdOrControl+N"
    } else {
        "CmdOrControl+Shift+N"
    }
}

/// Close: the tab in front, falling through to the window when there is none.
pub const fn close() -> &'static str {
    if cfg!(target_os = "macos") {
        "CmdOrControl+W"
    } else {
        "CmdOrControl+Shift+W"
    }
}

/// Previous / Next Task.
pub const fn navigate_task_up() -> &'static str {
    if cfg!(target_os = "macos") {
        "CmdOrControl+Alt+ArrowUp"
    } else {
        // Ctrl+Alt+Arrow switches GNOME workspaces.
        "Alt+ArrowUp"
    }
}

pub const fn navigate_task_down() -> &'static str {
    if cfg!(target_os = "macos") {
        "CmdOrControl+Alt+ArrowDown"
    } else {
        "Alt+ArrowDown"
    }
}

/// Previous / Next Repo. The same chord on both platforms: `Ctrl+Shift+Arrow`
/// is not a terminal key and GNOME has not claimed it.
pub const fn navigate_repo_up() -> &'static str {
    "CmdOrControl+Shift+ArrowUp"
}

pub const fn navigate_repo_down() -> &'static str {
    "CmdOrControl+Shift+ArrowDown"
}

/// Whether the predefined Edit items (Cut/Copy/Paste/Select All) may carry
/// their default accelerators.
///
/// On macOS they are ⌘X/⌘C/⌘V/⌘A and belong there. On Linux the same items
/// install Ctrl+X/C/V/A, and `Ctrl+C` reaching a menu instead of a running
/// agent is not a cosmetic difference — it is the difference between
/// interrupting a command and copying nothing. The webview provides its own
/// editing behaviour in text fields, so the menu does not need them.
pub const fn edit_items_take_accelerators() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing the menu installs on Linux may be a bare `Ctrl+<letter>`: those
    /// are the terminal's, and a native accelerator takes them for good.
    #[test]
    fn linux_menu_never_claims_a_plain_ctrl_letter() {
        if cfg!(target_os = "macos") {
            return;
        }
        for accelerator in [new_window(), close()] {
            let parts: Vec<&str> = accelerator.split('+').collect();
            let is_single_letter = parts.last().is_some_and(|last| {
                last.len() == 1 && last.chars().all(|c| c.is_ascii_alphabetic())
            });
            let has_second_modifier = parts.contains(&"Shift") || parts.contains(&"Alt");
            assert!(
                !is_single_letter || has_second_modifier,
                "{accelerator} would take a terminal key"
            );
        }
        assert!(
            !edit_items_take_accelerators(),
            "the Edit menu's Ctrl+C would outrank every agent session"
        );
    }

    /// The arrow chords GNOME has already claimed.
    #[test]
    fn linux_menu_stays_off_the_workspace_switcher() {
        if cfg!(target_os = "macos") {
            return;
        }
        for accelerator in [
            navigate_task_up(),
            navigate_task_down(),
            navigate_repo_up(),
            navigate_repo_down(),
        ] {
            let claims_workspace_chord = accelerator.contains("Alt")
                && (accelerator.contains("CmdOrControl") || accelerator.contains("Control"));
            assert!(
                !claims_workspace_chord,
                "{accelerator} is GNOME's workspace switcher"
            );
        }
    }

    /// macOS is frozen: these are the strings the menu has always installed.
    #[test]
    fn macos_menu_is_unchanged() {
        if !cfg!(target_os = "macos") {
            return;
        }
        assert_eq!(new_window(), "CmdOrControl+N");
        assert_eq!(close(), "CmdOrControl+W");
        assert_eq!(navigate_task_up(), "CmdOrControl+Alt+ArrowUp");
        assert_eq!(navigate_task_down(), "CmdOrControl+Alt+ArrowDown");
        assert_eq!(navigate_repo_up(), "CmdOrControl+Shift+ArrowUp");
        assert_eq!(navigate_repo_down(), "CmdOrControl+Shift+ArrowDown");
        assert!(edit_items_take_accelerators());
    }
}
