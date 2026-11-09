//! Repo maintenance tasks, run as `cargo xtask <task>`.
//!
//! Building in many git worktrees at once (under `.worktrees/`) leaves each
//! worktree's `target/` with incremental state that goes stale. `prune`
//! reclaims it so disk use tracks the work actually in flight.
//!
//! `scrub-corpus` derives the secret-scrubbed transcript corpus from raw
//! dumps of `~/.claude/projects` (docs/corpus.md).

// This is a CLI: printing is its interface.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod prune;
mod scrub;
mod worktrees;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "\
usage: cargo xtask <task>

tasks:
  prune [--dry-run] [--max-age-days N]
      Remove build output that no live work needs:
        - target/ of directories under .worktrees/ that git no longer tracks
        - incremental caches untouched for N days (default 3) in live worktrees
  scrub-corpus --raw <dir> --out <dir>
      Scrub secrets from the raw *.tar.zst dumps of ~/.claude/projects in <raw>
      into <out>/<dump>/projects/..., and print a redaction report (counts only).
  help
      Show this message.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.split_first() {
        Some((task, rest)) if task == "prune" => {
            let opts = parse_prune(rest)?;
            let root = repo_root()?;
            let report = prune::run(&root, &opts).map_err(|e| e.to_string())?;
            print!("{report}");
            if report.has_failures() {
                eprintln!("xtask: some removals failed (listed above)");
                std::process::exit(1);
            }
            Ok(())
        }
        Some((task, rest)) if task == "scrub-corpus" => {
            let opts = parse_scrub(rest)?;
            let report = scrub::run(&opts).map_err(|e| e.to_string())?;
            print!("{report}");
            Ok(())
        }
        Some((task, _)) if task == "help" || task == "--help" || task == "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        Some((task, _)) => Err(format!("unknown task `{task}`")),
        None => Err("no task given".to_owned()),
    }
}

fn parse_prune(args: &[String]) -> Result<prune::Options, String> {
    let mut opts = prune::Options::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dry-run" => opts.dry_run = true,
            "--max-age-days" => {
                let days: u64 = it
                    .next()
                    .ok_or("--max-age-days needs a value")?
                    .parse()
                    .map_err(|_| "--max-age-days must be a whole number of days")?;
                opts.max_age = days
                    .checked_mul(24 * 60 * 60)
                    .map(Duration::from_secs)
                    .ok_or("--max-age-days is too large")?;
            }
            other => return Err(format!("unknown prune option `{other}`")),
        }
    }
    Ok(opts)
}

fn parse_scrub(args: &[String]) -> Result<scrub::Options, String> {
    let (mut raw, mut out) = (None, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let slot = match arg.as_str() {
            "--raw" => &mut raw,
            "--out" => &mut out,
            other => return Err(format!("unknown scrub-corpus option `{other}`")),
        };
        let value = it
            .next()
            .ok_or_else(|| format!("{arg} needs a directory"))?;
        *slot = Some(PathBuf::from(value));
    }
    Ok(scrub::Options {
        raw: raw.ok_or("scrub-corpus needs --raw <dir>")?,
        out: out.ok_or("scrub-corpus needs --out <dir>")?,
    })
}

/// The main checkout's root, even when invoked from inside a worktree.
fn repo_root() -> Result<PathBuf, String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !out.status.success() {
        return Err("not inside a git repository".to_owned());
    }
    let raw = out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout);
    let common_dir = PathBuf::from(OsStr::from_bytes(raw));
    common_dir
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| "git common dir has no parent".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn prune_defaults() {
        let opts = parse_prune(&[]).expect("parses");
        assert!(!opts.dry_run);
        assert_eq!(opts.max_age, prune::Options::default().max_age);
    }

    #[test]
    fn prune_flags() {
        let opts = parse_prune(&args(&["--dry-run", "--max-age-days", "7"])).expect("parses");
        assert!(opts.dry_run);
        assert_eq!(opts.max_age, Duration::from_hours(7 * 24));
    }

    #[test]
    fn prune_rejects_bad_input() {
        assert!(parse_prune(&args(&["--max-age-days"])).is_err());
        assert!(parse_prune(&args(&["--max-age-days", "soon"])).is_err());
        assert!(parse_prune(&args(&["--max-age-days", &u64::MAX.to_string()])).is_err());
        assert!(parse_prune(&args(&["--force"])).is_err());
    }

    #[test]
    fn scrub_flags() {
        let opts = parse_scrub(&args(&["--out", "/o", "--raw", "/r"])).expect("parses");
        assert_eq!(
            opts,
            scrub::Options {
                raw: "/r".into(),
                out: "/o".into()
            }
        );
        assert!(parse_scrub(&args(&["--raw", "/r"])).is_err());
        assert!(parse_scrub(&args(&["--out", "/o"])).is_err());
        assert!(parse_scrub(&args(&["--raw"])).is_err());
        assert!(parse_scrub(&args(&["--raw", "/r", "--out", "/o", "--force"])).is_err());
    }

    #[test]
    fn unknown_task_is_an_error() {
        assert!(run(&args(&["deploy"])).is_err());
        assert!(run(&[]).is_err());
    }
}
