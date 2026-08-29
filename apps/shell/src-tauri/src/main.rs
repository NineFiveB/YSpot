// Windows GUI subsystem in release so no console window flashes; keep the
// console in debug builds for env_logger output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    yspot_shell::run()
}
