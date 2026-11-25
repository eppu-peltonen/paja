//! Test helper for Windows, where a shell script can't be run as a program: copied to
//! `<dir>\<name>.exe`, it runs the script `<dir>\<name>` with the POSIX shell, passing its
//! arguments, input and output through. The smoke tests use it for their fake `gh` and
//! `claude`.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let exe = std::env::current_exe().expect("finding this program");
    let script = exe.with_extension("");
    let shell = paja::platform::posix_shell().expect("finding a POSIX shell");
    let status = Command::new(shell)
        .arg(&script)
        .args(std::env::args_os().skip(1))
        .status()
        .expect("running the script");
    // A crash's NTSTATUS (negative, or above 255) must still read as a failure.
    ExitCode::from(
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(1),
    )
}
