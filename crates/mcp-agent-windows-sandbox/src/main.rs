#![allow(unsafe_op_in_unsafe_fn)]

#[cfg(any(windows, test))]
mod signature_der;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() {
    if let Err(error) = windows::run(std::env::args_os().skip(1)) {
        eprintln!("windows sandbox launch failed: {error}");
        std::process::exit(125);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("mcp-agent-windows-sandbox is a Windows-only helper");
    std::process::exit(125);
}
