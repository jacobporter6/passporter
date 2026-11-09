// Build identity, shared by `crates/bridge/build.rs` and `crates/web/build.rs`
// (each `include!`s this file and calls `stamp_build`), so the bridge and the
// PWA describe themselves the same way and can be compared.
//
// It sets, for the crate being built:
//
// - `PASSPORTER_GIT_SHA`: the short commit, with `-dirty` if the work tree
//   had changes, or `unknown` without git metadata (a tarball, a sandbox
//   that hides `.git`, git missing). A non-empty `PASSPORTER_GIT_SHA` in the
//   build environment wins, for builds outside a checkout.
// - `PASSPORTER_BUILT`: when the build script ran, RFC 3339 in UTC
//   (`2026-11-08T12:34:56Z`), and `PASSPORTER_BUILT_UNIX`, the same in unix
//   seconds. `SOURCE_DATE_EPOCH` fixes it, for reproducible builds.
//
// Never touches the network. It reruns when the commit changes (HEAD, its
// reflog, which moves with every commit and checkout in this work tree, the
// branch ref, packed refs), when one of the crate's own source dirs changes
// (when the crate recompiles anyway; `-dirty` follows), or when either
// override changes. It deliberately does not watch git's index, which other
// tools rewrite all the time, nor other crates: an edit elsewhere in the
// workspace shows up as `-dirty` on this crate's next rebuild.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Shown when the commit can't be known.
const UNKNOWN_SHA: &str = "unknown";

/// Emits the build identity, rerunning when any of `sources` (paths
/// relative to the crate) changes.
fn stamp_build(sources: &[&str]) {
    println!("cargo::rerun-if-env-changed=PASSPORTER_GIT_SHA");
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");
    for source in sources {
        println!("cargo::rerun-if-changed={source}");
    }
    let crate_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let sha = env_value("PASSPORTER_GIT_SHA").unwrap_or_else(|| git_sha(&crate_dir));
    let built = env_value("SOURCE_DATE_EPOCH")
        .and_then(|epoch| epoch.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs())
        });
    println!("cargo::rustc-env=PASSPORTER_GIT_SHA={sha}");
    println!("cargo::rustc-env=PASSPORTER_BUILT={}", rfc3339(built));
    println!("cargo::rustc-env=PASSPORTER_BUILT_UNIX={built}");
}

/// A non-empty environment variable.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

/// `git <args>` in `dir`, trimmed, if it succeeds. `--no-optional-locks`
/// keeps `git status` from rewriting the index (read-only sandboxes, and
/// other git users in the same tree).
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|text| text.trim().to_owned())
}

/// The short commit of the checkout `dir` is in, `-dirty` if it has
/// changes (untracked files included; ignored ones are not), and watches
/// the files that change with the commit.
fn git_sha(dir: &Path) -> String {
    let Some(sha) = git(dir, &["rev-parse", "--short=10", "HEAD"])
        .filter(|sha| !sha.is_empty() && sha.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return UNKNOWN_SHA.to_owned();
    };
    let mut watched = ["HEAD", "logs/HEAD", "packed-refs"].map(str::to_owned).to_vec();
    watched.extend(git(dir, &["symbolic-ref", "-q", "HEAD"]));
    for name in watched {
        // `--git-path` finds the right file in a linked worktree too.
        let path = git(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-path", &name],
        );
        // A missing file would rerun the script on every build.
        if let Some(path) = path.filter(|path| Path::new(path).exists()) {
            println!("cargo::rerun-if-changed={path}");
        }
    }
    match git(dir, &["status", "--porcelain"]) {
        Some(changes) if changes.is_empty() => sha,
        _ => format!("{sha}-dirty"),
    }
}

/// `unix` (seconds) as RFC 3339 in UTC.
fn rfc3339(unix: u64) -> String {
    let (days, secs) = (unix / 86_400, unix % 86_400);
    // Howard Hinnant's `civil_from_days`, for days since 1970-01-01.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}
