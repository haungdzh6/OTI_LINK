#![cfg(windows)]
#![windows_subsystem = "windows"]

#[path = "../oti_link_core_v10.rs"]
mod core;

fn main() {
    if let Err(e) = core::run(core::AppConfig::v10()) {
        let msg = e.to_string();
        if !msg.contains("already running") {
            core::show_error_dialog(&msg);
        }
    }
}
