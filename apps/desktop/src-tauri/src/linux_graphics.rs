//! The one decision the desktop binary has to make before WebKit exists.
//!
//! WebKitGTK's DMA-BUF renderer needs an openable DRM render node. On a machine
//! that has none — a VM with a virtual GPU, a headless session, a user not in
//! `render` — it does not fall back: the UI process starts, the network process
//! starts, and the *web* process never does. The app is alive, paints nothing,
//! and prints only warnings a healthy run also prints. `WEBKIT_DISABLE_DMABUF_RENDERER=1`
//! starts it.
//!
//! Phase 2 discovered this and put the probe in `kd`, which was right for a
//! preview run through the dev launcher and wrong for an installed one: a
//! package's user never goes through `kd`, so an installed Kanna on a machine
//! with no render node would be a window that never appears. The decision
//! belongs to the binary.
//!
//! Timing is the whole reason this is a separate module called from `main`
//! rather than something inside `run()`. WebKitGTK reads the variable when its
//! process pool initializes, which happens the first time GTK/WebKit is touched
//! and on whichever thread touches it. `std::env::set_var` is unsound once other
//! threads exist, so this must run while the process is still single-threaded —
//! before Tauri builds anything.
//!
//! This is a workaround for a missing capability, not the intended steady
//! state: on a machine whose GPU WebKit can use, this sets nothing and the
//! accelerated path is untouched. An explicit value in the environment always
//! wins, so an operator can force either behaviour and a test can assert both.

/// The variable WebKitGTK reads. Also forwarded by `kd`'s tmux layer, because a
/// dev respawn that dropped it would start an app with no window.
pub const DISABLE_DMABUF_RENDERER: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";

/// What the process should do about the DMA-BUF renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmabufDecision {
    /// The environment already says; leave it exactly as it is.
    RespectExplicitSetting,
    /// A render node is openable — say nothing and let WebKit accelerate.
    LeaveAccelerated,
    /// No usable render node; set the variable or get no web process.
    Disable,
}

/// Decide from the environment and a capability probe.
///
/// A capability probe rather than a message match: the failure prints nothing
/// distinctive, so there is no error to read.
pub fn decide(explicit: Option<&str>, has_render_node: impl FnOnce() -> bool) -> DmabufDecision {
    // An empty value is not a decision — that is how a shell exports an unset
    // variable — so it falls through to the probe rather than being honoured.
    if explicit.is_some_and(|value| !value.is_empty()) {
        return DmabufDecision::RespectExplicitSetting;
    }
    if has_render_node() {
        DmabufDecision::LeaveAccelerated
    } else {
        DmabufDecision::Disable
    }
}

/// Can this process open a DRM render node for read and write?
///
/// Read *and* write: a node present but not writable by this user is exactly
/// the case that fails, and an existence check would pass it.
#[cfg(target_os = "linux")]
fn has_openable_render_node() -> bool {
    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("renderD"))
            && std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(entry.path())
                .is_ok()
    })
}

/// Apply the decision. Call this first in `main`, before any thread is spawned
/// and before Tauri or GTK is touched.
///
/// # Safety contract
///
/// Relies on the process still being single-threaded, which is why it is a
/// `main`-only entry point and not a helper anything else may call.
#[cfg(target_os = "linux")]
// `set_var` is safe in edition 2021 and `unsafe` in 2024. Writing the `unsafe`
// block keeps this correct across that change; the allow keeps today's edition
// from warning about it.
#[allow(unused_unsafe)]
pub fn configure_before_webkit_starts() {
    let explicit = std::env::var(DISABLE_DMABUF_RENDERER).ok();
    if decide(explicit.as_deref(), has_openable_render_node) == DmabufDecision::Disable {
        // SAFETY: documented above — `main` has spawned nothing yet.
        unsafe { std::env::set_var(DISABLE_DMABUF_RENDERER, "1") };
    }
}

#[cfg(not(target_os = "linux"))]
pub fn configure_before_webkit_starts() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_setting_wins_in_both_directions() {
        assert_eq!(
            decide(Some("1"), || true),
            DmabufDecision::RespectExplicitSetting
        );
        assert_eq!(
            decide(Some("0"), || false),
            DmabufDecision::RespectExplicitSetting
        );
    }

    /// An exported-but-empty variable is how a shell spells "unset"; honouring
    /// it would leave a render-node-less machine with no window.
    #[test]
    fn an_empty_value_is_not_an_explicit_setting() {
        assert_eq!(decide(Some(""), || false), DmabufDecision::Disable);
        assert_eq!(decide(Some(""), || true), DmabufDecision::LeaveAccelerated);
    }

    /// The rule that keeps this from being a blanket downgrade: a machine whose
    /// GPU WebKit can use is left alone, so graphics evidence collected there
    /// still describes the accelerated path.
    #[test]
    fn a_usable_render_node_is_left_accelerated() {
        assert_eq!(decide(None, || true), DmabufDecision::LeaveAccelerated);
        assert_eq!(decide(None, || false), DmabufDecision::Disable);
    }
}
