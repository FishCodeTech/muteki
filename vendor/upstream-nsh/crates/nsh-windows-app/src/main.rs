//! The release runner with `nsh/base` (`nsh_platform_windows`'s defaults).

// A windowed program: no console opens with it.
#![windows_subsystem = "windows"]

fn main() -> std::process::ExitCode {
    nsh_windows_app::main()
}
