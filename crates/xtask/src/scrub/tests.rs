//! End-to-end scrub tests on synthetic dumps. No real transcript data.

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;

use ruzstd::encoding::{CompressionLevel, compress_to_vec};
use serde_json::json;

use super::*;

/// A fresh temp dir, removed on drop.
struct Scratch(
    PathBuf,
    #[expect(dead_code, reason = "held only to delete the directory on drop")] tempfile::TempDir,
);

impl Scratch {
    fn new(_name: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        Self(dir.path().to_owned(), dir)
    }
}

/// Secret-shaped strings built at runtime, so none sits in the source.
fn fake(prefix: &str, len: usize) -> String {
    let alphabet = b"Qx7Lm2Vb9Kp4Rz8Tw3Nc6Hy1";
    let body: String = (0..len)
        .map(|i| char::from(alphabet[(i * 5 + 1) % alphabet.len()]))
        .collect();
    format!("{prefix}{body}")
}

struct Secrets {
    github: String,
    bearer: String,
    api_key: String,
    password: String,
    anthropic: String,
}

impl Secrets {
    fn new() -> Self {
        Self {
            github: fake("ghp_", 36),
            bearer: fake("", 40),
            api_key: fake("k", 30),
            password: fake("pw", 14),
            anthropic: fake("sk-ant-api03-", 48),
        }
    }

    fn all(&self) -> [&str; 5] {
        [
            &self.github,
            &self.bearer,
            &self.api_key,
            &self.password,
            &self.anthropic,
        ]
    }
}

fn transcript(s: &Secrets) -> (String, String) {
    let clean = json!({"type": "user", "cwd": "/home/dev/git/widget", "message": {"role": "user",
        "content": "ssh dev@192.0.2.20 and check /home/dev/git/widget/src/lib.rs"}})
    .to_string();
    let lines = [
        json!({"type": "permission-mode", "permissionMode": "default"}).to_string(),
        clean.clone(),
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1",
            "name": "Bash", "input": {"command": format!("GITHUB_TOKEN={} gh pr list", s.github)}}]}})
        .to_string(),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1",
            "content": format!("> GET /v1 HTTP/1.1\n> Authorization: Bearer {}\n", s.bearer)}]}})
        .to_string(),
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t2",
            "name": "mcp__x__configure", "input": {"api_key": s.api_key, "region": "eu"}}]}})
        .to_string(),
        format!("not json at all: password={} ANTHROPIC_API_KEY={}", s.password, s.anthropic),
    ];
    (lines.join("\n") + "\n", clean)
}

/// Builds a `.tar.zst` with the given regular files plus a directory, a
/// symlink and an entry whose name tries to escape.
fn dump(path: &Path, files: &[(&str, &str)]) {
    let mut builder = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_size(0);
    dir.set_mode(0o755);
    builder
        .append_data(&mut dir, "projects/-w/", io::empty())
        .expect("dir entry");
    for (name, body) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o600);
        builder
            .append_data(&mut header, name, body.as_bytes())
            .expect("file entry");
    }
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    builder
        .append_link(&mut link, "projects/-w/link.jsonl", "/etc/passwd")
        .expect("symlink entry");

    // The builder refuses `..`, so write the name field directly.
    let evil = b"{\"type\":\"user\"}\n";
    let mut header = tar::Header::new_old();
    let name = b"../escape.jsonl";
    header.as_old_mut().name[..name.len()].copy_from_slice(name);
    header.set_size(evil.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    builder.append(&header, &evil[..]).expect("evil entry");

    let tar = builder.into_inner().expect("tar");
    fs::write(
        path,
        compress_to_vec(tar.as_slice(), CompressionLevel::Fastest),
    )
    .expect("dump");
}

#[test]
fn scrubs_a_dump_end_to_end() {
    let scratch = Scratch::new("e2e");
    let raw = scratch.0.join("raw");
    let out = scratch.0.join("out");
    fs::create_dir_all(&raw).expect("raw dir");
    let secrets = Secrets::new();
    let (body, clean) = transcript(&secrets);
    dump(
        &raw.join("laptop-2026-11-07.tar.zst"),
        &[
            ("projects/-w/s1.jsonl", &body),
            ("projects/-w/s1/subagents/agent-a1.jsonl", &body),
            ("projects/-w/s1/subagents/agent-a1.meta.json", "{}"),
            ("projects/-w/memory/MEMORY.md", "notes"),
        ],
    );
    fs::write(raw.join("README"), "ignored").expect("stray file");

    let report = run(&Options {
        raw: raw.clone(),
        out: out.clone(),
    })
    .expect("scrub");

    let root = out.join("laptop-2026-11-07/projects/-w");
    let main = fs::read_to_string(root.join("s1.jsonl")).expect("main transcript");
    let sub = fs::read_to_string(root.join("s1/subagents/agent-a1.jsonl")).expect("subagent");
    assert_eq!(main, sub);
    assert!(!root.join("s1/subagents/agent-a1.meta.json").exists());
    assert!(!root.join("link.jsonl").exists());
    assert!(!scratch.0.join("escape.jsonl").exists());
    assert!(!out.join("escape.jsonl").exists());

    let report_text = fs::read_to_string(out.join(REPORT_FILE)).expect("report");
    for secret in secrets.all() {
        assert!(!main.contains(secret), "leaked {secret}");
        assert!(!report_text.contains(secret), "report leaked {secret}");
    }

    let lines: Vec<&str> = main.lines().collect();
    assert_eq!(lines.len(), 6);
    assert_eq!(lines[1], clean, "clean lines are copied byte for byte");
    for line in &lines[..5] {
        serde_json::from_str::<Value>(line).expect("still valid JSON");
    }
    assert!(
        lines[2].contains("GITHUB_TOKEN=[REDACTED:github]"),
        "{}",
        lines[2]
    );
    assert!(
        lines[3].contains("Bearer [REDACTED:bearer]"),
        "{}",
        lines[3]
    );
    assert!(
        lines[4].contains(r#""api_key":"[REDACTED:secret-field]""#),
        "{}",
        lines[4]
    );
    assert!(lines[4].contains(r#""region":"eu""#), "{}", lines[4]);
    assert_eq!(
        lines[5],
        "not json at all: password=[REDACTED:password] ANTHROPIC_API_KEY=[REDACTED:anthropic]"
    );

    let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&root.join("s1.jsonl")), 0o600);
    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&out.join(REPORT_FILE)), 0o600);

    let mut counts = Counts::new();
    for (rule, n) in [
        ("github", 2),
        ("bearer", 2),
        ("secret-field", 2),
        ("password", 2),
        ("anthropic", 2),
    ] {
        counts.insert(rule, n);
    }
    assert_eq!(
        report,
        Report {
            archives: 1,
            files: 2,
            lines: 12,
            redacted_lines: 8,
            unparsed_lines: 2,
            skipped_entries: 3,
            unsafe_entries: 1,
            counts,
            ..Report::default()
        }
    );
    assert_eq!(report.to_string(), report_text);
    assert!(report_text.contains("github"), "{report_text}");
}

#[test]
fn refuses_to_write_inside_raw() {
    let scratch = Scratch::new("inside");
    let raw = scratch.0.join("raw");
    fs::create_dir_all(&raw).expect("raw dir");
    let link = scratch.0.join("link-to-raw");
    std::os::unix::fs::symlink(&raw, &link).expect("symlink");
    for out in [
        raw.join("scrubbed"),
        raw.join("../raw/x"),
        scratch.0.join("elsewhere/../raw/y"),
        link.join("z"),
    ] {
        let err = run(&Options {
            raw: raw.clone(),
            out: out.clone(),
        })
        .expect_err("out inside raw");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{}", out.display());
    }
    assert_eq!(
        fs::read_dir(&raw).expect("raw").count(),
        0,
        "nothing written"
    );
}

#[test]
fn tightens_existing_output_and_refuses_symlinks() {
    let scratch = Scratch::new("perms");
    let raw = scratch.0.join("raw");
    let out = scratch.0.join("out");
    fs::create_dir_all(&raw).expect("raw dir");
    fs::create_dir_all(out.join("dev/projects")).expect("out dirs");
    for dir in [&out, &out.join("dev"), &out.join("dev/projects")] {
        fs::set_permissions(dir, Permissions::from_mode(0o755)).expect("chmod");
    }
    fs::write(out.join(REPORT_FILE), "old").expect("old report");
    fs::set_permissions(out.join(REPORT_FILE), Permissions::from_mode(0o644)).expect("chmod");
    dump(&raw.join("dev.tar.zst"), &[("projects/-w/s.jsonl", "{}\n")]);
    // Not a safe stem: must be skipped, never joined onto --out.
    fs::write(raw.join("...tar.zst"), "x").expect("odd dump");

    let report = run(&Options {
        raw: raw.clone(),
        out: out.clone(),
    })
    .expect("scrub");
    assert_eq!(report.skipped_archives, 1);
    let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    for dir in [&out, &out.join("dev"), &out.join("dev/projects")] {
        assert_eq!(mode(dir), 0o700, "{}", dir.display());
    }
    assert_eq!(mode(&out.join(REPORT_FILE)), 0o600);

    // A symlink planted in the output tree is refused, not followed.
    let elsewhere = scratch.0.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("dir");
    fs::remove_dir_all(out.join("dev/projects")).expect("rm");
    std::os::unix::fs::symlink(&elsewhere, out.join("dev/projects")).expect("symlink");
    assert!(run(&Options { raw, out }).is_err());
    assert_eq!(fs::read_dir(&elsewhere).expect("dir").count(), 0);
}

#[test]
fn drops_oversized_lines() {
    let redactor = Redactor::new().expect("rules");
    let mut report = Report::default();
    let mut out = Vec::new();
    let input = format!("{{\"a\":1}}\n{}\n{{\"b\":2}}\n", "x".repeat(100));
    scrub_stream_capped(
        input.as_bytes(),
        &mut out,
        &redactor,
        &mut report,
        &mut propagate::Secrets::default(),
        50,
    )
    .expect("scrub");
    assert_eq!(out, b"{\"a\":1}\n{\"b\":2}\n");
    assert_eq!(report.oversized_lines, 1);
    assert_eq!(report.lines, 2);
}

#[test]
fn stems_must_be_plain_names() {
    for stem in ["laptop-2026-11-07", "desktop_claude.v2"] {
        assert!(is_safe_stem(stem), "{stem}");
    }
    for stem in ["", ".", "..", ".hidden", "a/b", "a b"] {
        assert!(!is_safe_stem(stem), "{stem:?}");
    }
}

#[test]
fn needs_at_least_one_dump() {
    let scratch = Scratch::new("empty");
    let raw = scratch.0.join("raw");
    fs::create_dir_all(&raw).expect("raw dir");
    let err = run(&Options {
        raw,
        out: scratch.0.join("out"),
    })
    .expect_err("no dumps");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[test]
fn only_plain_relative_paths_are_safe() {
    assert_eq!(
        safe_relative(Path::new("./projects/-w/s.jsonl")),
        Some(PathBuf::from("projects/-w/s.jsonl"))
    );
    for bad in [
        "../x.jsonl",
        "/etc/x.jsonl",
        "projects/../../x.jsonl",
        ".",
        "",
    ] {
        assert_eq!(safe_relative(Path::new(bad)), None, "{bad}");
    }
}

#[test]
fn keeps_a_missing_final_newline() {
    let redactor = Redactor::new().expect("rules");
    let mut report = Report::default();
    let mut out = Vec::new();
    scrub_stream(
        &b"{\"a\":1}\n{\"b\":2}"[..],
        &mut out,
        &redactor,
        &mut report,
        &mut propagate::Secrets::default(),
    )
    .expect("scrub");
    assert_eq!(out, b"{\"a\":1}\n{\"b\":2}");
    assert_eq!(report.lines, 2);
}

/// Runs both passes over `input` as one transcript file.
fn scrub_text(input: &str) -> (String, Report) {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("t.jsonl");
    let redactor = Redactor::new().expect("rules");
    let mut report = Report::default();
    scrub_file(input.as_bytes(), &target, &redactor, &mut report).expect("scrub");
    (fs::read_to_string(&target).expect("output"), report)
}

/// The review's probe: a `Read` result carries the file parsed (where the
/// `client_secret` key gives the value away) and line-numbered (where no
/// rule can tell), and the value is echoed in a later, unrelated field.
#[test]
fn a_secret_found_once_is_redacted_everywhere() {
    // Too short for the high-entropy rule: only the `client_secret` key
    // gives it away, yet it looks like a secret, so it propagates.
    let secret = ["Tr0ub4dor", "and", "3"].join("-");
    let file = format!("{{\n  \"client_id\": \"app\",\n  \"client_secret\": \"{secret}\"\n}}\n");
    let numbered = file
        .lines()
        .enumerate()
        .fold(String::new(), |mut acc, (i, l)| {
            let _ = writeln!(acc, "{:>6}\t{l}", i + 1);
            acc
        });
    let lines = [
        json!({"type": "assistant", "message": {"content": format!("I will check {secret} later")}})
            .to_string(),
        json!({"type": "user", "message": {"content": [{"type": "tool_result",
            "tool_use_id": "r1", "content": numbered}]},
            "toolUseResult": {"type": "text", "file": {"filePath": "/w/oauth.json", "content": file}}})
        .to_string(),
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "b1",
            "name": "Bash", "input": {"command": format!("echo {secret} | wc -c")}}]}})
        .to_string(),
    ];
    let (out, report) = scrub_text(&(lines.join("\n") + "\n"));
    assert!(!out.contains(&secret), "{out}");
    assert!(out.contains("client_id"), "{out}");
    for line in out.lines() {
        serde_json::from_str::<Value>(line).expect("still JSON");
    }
    assert!(
        report.counts.get("propagated").is_some_and(|&n| n >= 3),
        "{report}"
    );
}

/// A word that once sat under a secret key (a project name used as a
/// password) is not wiped from every path and branch in the file.
#[test]
fn dictionary_words_do_not_propagate() {
    let lines = [
        json!({"type": "user", "toolUseResult": {"password": "login"}}).to_string(),
        json!({"type": "user", "cwd": "/home/dev/w/gadget/.worktrees/login",
            "gitBranch": "feature/login-page"})
        .to_string(),
    ];
    let (out, report) = scrub_text(&(lines.join("\n") + "\n"));
    assert!(out.contains(".worktrees/login"), "{out}");
    assert!(out.contains("feature/login-page"), "{out}");
    assert_eq!(report.counts.get("propagated"), None, "{report}");
}

/// The review's probe: duplicate keys hide a value from the parsed view.
#[test]
fn duplicate_keys_are_caught_on_the_raw_line() {
    let token = fake("ghp_", 36);
    let (out, report) = scrub_text(&format!("{{\"t\":\"{token}\",\"t\":\"x\"}}\n"));
    assert!(!out.contains(&token), "{out}");
    assert_eq!(report.counts.get("github"), Some(&1));
}

#[test]
fn a_dump_landing_in_raw_is_rejected() {
    let scratch = Scratch::new("overlap");
    let out = scratch.0.join("c");
    let raw = out.join("raw");
    fs::create_dir_all(&raw).expect("raw dir");
    dump(&raw.join("raw.tar.zst"), &[("projects/-w/s.jsonl", "{}\n")]);
    dump(&raw.join("dev.tar.zst"), &[("projects/-w/s.jsonl", "{}\n")]);
    let report = run(&Options {
        raw: raw.clone(),
        out: out.clone(),
    })
    .expect("scrub");
    assert_eq!(report.skipped_archives, 1);
    assert_eq!(report.archives, 1);
    assert!(!raw.join("projects").exists(), "nothing written into raw");
    assert!(out.join("dev/projects/-w/s.jsonl").exists());
}
