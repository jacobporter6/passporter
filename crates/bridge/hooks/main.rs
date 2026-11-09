//! `passporter-hook`: the Claude Code hook that tells the bridge what an
//! agent is asking (`docs/hooks.md`).
//!
//! - `passporter-hook forward` is the hook command. It reads one payload on
//!   stdin and writes it to the bridge's hook socket. It never writes to
//!   stdout (Claude Code would read that as a decision) and always exits 0,
//!   so a missing or slow bridge never gets in Claude's way.
//! - `passporter-hook install --settings <file>` prints the change to a
//!   Claude Code settings file that installs the hook, as a diff, and the
//!   command that applies it. Only `--write` changes the file. Running it
//!   again changes nothing.

mod forward;
mod install;
mod json;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage:
  passporter-hook forward
      Forward the Claude Code hook payload on stdin to the bridge.
  passporter-hook install --settings <settings.json> [--hook <path>] [--write]
      Show the settings change that installs this hook (a dry run), or
      apply it with --write. --hook defaults to this binary.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("forward") if args.len() == 1 => {
            forward::run();
            ExitCode::SUCCESS
        }
        Some("install") => {
            match install_args(&args[1..]).and_then(|options| install::run(&options)) {
                Ok(report) => {
                    let _ = io::stdout().lock().write_all(report.as_bytes());
                    ExitCode::SUCCESS
                }
                Err(error) => fail(&error),
            }
        }
        _ => fail(USAGE),
    }
}

fn fail(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr().lock(), "passporter-hook: {message}");
    ExitCode::FAILURE
}

/// Parses `install`'s flags.
fn install_args(args: &[String]) -> Result<install::Options, String> {
    let mut settings = None;
    let mut hook = None;
    let mut write = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--settings" => settings = args.next().map(PathBuf::from),
            "--hook" => hook = args.next().map(PathBuf::from),
            "--write" => write = true,
            _ => return Err(USAGE.to_owned()),
        }
    }
    let settings = settings.ok_or_else(|| USAGE.to_owned())?;
    let hook = match hook {
        Some(hook) => hook,
        None => std::env::current_exe()
            .map_err(|error| format!("locating this binary: {error}; pass --hook"))?,
    };
    Ok(install::Options {
        settings,
        hook,
        write,
    })
}
