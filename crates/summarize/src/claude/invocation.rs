//! The hardened `claude -p` command line (design §6.4).
//!
//! Headless Claude Code is a full agent by default: it has shell and file
//! tools and loads `CLAUDE.md`, hooks, skills, settings and MCP servers from
//! its working directory and the user's home. Every flag below exists to
//! switch one of those off; each was checked against `claude --help`
//! (Claude Code 2.1.317) and the CLI reference at
//! <https://code.claude.com/docs/en/cli-reference>.

use std::ffi::OsString;
use std::path::PathBuf;

use serde_json::Value;

/// The argument vector and environment for one run. Built by
/// [`Invocation::new`]; the working directory is a fresh temp dir chosen at
/// spawn time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invocation {
    /// The executable.
    pub(crate) program: PathBuf,
    /// Arguments, without the program name.
    pub(crate) args: Vec<String>,
    /// The complete child environment (the parent's is cleared).
    pub(crate) env: Vec<(String, String)>,
}

/// Parent environment variables passed through: what the CLI needs to find
/// its login (`HOME`, `CLAUDE_CONFIG_DIR`, XDG dirs), run (`PATH`, locale)
/// and reach the API (proxy and CA settings). Everything else, including
/// API keys and `CLAUDE_CODE_*` overrides of the parent, is dropped.
const INHERITED_ENV: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "CLAUDE_CONFIG_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// Variables always set, each documented at
/// <https://code.claude.com/docs/en/env-vars> (`SKIP_PROMPT_HISTORY` under
/// `--no-session-persistence` in the CLI reference). Defence in depth on top
/// of the flags.
pub(crate) const FIXED_ENV: &[(&str, &str)] = &[
    // No auto-update, telemetry, error reporting or feature-flag fetches.
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    // No CLAUDE.md of any kind (user, project, auto memory).
    ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
    // No auto memory read or written.
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    // Nothing saved to prompt history.
    ("CLAUDE_CODE_SKIP_PROMPT_HISTORY", "1"),
];

/// An empty MCP configuration: with `--strict-mcp-config`, the session has
/// no MCP servers at all.
const NO_MCP_SERVERS: &str = r#"{"mcpServers":{}}"#;

/// Settings layered on top (`--settings`): hooks off even if some source
/// were still loaded.
const FLAG_SETTINGS: &str = r#"{"disableAllHooks":true}"#;

impl Invocation {
    /// The hardened command line for one structured request. `inherited`
    /// is the parent environment already reduced by [`allowlisted_env`].
    pub(crate) fn new(
        program: PathBuf,
        model: &str,
        schema: &Value,
        system_prompt: &str,
        inherited: &[(String, String)],
    ) -> Self {
        let args = [
            // Non-interactive: read the prompt from stdin, print, exit.
            "--print",
            "--model",
            model,
            // A single JSON result object with `structured_output`.
            "--output-format",
            "json",
            "--json-schema",
            &schema.to_string(),
            // Replace Claude Code's agent system prompt entirely.
            "--system-prompt",
            system_prompt,
            // No built-in tools at all ("" disables all).
            "--tools",
            "",
            // `--tools` does not cover MCP tools: deny any that appear.
            "--disallowedTools",
            "mcp__*",
            // Only MCP servers from --mcp-config, and that config is empty.
            "--strict-mcp-config",
            "--mcp-config",
            NO_MCP_SERVERS,
            // Load no user, project or local settings files.
            "--setting-sources",
            "",
            "--settings",
            FLAG_SETTINGS,
            // No CLAUDE.md, skills, plugins, hooks, MCP servers, custom
            // commands/agents, output styles or auto memory.
            "--safe-mode",
            // No skills or slash commands, so `/x` in the input is just text.
            "--disable-slash-commands",
            // Nothing written to disk for resuming.
            "--no-session-persistence",
            // Deny anything that would need approval, and tell the model
            // nobody can approve (also removes AskUserQuestion).
            "--permission-mode",
            "dontAsk",
            "--permission-prompts",
            "none",
            "--no-chrome",
        ]
        .map(str::to_owned)
        .to_vec();

        let mut env = inherited.to_vec();
        env.extend(FIXED_ENV.iter().map(|&(k, v)| (k.to_owned(), v.to_owned())));

        Self { program, args, env }
    }
}

/// The [`INHERITED_ENV`] entries of `parent` (e.g. `std::env::vars_os()`)
/// with UTF-8 values, sorted by name, first occurrence kept. Everything
/// else is dropped here, so it is never stored, logged or passed on.
pub(crate) fn allowlisted_env(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = parent
        .into_iter()
        .filter(|(k, _)| k.to_str().is_some_and(|k| INHERITED_ENV.contains(&k)))
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    env.sort_by(|a, b| a.0.cmp(&b.0));
    env.dedup_by(|a, b| a.0 == b.0);
    env
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn invocation(inherited: Vec<(&str, &str)>) -> Invocation {
        Invocation::new(
            PathBuf::from("claude"),
            "claude-haiku-5-5",
            &json!({"type": "object"}),
            "SYSTEM",
            &allowlisted_env(
                inherited
                    .into_iter()
                    .map(|(k, v)| (OsString::from(k), OsString::from(v))),
            ),
        )
    }

    /// The exact argv: any change to the hardening must update this test.
    #[test]
    fn exact_argv() {
        let inv = invocation(vec![]);
        let want: Vec<&str> = vec![
            "--print",
            "--model",
            "claude-haiku-5-5",
            "--output-format",
            "json",
            "--json-schema",
            r#"{"type":"object"}"#,
            "--system-prompt",
            "SYSTEM",
            "--tools",
            "",
            "--disallowedTools",
            "mcp__*",
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--setting-sources",
            "",
            "--settings",
            r#"{"disableAllHooks":true}"#,
            "--safe-mode",
            "--disable-slash-commands",
            "--no-session-persistence",
            "--permission-mode",
            "dontAsk",
            "--permission-prompts",
            "none",
            "--no-chrome",
        ];
        assert_eq!(inv.args, want);
    }

    #[test]
    fn never_enables_tools_or_extra_context() {
        let inv = invocation(vec![]);
        for forbidden in [
            "--allowedTools",
            "--allowed-tools",
            "--add-dir",
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "--continue",
            "--resume",
            "--agents",
            "--plugin-dir",
            "--chrome",
            "--append-system-prompt",
        ] {
            assert!(!inv.args.iter().any(|a| a == forbidden), "{forbidden}");
        }
        // The input is never on the command line.
        assert!(!inv.args.iter().any(|a| a.contains("UNTRUSTED DATA")));
    }

    #[test]
    fn environment_is_an_allowlist() {
        let inv = invocation(vec![
            ("HOME", "/home/u"),
            ("PATH", "/usr/bin"),
            ("ANTHROPIC_API_KEY", "sk-secret"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("CLAUDECODE", "1"),
            ("AWS_SECRET_ACCESS_KEY", "x"),
            ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "0"),
        ]);
        let mut want: Vec<(String, String)> = vec![
            ("HOME".into(), "/home/u".into()),
            ("PATH".into(), "/usr/bin".into()),
        ];
        want.extend(FIXED_ENV.iter().map(|&(k, v)| (k.into(), v.into())));
        assert_eq!(inv.env, want);
    }
}
