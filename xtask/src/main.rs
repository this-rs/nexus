//! `cargo xtask …` — see [`xtask::run`] for the commands.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = std::env::current_dir().expect("the current directory is readable");
    let mut out = String::new();
    let code = xtask::run(&args, &cwd, &mut out);
    print!("{out}");
    ExitCode::from(code)
}
