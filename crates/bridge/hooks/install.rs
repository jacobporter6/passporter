//! `passporter-hook install`: the settings change that runs
//! `passporter-hook forward` on `PermissionRequest` (the only event the
//! bridge uses), shown as a diff (and applied only with `--write`).
//!
//! Idempotent: an existing `passporter-hook … forward` entry counts as
//! installed (its path is updated if it moved), so a second run changes
//! nothing. Everything else in the file is kept, in its order; the file is
//! rewritten as two-space JSON, the format Claude Code itself writes.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::json::Json;

/// The hook event installed, and its matcher (`*`: every tool).
const EVENT: &str = "PermissionRequest";
const MATCHER: &str = "*";

/// Seconds Claude Code waits for the hook; it finishes in milliseconds.
const TIMEOUT_SECS: u64 = 5;

/// The binary's name, which marks a command as this hook.
const HOOK_NAME: &str = "passporter-hook";

/// Largest settings file handled (1 MiB).
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

/// Lines of context around each change in the diff.
const CONTEXT: usize = 3;

/// What to install where.
#[derive(Debug)]
pub(crate) struct Options {
    /// The Claude Code settings file.
    pub(crate) settings: PathBuf,
    /// The `passporter-hook` binary the hook runs.
    pub(crate) hook: PathBuf,
    /// Apply the change instead of only showing it.
    pub(crate) write: bool,
}

/// Plans (and with `write`, applies) the change; returns the report.
pub(crate) fn run(options: &Options) -> Result<String, String> {
    let path = &options.settings;
    let before = read(path)?;
    let command = command(&options.hook)?;
    let planned = plan(before.as_deref(), &command)?;
    let before = before.unwrap_or_default();
    let mut report = String::new();
    let Some(after) = planned else {
        let _ = writeln!(
            report,
            "passporter-hook is already installed in {}; nothing to do.",
            path.display()
        );
        return Ok(report);
    };
    let _ = writeln!(report, "--- {}", path.display());
    let _ = writeln!(report, "+++ {} (with passporter-hook)", path.display());
    report.push_str(&diff(&before, &after));
    if options.write {
        write(path, &after).map_err(|error| format!("writing {}: {error}", path.display()))?;
        let _ = writeln!(report, "\nWrote {}.", path.display());
    } else {
        let _ = writeln!(
            report,
            "\nDry run: {} was not changed. To apply this change, run:\n\n  {} install --settings {} --hook {} --write",
            path.display(),
            quote(&options.hook.to_string_lossy()),
            quote(&path.to_string_lossy()),
            quote(&options.hook.to_string_lossy()),
        );
    }
    Ok(report)
}

/// The file's text, or `None` if it does not exist yet.
fn read(path: &Path) -> Result<Option<String>, String> {
    match fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_SETTINGS_BYTES => {
            return Err(format!(
                "{} is over {MAX_SETTINGS_BYTES} bytes",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    }
    fs::read_to_string(path)
        .map(Some)
        .map_err(|error| format!("reading {}: {error}", path.display()))
}

/// The hook command for the binary at `hook` (absolute, shell-quoted).
fn command(hook: &Path) -> Result<String, String> {
    if !hook.is_absolute() {
        return Err(format!(
            "--hook must be an absolute path: {}",
            hook.display()
        ));
    }
    if hook.file_name().and_then(|name| name.to_str()) != Some(HOOK_NAME) {
        return Err(format!("--hook must name the {HOOK_NAME} binary"));
    }
    let hook = hook
        .to_str()
        .ok_or_else(|| "--hook must be valid UTF-8".to_owned())?;
    Ok(format!("{} forward", quote(hook)))
}

/// `text` quoted for a POSIX shell when it needs it.
fn quote(text: &str) -> String {
    let safe = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_.-+@%=:,~".contains(c));
    if safe {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// Whether `command` runs some `passporter-hook forward`.
fn is_ours(command: &str) -> bool {
    let Some((program, rest)) = first_word(command) else {
        return false;
    };
    Path::new(&program)
        .file_name()
        .and_then(|name| name.to_str())
        == Some(HOOK_NAME)
        && rest.trim() == "forward"
}

/// The first word of a POSIX shell command, unquoted (single quotes,
/// double quotes with `\` escapes, and bare `\` escapes, as [`quote`] and
/// hand-written commands use them), and the rest of the command. `None`
/// for an unterminated quote or escape.
fn first_word(command: &str) -> Option<(String, &str)> {
    let command = command.trim_start();
    let mut word = String::new();
    let mut chars = command.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            c if c.is_whitespace() => return Some((word, &command[at..])),
            '\'' => loop {
                match chars.next()?.1 {
                    '\'' => break,
                    c => word.push(c),
                }
            },
            '"' => loop {
                match chars.next()?.1 {
                    '"' => break,
                    '\\' => word.push(chars.next()?.1),
                    c => word.push(c),
                }
            },
            '\\' => word.push(chars.next()?.1),
            c => word.push(c),
        }
    }
    Some((word, ""))
}

/// The settings text with the hook installed, or `None` if the settings
/// already have it. That is decided on the parsed JSON, not the text, so a
/// file in another format that has the hook is left alone (never
/// reformatted).
fn plan(before: Option<&str>, command: &str) -> Result<Option<String>, String> {
    let original = match before {
        Some(text) if !text.trim().is_empty() => Json::parse(text)
            .map_err(|error| format!("the settings file is not valid JSON: {error}"))?,
        _ => Json::Object(Vec::new()),
    };
    let mut settings = original.clone();
    let hooks = settings
        .entry("hooks", || Json::Object(Vec::new()))
        .ok_or("the settings file is not a JSON object")?;
    if !matches!(hooks, Json::Object(_)) {
        return Err("\"hooks\" is not an object".to_owned());
    }
    let Some(Json::Array(groups)) = hooks.entry(EVENT, || Json::Array(Vec::new())) else {
        return Err(format!("\"hooks.{EVENT}\" is not an array"));
    };
    if !update_ours(groups, command) {
        groups.push(group(command));
    }
    if settings == original {
        return Ok(None);
    }
    let mut after = settings.pretty();
    after.push('\n');
    Ok(Some(after))
}

/// Points every `passporter-hook forward` entry in `groups` at `command`;
/// whether there was one.
fn update_ours(groups: &mut [Json], command: &str) -> bool {
    let mut found = false;
    for group in groups {
        let Some(Json::Array(entries)) = group.member_mut("hooks") else {
            continue;
        };
        for entry in entries {
            let Some(value) = entry.member_mut("command") else {
                continue;
            };
            if value.as_str().is_some_and(|command| is_ours(&command)) {
                *value = Json::string(command);
                found = true;
            }
        }
    }
    found
}

/// A matcher group running `command`.
fn group(command: &str) -> Json {
    let hook = Json::object([
        ("type", Json::string("command")),
        ("command", Json::string(command)),
        ("timeout", Json::number(TIMEOUT_SECS)),
    ]);
    Json::object([
        ("matcher", Json::string(MATCHER)),
        ("hooks", Json::Array(vec![hook])),
    ])
}

/// `path` with symlinks followed to the file they name, even when that file
/// does not exist yet (a dangling dotfiles link), so writing it keeps the
/// links.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    const MAX_LINKS: usize = 40;
    let mut current = path.to_owned();
    for _ in 0..MAX_LINKS {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&current)?;
                current = match current.parent() {
                    Some(dir) if target.is_relative() => dir.join(target),
                    _ => target,
                };
            }
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(format!(
        "{} has too many levels of symlinks",
        path.display()
    )))
}

/// Writes `text` over the settings file atomically, keeping its mode (600
/// for a new file). A symlinked settings file (dotfiles) stays a symlink:
/// its target is what gets replaced. The temp file is created exclusively
/// (`O_EXCL`, so a planted file or symlink at its name fails the write)
/// with the final mode from the start.
fn write(path: &Path, text: &str) -> io::Result<()> {
    let target = resolve(path)?;
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("the directory {} does not exist", dir.display()),
        ));
    }
    let mode = fs::metadata(&target).map_or(0o600, |meta| meta.permissions().mode() & 0o777);
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".passporter-hook.{}.tmp", std::process::id()));
    let tmp = target.with_file_name(tmp_name);
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            // `mode` is filtered by the umask; set it exactly.
            file.set_permissions(fs::Permissions::from_mode(mode))?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, &target));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// One step of a line diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step<'a> {
    Same(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

/// A unified-style diff of `before` and `after`: changed lines with
/// [`CONTEXT`] lines around them, hunks separated by `@@`.
fn diff(before: &str, after: &str) -> String {
    let steps = steps(
        &before.lines().collect::<Vec<_>>(),
        &after.lines().collect::<Vec<_>>(),
    );
    let changed: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, step)| !matches!(step, Step::Same(_)))
        .map(|(i, _)| i)
        .collect();
    let shown = |i: usize| {
        changed
            .iter()
            .any(|&c| i + CONTEXT >= c && i <= c + CONTEXT)
    };
    let mut out = String::new();
    let mut last_shown = None;
    for (i, step) in steps.iter().enumerate() {
        if !shown(i) {
            continue;
        }
        if last_shown.is_none_or(|last| last + 1 != i) {
            out.push_str("@@\n");
        }
        last_shown = Some(i);
        let (sign, line) = match step {
            Step::Same(line) => (' ', line),
            Step::Removed(line) => ('-', line),
            Step::Added(line) => ('+', line),
        };
        let _ = writeln!(out, "{sign}{line}");
    }
    out
}

/// The longest-common-subsequence edit script from `a` to `b`, after
/// trimming their common prefix and suffix. The middle of a settings file
/// the installer touched is small; a huge one is shown as all changed.
fn steps<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Step<'a>> {
    const MAX_CELLS: usize = 4_000_000;
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (ma, mb) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut steps: Vec<Step<'a>> = a[..prefix].iter().map(|l| Step::Same(l)).collect();
    if (ma.len() + 1) * (mb.len() + 1) > MAX_CELLS {
        steps.extend(ma.iter().map(|l| Step::Removed(l)));
        steps.extend(mb.iter().map(|l| Step::Added(l)));
    } else {
        // lcs[i][j]: the LCS length of ma[i..] and mb[j..].
        let width = mb.len() + 1;
        let mut lcs = vec![0_u32; (ma.len() + 1) * width];
        for i in (0..ma.len()).rev() {
            for j in (0..mb.len()).rev() {
                lcs[i * width + j] = if ma[i] == mb[j] {
                    lcs[(i + 1) * width + j + 1] + 1
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < ma.len() || j < mb.len() {
            if i < ma.len() && j < mb.len() && ma[i] == mb[j] {
                steps.push(Step::Same(ma[i]));
                i += 1;
                j += 1;
            } else if j < mb.len()
                && (i == ma.len() || lcs[i * width + j + 1] >= lcs[(i + 1) * width + j])
            {
                steps.push(Step::Added(mb[j]));
                j += 1;
            } else {
                steps.push(Step::Removed(ma[i]));
                i += 1;
            }
        }
    }
    steps.extend(a[a.len() - suffix..].iter().map(|l| Step::Same(l)));
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOOK: &str = "/opt/passporter/bin/passporter-hook";
    const COMMAND: &str = "/opt/passporter/bin/passporter-hook forward";

    const EXISTING: &str = r#"{
  "model": "opus",
  "permissions": {
    "allow": [
      "Bash(cargo test:*)"
    ]
  },
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "notify-send done"
          }
        ]
      }
    ]
  },
  "env": {
    "B": "2",
    "A": "1"
  }
}
"#;

    fn options(settings: PathBuf, write: bool) -> Options {
        Options {
            settings,
            hook: PathBuf::from(HOOK),
            write,
        }
    }

    #[test]
    fn plan_adds_the_hook_and_keeps_everything_else() {
        let after = plan(Some(EXISTING), COMMAND)
            .expect("plans")
            .expect("changes");
        let value: serde_json::Value = serde_json::from_str(&after).expect("valid");
        let groups = value["hooks"]["PermissionRequest"]
            .as_array()
            .expect("groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], COMMAND);
        assert_eq!(groups[0]["hooks"][0]["timeout"], 5);
        assert!(
            value["hooks"].get("Notification").is_none(),
            "only what the bridge uses"
        );
        assert_eq!(
            value["hooks"]["Stop"][0]["hooks"][0]["command"],
            "notify-send done"
        );
        assert_eq!(value["hooks"]["PermissionRequest"][0]["matcher"], "*");
        assert_eq!(
            value["hooks"]["Stop"].as_array().map(Vec::len),
            Some(1),
            "left alone"
        );
        // Only additions: every original line survives, in order.
        let steps = steps(
            &EXISTING.lines().collect::<Vec<_>>(),
            &after.lines().collect::<Vec<_>>(),
        );
        assert!(!steps.iter().any(|step| matches!(step, Step::Removed(_))));
        assert!(after.find("\"B\"") < after.find("\"A\""), "key order kept");
    }

    #[test]
    fn plan_is_idempotent_and_follows_a_moved_binary() {
        let once = plan(Some(EXISTING), COMMAND)
            .expect("plans")
            .expect("changes");
        assert_eq!(plan(Some(&once), COMMAND).expect("plans"), None);
        // Another format with the hook already in: still installed.
        let reformatted =
            serde_json::to_string(&serde_json::from_str::<serde_json::Value>(&once).expect("json"))
                .expect("compact");
        assert_eq!(plan(Some(&reformatted), COMMAND).expect("plans"), None);
        let moved = plan(Some(&once), "/new/passporter-hook forward")
            .expect("plans")
            .expect("changes");
        assert_eq!(moved.matches("passporter-hook forward").count(), 1);
        assert!(!moved.contains(COMMAND));
    }

    #[test]
    fn plan_starts_a_missing_or_empty_file() {
        for before in [None, Some(""), Some("{}")] {
            let after = plan(before, COMMAND).expect("plans").expect("changes");
            assert_eq!(after.matches(COMMAND).count(), 1, "{before:?}");
        }
    }

    #[test]
    fn plan_refuses_settings_it_does_not_understand() {
        for bad in [
            "[]",
            "{\"hooks\": []}",
            "{\"hooks\": {\"PermissionRequest\": {}}}",
            "{",
        ] {
            assert!(plan(Some(bad), COMMAND).is_err(), "{bad}");
        }
    }

    #[test]
    fn commands_are_recognized_and_quoted() {
        assert!(is_ours(COMMAND));
        assert!(is_ours("'/my dir/passporter-hook' forward"));
        assert!(is_ours("\"/my dir/passporter-hook\" forward"));
        assert!(is_ours("/my\\ dir/passporter-hook forward"));
        assert!(!is_ours("'/unterminated/passporter-hook forward"));
        for odd in [
            "/it's/passporter-hook",
            "/a \"b\"/passporter-hook",
            "/x\\y/passporter-hook",
        ] {
            let command = command(Path::new(odd)).expect("ok");
            assert!(is_ours(&command), "{command}");
            let (program, _) = first_word(&command).expect("parses");
            assert_eq!(program, odd);
        }
        assert!(!is_ours("/bin/passporter-hook-old forward"));
        assert!(!is_ours("/bin/passporter-hook forward; rm -rf /"));
        assert!(!is_ours("notify-send done"));
        assert_eq!(
            command(Path::new("/my dir/passporter-hook")).expect("ok"),
            "'/my dir/passporter-hook' forward"
        );
        assert!(command(Path::new("passporter-hook")).is_err(), "relative");
        assert!(command(Path::new("/bin/sh")).is_err(), "another binary");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn the_diff_shows_additions_in_context() {
        let diff = diff("a\nb\nc\nd\ne\nf\ng\nh\n", "a\nb\nc\nd\nX\ne\nf\ng\nh\n");
        assert_eq!(diff, "@@\n b\n c\n d\n+X\n e\n f\n g\n");
    }

    #[test]
    fn a_dry_run_on_a_temp_settings_file_leaves_it_alone() {
        let tmp = tempfile::tempdir().expect("tmp");
        let settings = tmp.path().join("settings.json");
        fs::write(&settings, EXISTING).expect("write");
        let report = run(&options(settings.clone(), false)).expect("runs");
        let added = report
            .lines()
            .filter(|line| line.starts_with('+') && line.contains(COMMAND))
            .count();
        assert_eq!(added, 1, "{report}");
        assert!(report.contains("Dry run"), "{report}");
        assert!(
            report.contains(&format!("--settings {} ", settings.display())),
            "{report}"
        );
        assert!(report.ends_with("--write\n"), "{report}");
        assert_eq!(fs::read_to_string(&settings).expect("read"), EXISTING);
    }

    #[test]
    fn write_keeps_a_symlinked_settings_file_a_symlink() {
        let tmp = tempfile::tempdir().expect("tmp");
        let real = tmp.path().join("dotfiles-settings.json");
        fs::write(&real, EXISTING).expect("write");
        let link = tmp.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).expect("link");
        run(&options(link.clone(), true)).expect("writes");
        assert!(
            fs::symlink_metadata(&link)
                .expect("meta")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&real)
                .expect("read")
                .matches(COMMAND)
                .count(),
            1
        );
        let leftovers = fs::read_dir(tmp.path()).expect("dir").count();
        assert_eq!(leftovers, 2, "no temp file left behind");
    }

    #[test]
    fn write_follows_a_dangling_symlink_and_explains_a_missing_directory() {
        let tmp = tempfile::tempdir().expect("tmp");
        let real = tmp.path().join("dotfiles").join("settings.json");
        fs::create_dir(real.parent().expect("dir")).expect("mkdir");
        let link = tmp.path().join("settings.json");
        std::os::unix::fs::symlink("dotfiles/settings.json", &link).expect("link");
        run(&options(link.clone(), true)).expect("writes");
        assert!(
            fs::symlink_metadata(&link)
                .expect("meta")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&real)
                .expect("read")
                .matches(COMMAND)
                .count(),
            1
        );

        let missing = tmp.path().join("nowhere").join("settings.json");
        let error = run(&options(missing, true)).expect_err("no directory");
        assert!(error.contains("does not exist"), "{error}");
    }

    #[test]
    fn write_refuses_a_planted_temp_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let settings = tmp.path().join("settings.json");
        fs::write(&settings, EXISTING).expect("write");
        let victim = tmp.path().join("victim");
        let planted = tmp.path().join(format!(
            ".settings.json.passporter-hook.{}.tmp",
            std::process::id()
        ));
        std::os::unix::fs::symlink(&victim, &planted).expect("plant");
        assert!(run(&options(settings.clone(), true)).is_err());
        assert!(!victim.exists(), "the symlink was not followed");
        assert_eq!(fs::read_to_string(&settings).expect("read"), EXISTING);
    }

    #[test]
    fn write_applies_once_and_keeps_the_mode() {
        let tmp = tempfile::tempdir().expect("tmp");
        let settings = tmp.path().join("settings.json");
        fs::write(&settings, EXISTING).expect("write");
        fs::set_permissions(&settings, fs::Permissions::from_mode(0o640)).expect("chmod");
        run(&options(settings.clone(), true)).expect("writes");
        let written = fs::read_to_string(&settings).expect("read");
        assert_eq!(written.matches(COMMAND).count(), 1);
        let mode = fs::metadata(&settings).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let again = run(&options(settings.clone(), true)).expect("runs");
        assert!(again.contains("already installed"), "{again}");
        assert_eq!(fs::read_to_string(&settings).expect("read"), written);
    }
}
