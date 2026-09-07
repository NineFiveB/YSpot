// Windows GUI subsystem in release so no console window flashes; keep the
// console in debug builds, where the logger mirrors every line to stderr.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    yspot_shell::run()
}
