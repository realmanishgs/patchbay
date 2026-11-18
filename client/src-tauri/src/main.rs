#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Desktop entry point. The application lives in the `patchbay_lib` library crate.

fn main() {
    patchbay_lib::run()
}
