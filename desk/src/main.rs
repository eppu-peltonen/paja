// No console window behind Desk's on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    paja_desk::run();
}
