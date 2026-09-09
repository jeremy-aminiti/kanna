// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Must be the first thing the process does: it sets an environment
    // variable WebKitGTK reads at initialization, and `set_var` is only sound
    // while the process is single-threaded. See the module docs.
    kanna_desktop_lib::linux_graphics::configure_before_webkit_starts();
    kanna_desktop_lib::run()
}
