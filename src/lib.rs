use std::env;
use std::path::Path;
use std::process::ExitCode;

mod git;
mod herdr;
mod project;
mod ui;

pub fn run() -> ExitCode {
    match env::args().nth(1).as_deref() {
        Some("open-pane") => herdr::toggle_git_pane(),
        _ => run_app(),
    }
}

fn run_app() -> ExitCode {
    let cwd = env::current_dir().unwrap_or_else(|_| Path::new(".").to_owned());
    if let Err(error) = ui::run(&cwd) {
        eprintln!("Herdr Git: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
