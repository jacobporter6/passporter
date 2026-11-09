//! A conservative parser for Claude Code's and Codex's interactive dialogs,
//! from the text of herdr's `agent.read` (design §6.2).
//!
//! It recognizes the numbered menus Claude Code draws for permission
//! prompts (`Do you want to proceed?`, `Do you want to make this edit to
//! X?`), plan approval (`Would you like to proceed?`) and generic questions
//! with a `❯` cursor, and turns them into a protocol [`PendingInput`] with
//! `source: "screen"`.
//!
//! Codex (0.166, [`Dialect::Codex`]) draws its approvals the other way
//! up: the question (`Would you like to run the following command?`) comes
//! first, then the request (environment, the agent's reason, the command),
//! then a menu with a `›` cursor whose labels end in a shortcut letter
//! (`Yes, proceed (y)`). The question is found by its fixed wording, the
//! lines between it and the menu are the detail, and the same risk rules
//! apply: *Yes, proceed* is low, *Yes, and don't ask again…* elevated.
//!
//! # Rules
//!
//! - Only the **bottom-most** numbered menu counts, and only if nothing but
//!   blank lines, box rules and key hints (`Esc to cancel · …`) follows it:
//!   a menu with output or a prompt box below it is history, not a live
//!   dialog.
//! - [`MIN_CONFIDENCE`] or more: the options are offered. Below it the input
//!   is returned with **no options** and kind `unknown`, so the app falls
//!   back to the screen tail and keypad, and policy still treats the agent
//!   as being at a dialog. The parser never guesses.
//! - Risk: a label mentioning *don't ask again*, *always*, *auto-accept*,
//!   *bypass* (and similar standing grants) is elevated, whatever else it
//!   says; plain *Yes*/*proceed*/*manually approve* and any *No…* are low;
//!   anything unrecognized is elevated. The whole label is classified,
//!   including lines it wrapped onto.
//! - Each option is selected by its digit (Claude's menus accept it), except
//!   a label marked `(esc)`, which is selected with Esc.
//! - Only *No, and tell Claude what to do differently (esc)* accepts
//!   feedback: Esc rejects and returns Claude to its prompt, where the
//!   feedback is then sent as a prompt. Claude Code 2.1.317 shows a plain
//!   *No* with an `Esc to cancel · Tab to amend` footer instead: there,
//!   feedback goes through *Tab to amend*, which is not supported yet, so
//!   `no` does not accept feedback (choose it, then send a prompt).
//!
//! - Prompt: the dialog's question, except where it names nothing. A
//!   generic command question (Claude's `Bash command` dialog asking `Do
//!   you want to proceed?`, Codex's `Would you like to run the following
//!   command?`) becomes ``Run `<command's first line>`?``, and a plan
//!   approval `Approve the plan?`: the hook path's wording, so either
//!   source reads the same on a card, in a notification and in the car.
//!
//! Screen text is untrusted: everything here is bounded, and the labels,
//! prompt and detail it produces are plain text for display.

use protocol::{
    Confidence, OptionId, PendingId, PendingInput, PendingKind, PendingOption, PendingOptions,
    PendingSource, Risk, ScreenTail, text,
};

use super::hook::{HookPermission, Match};
use crate::registry::sanitize_label;

/// Options are offered only at or above this confidence (percent).
pub(crate) const MIN_CONFIDENCE: u8 = 60;

/// Lines from the bottom of the screen the parser looks at.
const WINDOW_LINES: usize = 80;
/// Most option lines plus their wrapped continuations in one menu.
const MAX_MENU_LINES: usize = 40;
/// Most lines of the question above a menu.
const MAX_QUESTION_LINES: usize = 3;
/// How far above the question a dialog title (e.g. `Bash command`) may be.
const MAX_TITLE_DISTANCE: usize = 40;
/// Labels Claude Code itself writes (recognized options).
const MAX_LABEL_CHARS: usize = 80;
/// Labels of unrecognized options (`opt_N`), often agent-written
/// (`AskUserQuestion`): shown in full up to this generous cap.
const MAX_FREE_LABEL_CHARS: usize = 300;
pub(super) const MAX_PROMPT_CHARS: usize = 200;
const MAX_DETAIL_LINES: usize = 20;
const MAX_DETAIL_CHARS: usize = 2000;

/// The keys that select options 1–9.
const DIGITS: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];
const ESC: &str = "esc";

/// Titles Claude Code draws at the top of a dialog box; the lines between
/// one and the question are the dialog's detail.
const TITLES: &[&str] = &[
    "bash command",
    "edit file",
    "create file",
    "write file",
    "overwrite file",
    "fetch",
    "here is claude's plan:",
];

/// Phrases that make an option a standing grant (or otherwise widen what
/// Claude may do unasked): always elevated.
const ELEVATED_MARKERS: &[&str] = &[
    "don't ask again",
    "dont ask again",
    "always",
    "auto-accept",
    "auto accept",
    "auto mode",
    "bypass",
    "allow all",
    "this session",
    "clear context",
    "skip permissions",
    "dangerously",
];

/// Whose dialogs a screen shows: picks the menu cursor, where the question
/// sits, which key hints labels carry and how the agent names itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Dialect {
    /// Claude Code (and any agent kind without a dialect of its own).
    #[default]
    Claude,
    /// Codex CLI.
    Codex,
}

impl Dialect {
    /// The dialect of a herdr agent kind (`claude`, `codex`, …).
    pub(crate) fn for_kind(kind: Option<&str>) -> Self {
        match crate::registry::agent_kind(kind) {
            protocol::AgentKind::Codex => Self::Codex,
            _ => Self::Claude,
        }
    }

    /// The marker in front of the selected option.
    fn cursor(self) -> char {
        match self {
            Self::Claude => '❯',
            Self::Codex => '›',
        }
    }

    /// How the agent's own menus name it (`tell Claude what to do…`).
    fn agent_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// How Codex approval questions start (lowercase). Codex writes these;
/// the agent's reason is shown on a `Reason:` line and never matches.
const CODEX_QUESTIONS: &[&str] = &["would you like to ", "do you want to "];

/// Questions that say nothing about what is asked (lower case): a dialog
/// asking one of them over a command it shows is described by that
/// command instead ([`run_prompt`]).
const GENERIC_COMMAND_QUESTIONS: &[&str] = &[
    // Claude Code's `Bash command` dialog.
    "do you want to proceed?",
    // Codex's command approval.
    "would you like to run the following command?",
];

/// The prompt of a plan approval, whatever the dialog's own question
/// (Claude's is `Would you like to proceed?`); the hook path says the same.
pub(super) const PLAN_PROMPT: &str = "Approve the plan?";

/// A parsed screen: the pending input plus the bridge-internal key that
/// selects each offered option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dialog {
    /// What goes on the wire.
    pub(crate) input: PendingInput,
    /// One allowlisted herdr key per option in `input.options`.
    keys: Vec<(OptionId, &'static str)>,
    /// How a hook request [described](Self::describe_with) it, if one did.
    described: Option<Match>,
}

impl Dialog {
    /// The herdr key that selects option `id`, if it is offered.
    pub(crate) fn key_for(&self, id: &OptionId) -> Option<&'static str> {
        self.keys
            .iter()
            .find(|(option, _)| option == id)
            .map(|(_, key)| *key)
    }

    /// The input as policy judges it: [`PendingSource::Hook`] once a hook
    /// request matched the whole dialog ([`Match::Exact`]), the screen's
    /// source otherwise. A prefix match against a cut screen proves only
    /// the start of the command (what follows the cut could be anything),
    /// so it stays screen-sourced. Only the car policy looks at the source
    /// (it answers hook-sourced dialogs only); the wire keeps saying
    /// `screen`, because the options and keys are still the screen's.
    pub(crate) fn policy_input(&self) -> PendingInput {
        let mut input = self.input.clone();
        if self.described == Some(Match::Exact) {
            input.source = PendingSource::Hook;
        }
        input
    }

    /// Takes the prompt and detail of `hook`, a request that
    /// [describes](HookPermission::describes) this dialog. A hook without a
    /// detail (a question) leaves the screen's detail alone.
    ///
    /// Everything else stays the screen's: the options and their keys (the
    /// hook does not say which keys answer the dialog), the kind of an
    /// unparsed dialog (`unknown`, so it stays a dialog), the source
    /// (`screen`, with its confidence and screen tail, so policy keeps
    /// treating the options as screen-parsed) and the id. The id is the
    /// screen's because the hook adds no new facts about the dialog, only a
    /// fuller wording of what the screen shows; it therefore does not
    /// change when a hook arrives or is forgotten, and an answer in flight
    /// is not refused as stale.
    pub(crate) fn describe_with(&mut self, hook: &HookPermission, matched: Match) {
        self.described = Some(matched);
        self.input.prompt.clone_from(&hook.prompt);
        if hook.detail.is_some() {
            self.input.detail.clone_from(&hook.detail);
        }
    }
}

/// Parses `screen` (herdr `agent.read` text) of an agent of `dialect`
/// blocked at `seq` into its pending input. Always an input (one without options when
/// nothing parses confidently); `None` only if the id cannot be built,
/// which cannot happen for `screen-<u64>-<hex>`.
///
/// The id is `screen-<seq>-<12 hex>`: the hex is a hash of what the user
/// is shown and what choosing sends (kind, prompt, detail, and each
/// option's id, label and key, in order). Two different dialogs at the same
/// `seq` (herdr does not always bump it between back-to-back prompts) get
/// different ids, so an answer meant for one is stale for the other.
pub(crate) fn parse(screen: &str, seq: u64, dialect: Dialect) -> Option<Dialog> {
    let mut dialog = parse_body(
        screen,
        PendingId::new(format!("screen-{seq}")).ok()?,
        dialect,
    );
    dialog.input.id = PendingId::new(format!("screen-{seq}-{}", content_hash(&dialog))).ok()?;
    Some(dialog)
}

/// Whether `id` is of a dialog parsed while the agent was at `seq` (ids
/// are `screen-<seq>-<hex>`): a cached parse from before is not the dialog
/// the agent shows now.
pub(crate) fn parsed_at(id: &PendingId, seq: u64) -> bool {
    id.as_str()
        .strip_prefix("screen-")
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(at, _)| at == seq.to_string())
}

/// 48 bits of FNV-1a 64 over a length-prefixed encoding of the dialog's
/// content, as 12 lowercase hex digits. Not a security boundary: it only
/// has to tell two dialogs apart.
fn content_hash(dialog: &Dialog) -> String {
    fn field(hash: &mut u64, bytes: &[u8]) {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        for byte in len.to_le_bytes().iter().chain(bytes) {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    let input = &dialog.input;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let kind = serde_json::to_string(&input.kind).unwrap_or_default();
    field(&mut hash, kind.as_bytes());
    field(&mut hash, input.prompt.as_bytes());
    match &input.detail {
        Some(detail) => {
            field(&mut hash, b"1");
            field(&mut hash, detail.as_bytes());
        }
        None => field(&mut hash, b"0"),
    }
    for option in input.options.as_slice() {
        field(&mut hash, option.id.as_str().as_bytes());
        field(&mut hash, option.label.as_bytes());
        field(
            &mut hash,
            dialog.key_for(&option.id).unwrap_or("").as_bytes(),
        );
    }
    format!("{:012x}", hash >> 16)
}

/// [`parse`] with a given id.
fn parse_body(screen: &str, id: PendingId, dialect: Dialect) -> Dialog {
    let tail = ScreenTail::new(
        crate::server::screen_tail(screen, protocol::pending::MAX_SCREEN_TAIL_LINES)
            .lines()
            .map(str::to_owned)
            .collect(),
    );
    let lines = window(screen);
    let parsed = find_menu(&lines, dialect).map(|menu| analyze(&lines, &menu, dialect));
    let Some(parsed) = parsed else {
        return low_confidence(id, String::new(), None, 0, tail);
    };
    let confidence = parsed.confidence;
    if confidence < MIN_CONFIDENCE {
        let prompt = if parsed.prompt.ends_with('?') {
            parsed.prompt
        } else {
            String::new()
        };
        return low_confidence(id, prompt, parsed.detail, confidence, tail);
    }
    let mut options = Vec::new();
    let mut keys = Vec::new();
    for choice in parsed.choices {
        let Ok(option_id) = OptionId::new(&choice.id) else {
            continue;
        };
        keys.push((option_id.clone(), choice.key));
        options.push(PendingOption {
            id: option_id,
            label: choice.label,
            risk: choice.risk,
            accepts_feedback: choice.accepts_feedback,
        });
    }
    // Ids are made unique below; a failure here means a bug, and the safe
    // answer to a bug is the keypad.
    let Ok(options) = PendingOptions::new(options) else {
        return low_confidence(id, parsed.prompt, parsed.detail, 0, tail);
    };
    Dialog {
        input: PendingInput {
            id,
            kind: parsed.kind,
            prompt: parsed.prompt,
            detail: parsed.detail,
            options,
            suggestions: Vec::new(),
            source: screen_source(confidence, tail),
        },
        keys,
        described: None,
    }
}

fn screen_source(confidence: u8, screen_tail: ScreenTail) -> PendingSource {
    PendingSource::Screen {
        confidence: Confidence::from(u64::from(confidence)),
        screen_tail,
    }
}

fn low_confidence(
    id: PendingId,
    prompt: String,
    detail: Option<String>,
    confidence: u8,
    tail: ScreenTail,
) -> Dialog {
    Dialog {
        input: PendingInput {
            id,
            kind: PendingKind::Unknown,
            prompt,
            detail,
            options: PendingOptions::default(),
            suggestions: Vec::new(),
            source: screen_source(confidence, tail),
        },
        keys: Vec::new(),
        described: None,
    }
}

/// One screen line with control and hidden characters removed and box
/// borders (`│ … │`) stripped, trailing space trimmed.
#[derive(Debug, Clone)]
struct Line {
    text: String,
}

impl Line {
    fn new(raw: &str) -> Self {
        let clean: String = raw
            .chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|&c| !(c.is_control() || text::is_hidden_char(c)))
            .collect();
        let mut text = clean.trim_end();
        loop {
            let start = text.trim_start();
            let Some(inner) = start.strip_prefix('│').or_else(|| start.strip_prefix('┃'))
            else {
                break;
            };
            let inner = inner.trim_end();
            text = inner
                .strip_suffix('│')
                .or_else(|| inner.strip_suffix('┃'))
                .unwrap_or(inner)
                .trim_end();
        }
        Self {
            text: text.to_owned(),
        }
    }

    fn trimmed(&self) -> &str {
        self.text.trim()
    }

    fn is_blank(&self) -> bool {
        self.trimmed().is_empty()
    }

    fn indent(&self) -> usize {
        self.text.chars().take_while(|c| *c == ' ').count()
    }

    /// Only box-drawing characters (a rule or a box corner line).
    fn is_rule(&self) -> bool {
        let t = self.trimmed();
        !t.is_empty()
            && t.chars()
                .all(|c| c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c))
    }

    /// A key-hint footer such as `Esc to cancel · Tab to amend`.
    fn is_hint(&self) -> bool {
        let t = self.trimmed().to_lowercase();
        [
            "esc to ",
            "enter to ",
            "tab to ",
            "↑/↓",
            "ctrl+",
            "to navigate",
            "to cancel",
        ]
        .iter()
        .any(|hint| t.contains(hint))
    }

    /// `[cursor] N. label`, with the digit's column.
    fn option(&self, dialect: Dialect) -> Option<OptionLine> {
        let indent = self.indent();
        let rest = &self.text[indent..];
        let (cursor, rest) = match rest.strip_prefix(dialect.cursor()) {
            Some(after) => (true, after.trim_start()),
            None => (false, rest),
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || digits.len() > 2 {
            return None;
        }
        let after = rest[digits.len()..].strip_prefix(". ")?;
        let label = after.trim();
        if label.is_empty() {
            return None;
        }
        let digit_col = self.text.chars().count() - rest.chars().count();
        Some(OptionLine {
            number: digits.parse().ok()?,
            label: label.to_owned(),
            cursor,
            digit_col,
        })
    }
}

#[derive(Debug, Clone)]
struct OptionLine {
    number: usize,
    label: String,
    cursor: bool,
    digit_col: usize,
}

/// The last [`WINDOW_LINES`] lines, trailing blank lines dropped.
fn window(screen: &str) -> Vec<Line> {
    let mut lines: Vec<Line> = screen.lines().map(Line::new).collect();
    while lines.last().is_some_and(Line::is_blank) {
        lines.pop();
    }
    let start = lines.len().saturating_sub(WINDOW_LINES);
    lines.split_off(start)
}

/// The bottom-most numbered menu.
#[derive(Debug)]
struct Menu {
    /// Index of the first option line.
    start: usize,
    /// Each option with its label continuation lines joined.
    options: Vec<OptionLine>,
    /// The structure is sound: options numbered 1..=n, every line between
    /// them a deeper-indented continuation.
    well_formed: bool,
    /// Something other than blank lines, rules and hints follows the menu.
    trailing_junk: bool,
}

fn find_menu(lines: &[Line], dialect: Dialect) -> Option<Menu> {
    let last = lines
        .iter()
        .rposition(|line| line.option(dialect).is_some())?;
    // Walk up to option 1 without crossing a blank line.
    let mut start = None;
    for i in (last.saturating_sub(MAX_MENU_LINES)..=last).rev() {
        let line = &lines[i];
        if line.is_blank() {
            break;
        }
        if line
            .option(dialect)
            .is_some_and(|option| option.number == 1)
        {
            start = Some(i);
            break;
        }
    }
    let Some(start) = start else {
        // A menu we cannot see the top of: not well formed.
        let options = lines[last].option(dialect).into_iter().collect();
        return Some(Menu {
            start: last,
            options,
            well_formed: false,
            trailing_junk: false,
        });
    };
    let mut options: Vec<OptionLine> = Vec::new();
    let mut well_formed = true;
    let mut end = start;
    for (i, line) in lines.iter().enumerate().skip(start) {
        if let Some(option) = line.option(dialect) {
            if i > last {
                break;
            }
            options.push(option);
            end = i;
            continue;
        }
        let Some(current) = options.last_mut() else {
            break;
        };
        let continuation = !line.is_blank() && !line.is_hint() && line.indent() > current.digit_col;
        if continuation {
            current.label.push(' ');
            current.label.push_str(line.trimmed());
            end = i;
        } else if i < last {
            well_formed = false;
            end = i;
        } else {
            break;
        }
    }
    well_formed &= options
        .iter()
        .enumerate()
        .all(|(i, option)| option.number == i + 1)
        && (2..=DIGITS.len()).contains(&options.len());
    let trailing_junk = lines[end + 1..]
        .iter()
        .any(|line| !(line.is_blank() || line.is_rule() || line.is_hint()));
    Some(Menu {
        start,
        options,
        well_formed,
        trailing_junk,
    })
}

/// One option as offered, before validation into protocol types.
#[derive(Debug)]
struct Choice {
    id: String,
    label: String,
    risk: Risk,
    accepts_feedback: bool,
    key: &'static str,
}

#[derive(Debug)]
struct Analysis {
    kind: PendingKind,
    prompt: String,
    detail: Option<String>,
    choices: Vec<Choice>,
    confidence: u8,
}

/// The question a menu answers, with where its dialog is drawn.
struct Question {
    prompt: String,
    /// The line of a Claude dialog title, if there is one.
    title: Option<usize>,
    detail: Option<String>,
    /// A Codex approval question, found by its wording.
    codex_approval: bool,
    /// The command the dialog asks to run, when it is a Claude `Bash
    /// command` dialog or a Codex approval, laid out unambiguously.
    command: Option<Command>,
}

/// What a dialog shows of the command it asks to run.
struct Command {
    /// Its first screen line.
    first: String,
    /// Whether more of it follows on other lines (a wrap or a multi-line
    /// command). Lines are never joined: Claude wraps mid-word.
    cut: bool,
}

fn find_question(lines: &[Line], menu: &Menu, dialect: Dialect) -> Question {
    if dialect == Dialect::Codex
        && let Some(at) = codex_question(lines, menu.start)
    {
        let body = &lines[at + 1..menu.start];
        return Question {
            prompt: lines[at].trimmed().to_owned(),
            title: None,
            detail: detail(body),
            codex_approval: true,
            command: codex_command(body),
        };
    }
    let (prompt, question_top) = question(lines, menu.start);
    let title = match dialect {
        Dialect::Claude => question_top.and_then(|top| dialog_title(lines, top)),
        Dialect::Codex => None,
    };
    let body = match (title, question_top) {
        (Some(title), Some(top)) => Some((&lines[title], &lines[title + 1..top])),
        _ => None,
    };
    Question {
        prompt,
        title,
        detail: body.and_then(|(_, body)| detail(body)),
        codex_approval: false,
        command: body.and_then(|(title, body)| {
            title
                .trimmed()
                .eq_ignore_ascii_case("bash command")
                .then(|| bash_command(body))
                .flatten()
        }),
    }
}

fn analyze(lines: &[Line], menu: &Menu, dialect: Dialect) -> Analysis {
    let Question {
        prompt,
        title,
        detail,
        codex_approval,
        command,
    } = find_question(lines, menu, dialect);
    let lower = prompt.to_lowercase();

    let mut choices: Vec<Choice> = Vec::new();
    let mut all_recognized = true;
    for option in &menu.options {
        let (label, hint) = strip_key_hint(&option.label, dialect);
        let class = classify(label, hint == Some(ESC), dialect);
        let Class::Option {
            slug,
            risk,
            accepts_feedback,
        } = class
        else {
            continue;
        };
        all_recognized &= slug.is_some();
        let fallback = format!("opt_{}", option.number);
        let id = slug
            .filter(|slug| !choices.iter().any(|c| c.id == *slug))
            .map_or(fallback, str::to_owned);
        let key = if hint == Some(ESC) {
            ESC
        } else {
            let Some(digit) = option.number.checked_sub(1).and_then(|i| DIGITS.get(i)) else {
                all_recognized = false;
                continue;
            };
            digit
        };
        // Risk was classified on the whole label. A recognized option (text
        // Claude Code writes) drops its ` · description` tail (2.1.317's
        // "switch to auto mode · auto mode handles…"); an unrecognized one,
        // possibly agent-written, is shown in full: never hide its text.
        let label = if slug.is_some() {
            bounded(label.split(" · ").next().unwrap_or(label), MAX_LABEL_CHARS)
        } else {
            bounded(label, MAX_FREE_LABEL_CHARS)
        };
        choices.push(Choice {
            id,
            label,
            risk,
            accepts_feedback,
            key,
        });
    }

    let permission = match dialect {
        Dialect::Claude => lower.starts_with("do you want to") && lower.ends_with('?'),
        Dialect::Codex => codex_approval,
    };
    let plan_markers = title.is_some_and(|t| lines[t].trimmed().to_lowercase().contains("plan"))
        || choices.iter().any(|c| {
            c.id.starts_with("yes_auto_accept")
                || c.id == "yes_manual"
                || c.id == "no_keep_planning"
        });
    let plan = lower.starts_with("would you like to proceed") && plan_markers;
    let kind = if permission {
        PendingKind::Permission
    } else if plan {
        PendingKind::PlanApproval
    } else if lower.ends_with('?') {
        PendingKind::Question
    } else {
        PendingKind::Unknown
    };

    let cursors = menu.options.iter().filter(|o| o.cursor).count();
    let mut score: i32 = 0;
    if menu.well_formed && !choices.is_empty() {
        score += 35;
        if cursors == 1 {
            score += 20;
        }
        if lower.ends_with('?') {
            score += 15;
        }
        if permission || plan {
            score += 20;
        }
        if all_recognized {
            score += 10;
        }
        // Claude's menus always show exactly one `❯`.
        if menu.trailing_junk || cursors != 1 {
            score -= 50;
        }
    }
    let mut confidence = u8::try_from(score.clamp(0, 100)).unwrap_or(0);
    if dialect == Dialect::Codex && !codex_approval {
        // Only Codex approvals with their question are understood; any
        // other Codex menu (or one whose question is out of sight) goes
        // to the keypad rather than chips with a guessed prompt.
        confidence = confidence.min(MIN_CONFIDENCE - 1);
    }
    Analysis {
        kind,
        prompt: bounded(&shown_prompt(kind, prompt, command), MAX_PROMPT_CHARS),
        detail,
        choices,
        confidence,
    }
}

/// The question directly above the menu (skipping blank lines): up to
/// [`MAX_QUESTION_LINES`] contiguous text lines, stopping at a rule or at
/// a line that ends a sentence above the bottom one. Returns it joined, and
/// the index of its top line.
fn question(lines: &[Line], menu_start: usize) -> (String, Option<usize>) {
    let mut i = menu_start;
    while i > 0 && lines[i - 1].is_blank() {
        i -= 1;
    }
    let mut parts: Vec<&str> = Vec::new();
    let mut top = None;
    while i > 0 && parts.len() < MAX_QUESTION_LINES {
        let line = &lines[i - 1];
        if line.is_blank() || line.is_rule() {
            break;
        }
        let t = line.trimmed();
        if !parts.is_empty() && t.ends_with(['.', '?', '!', ':']) {
            break;
        }
        parts.push(t);
        top = Some(i - 1);
        i -= 1;
    }
    parts.reverse();
    (parts.join(" "), top)
}

/// The index of a Codex approval question above the menu starting at
/// `menu_start`, anchored on what the agent cannot draw:
///
/// - Codex indents everything in its approval (question, environment,
///   reason, command) while the conversation above starts in column 0
///   (`• Running …`). The dialog's top is therefore only known to be in
///   sight when a column-0 line is visible above the question; otherwise
///   the question may have scrolled off and an agent-written line be all
///   that remains.
/// - Exactly one indented line above the menu may open with one of
///   [`CODEX_QUESTIONS`] and end in `?`. A second one (the agent wrote a
///   question-like line in its command or reason, or one is left in the
///   conversation) makes the screen ambiguous.
/// - The question is the approval's first line: nothing but blank lines
///   between it and the last column-0 line above it. In a narrow pane
///   Codex wraps its question so that no line matches; an agent-written
///   copy lower in the approval then still has Codex's lines above it.
/// - Between the question and the blank lines right above the menu there
///   is no rule and no gap of two blank lines: Codex leaves single blank
///   lines inside its approval, so a wider gap means the question belongs
///   to something else.
///
/// Anything else has no question, and the screen falls back to the keypad.
fn codex_question(lines: &[Line], menu_start: usize) -> Option<usize> {
    let mut end = menu_start;
    while end > 0 && lines[end - 1].is_blank() {
        end -= 1;
    }
    let is_question = |line: &Line| {
        let lower = line.trimmed().to_lowercase();
        line.indent() > 0
            && lower.ends_with('?')
            && CODEX_QUESTIONS.iter().any(|q| lower.starts_with(q))
    };
    let mut candidates = (0..end).filter(|&i| is_question(&lines[i]));
    let question = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    let body = &lines[question + 1..end];
    let gap = body
        .windows(2)
        .any(|pair| pair[0].is_blank() && pair[1].is_blank());
    if gap || body.iter().any(Line::is_rule) {
        return None;
    }
    // The question must open the indented approval block: the first
    // non-blank line below the last column-0 line. A wrapped question (a
    // narrow pane) matches no candidate, so a copy the agent wrote further
    // down cannot stand in for it.
    let conversation = lines[..question]
        .iter()
        .rposition(|line| !line.is_blank() && line.indent() == 0)?;
    let opens_block = lines[conversation + 1..question].iter().all(Line::is_blank);
    opens_block.then_some(question)
}

/// The prompt for a dialog of `kind` asking `question` (about `command`,
/// if it shows one): [`PLAN_PROMPT`] for a plan approval, ``Run `…`?``
/// for a permission whose question is one of
/// [`GENERIC_COMMAND_QUESTIONS`], else the question itself.
fn shown_prompt(kind: PendingKind, question: String, command: Option<Command>) -> String {
    match (kind, command) {
        (PendingKind::PlanApproval, _) => PLAN_PROMPT.to_owned(),
        (PendingKind::Permission, Some(command))
            if GENERIC_COMMAND_QUESTIONS.contains(&question.to_lowercase().as_str()) =>
        {
            run_prompt(&command.first, command.cut).unwrap_or(question)
        }
        _ => question,
    }
}

/// The command in a Claude `Bash command` dialog's `body` (between its
/// title and its question), if the layout is one of the two Claude draws:
///
/// - 2.1.317: a description, then the command between two rules, then
///   nothing. More than one text line between the rules means it is cut.
/// - Earlier: no rules; the command, then a one-line description. More
///   than two text lines means the command is cut.
///
/// The description is agent-written, so any other number of rules (one
/// it drew to pose as the frame) means `None`: the generic question
/// stays rather than naming the wrong command. `Tip:` paragraphs are
/// skipped.
fn bash_command(body: &[Line]) -> Option<Command> {
    let kept = without_tips(body);
    let rules: Vec<usize> = (0..kept.len()).filter(|&i| kept[i].is_rule()).collect();
    let text = |lines: &[&Line]| -> Vec<String> {
        lines
            .iter()
            .filter(|line| !line.is_blank())
            .map(|line| line.trimmed().to_owned())
            .collect()
    };
    let (lines, max_uncut) = match rules.as_slice() {
        [] => (text(&kept), 2),
        [open, close] if text(&kept[close + 1..]).is_empty() => (text(&kept[open + 1..*close]), 1),
        _ => return None,
    };
    let cut = lines.len() > max_uncut;
    let first = lines.into_iter().next()?;
    Some(Command { first, cut })
}

/// The command on a Codex approval's `$ ` line, if its `body` has exactly
/// one (the agent-written `Reason:` above it could hold a line that poses
/// as one). A text line right after it means the command is cut.
fn codex_command(body: &[Line]) -> Option<Command> {
    let mut dollars = (0..body.len()).filter(|&i| body[i].trimmed().starts_with("$ "));
    let at = dollars.next()?;
    if dollars.next().is_some() {
        return None;
    }
    let first = body[at]
        .trimmed()
        .trim_start_matches("$ ")
        .trim()
        .to_owned();
    let cut = body.get(at + 1).is_some_and(|line| !line.is_blank());
    (!first.is_empty()).then_some(Command { first, cut })
}

/// ``Run `command`?`` for the first line of a command, with ` …` inside
/// the span when it is `cut` (more of it follows): one clean label line,
/// backticks inside it turned into `'` so the inline code span stays
/// whole. `None` for a blank line. The hook path words its Bash requests
/// the same way, so a card reads the same either way.
pub(super) fn run_prompt(first_line: &str, cut: bool) -> Option<String> {
    let mut line = sanitize_label(first_line).replace('`', "'");
    if line.is_empty() {
        return None;
    }
    if cut && !line.ends_with('…') {
        line.push_str(" …");
    }
    Some(format!("Run `{line}`?"))
}

/// The index of a known dialog title above `question_top`.
fn dialog_title(lines: &[Line], question_top: usize) -> Option<usize> {
    let from = question_top.saturating_sub(MAX_TITLE_DISTANCE);
    (from..question_top)
        .rev()
        .find(|&i| TITLES.contains(&lines[i].trimmed().to_lowercase().as_str()))
}

/// The dialog body between its title and the question: rules and `Tip:`
/// paragraphs dropped, common indentation removed, bounded.
fn detail(body: &[Line]) -> Option<String> {
    let kept: Vec<&Line> = without_tips(body)
        .into_iter()
        .filter(|line| !line.is_rule())
        .collect();
    let first = kept.iter().position(|line| !line.is_blank())?;
    let last = kept.iter().rposition(|line| !line.is_blank())?;
    let kept = &kept[first..=last];
    let indent = kept
        .iter()
        .filter(|line| !line.is_blank())
        .map(|line| line.indent())
        .min()
        .unwrap_or(0);
    let mut out = String::new();
    for line in kept.iter().take(MAX_DETAIL_LINES) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.extend(line.text.chars().skip(indent));
    }
    if kept.len() > MAX_DETAIL_LINES {
        out.push_str("\n…");
    }
    Some(bounded(&out, MAX_DETAIL_CHARS))
}

/// `body` without Claude's `Tip: …` paragraphs. A tip runs from its
/// `Tip:` line through the lines it wrapped onto, which start in lower
/// case or punctuation; a blank line, a rule or a line starting with a
/// capital letter or digit ends it (2.1.317 puts the description right
/// after the tip, with no blank line).
fn without_tips(body: &[Line]) -> Vec<&Line> {
    let mut kept = Vec::new();
    let mut in_tip = false;
    for line in body {
        let t = line.trimmed();
        if t.starts_with("Tip:") {
            in_tip = true;
            continue;
        }
        if in_tip {
            let starts_new = t
                .chars()
                .next()
                .is_none_or(|c| c.is_uppercase() || c.is_ascii_digit());
            if !(starts_new || line.is_rule()) {
                continue;
            }
            in_tip = false;
        }
        kept.push(line);
    }
    kept
}

/// Splits a trailing key hint such as `(esc)` or `(shift+tab)` off a label;
/// in Codex menus also a shortcut letter such as `(y)`.
fn strip_key_hint(label: &str, dialect: Dialect) -> (&str, Option<&str>) {
    let trimmed = label.trim_end();
    if let Some(open) = trimmed.rfind('(')
        && trimmed.ends_with(')')
    {
        let hint = &trimmed[open + 1..trimmed.len() - 1];
        let looks_like_key = !hint.is_empty()
            && hint.len() <= 16
            && !hint.contains(' ')
            && (hint == ESC
                || hint.contains("tab")
                || hint.contains('+')
                || (dialect == Dialect::Codex
                    && hint.len() == 1
                    && hint.bytes().all(|b| b.is_ascii_lowercase())));
        if looks_like_key {
            return (trimmed[..open].trim_end(), Some(hint));
        }
    }
    (trimmed, None)
}

/// How one option label is offered.
#[derive(Debug, PartialEq, Eq)]
enum Class {
    /// A text-entry row (`Type something.`): not a chip.
    TextEntry,
    Option {
        /// A stable id, or `None` for `opt_N`.
        slug: Option<&'static str>,
        risk: Risk,
        accepts_feedback: bool,
    },
}

fn classify(label: &str, esc_hint: bool, dialect: Dialect) -> Class {
    let norm = label
        .to_lowercase()
        .replace('\u{2019}', "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let low = |slug| Class::Option {
        slug,
        risk: Risk::Low,
        accepts_feedback: false,
    };
    if norm.starts_with("type ") || norm == "chat about this" {
        return Class::TextEntry;
    }
    if ELEVATED_MARKERS.iter().any(|marker| norm.contains(marker)) {
        let yes = norm.starts_with("yes");
        let slug = if !yes {
            None
        } else if norm.contains("bypass") {
            Some("yes_bypass")
        } else if norm.contains("clear context") {
            Some("yes_clear_context")
        } else if norm.contains("auto mode") {
            Some("yes_auto_mode")
        } else if norm.contains("auto-accept") || norm.contains("auto accept") {
            Some("yes_auto_accept")
        } else {
            Some("yes_always")
        };
        return Class::Option {
            slug,
            risk: Risk::Elevated,
            accepts_feedback: false,
        };
    }
    match norm.as_str() {
        "yes" | "yes, proceed" | "proceed" => return low(Some("yes")),
        "yes, just this once" if dialect == Dialect::Codex => return low(Some("yes")),
        "yes, and manually approve edits" | "yes, manually approve edits" => {
            return low(Some("yes_manual"));
        }
        "no" => return low(Some("no")),
        "no, keep planning" => return low(Some("no_keep_planning")),
        _ => {}
    }
    let feedback = format!(
        "no, and tell {} what to do differently",
        dialect.agent_name()
    );
    if norm.starts_with(&feedback) {
        return Class::Option {
            slug: Some("no_feedback"),
            risk: Risk::Low,
            accepts_feedback: esc_hint,
        };
    }
    if norm.starts_with("no,") || norm.starts_with("no ") {
        return low(None);
    }
    Class::Option {
        slug: None,
        risk: Risk::Elevated,
        accepts_feedback: false,
    }
}

/// At most `max` characters, with `…` marking a cut.
pub(super) fn bounded(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> u64 {
        7
    }

    fn parse(screen: &str, seq: u64) -> Dialog {
        super::parse(screen, seq, Dialect::Claude).unwrap()
    }

    fn parse_codex(screen: &str) -> Dialog {
        super::parse(screen, id(), Dialect::Codex).unwrap()
    }

    fn is_screen_id(id: &PendingId, seq: u64) -> bool {
        id.as_str()
            .strip_prefix(&format!("screen-{seq}-"))
            .is_some_and(|hex| hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit()))
    }

    #[test]
    fn real_codex_0_166_command_approval() {
        use Risk::{Elevated, Low};
        let dialog = parse_codex(include_str!("fixtures/codex_0_166_command_approval.txt"));
        assert_eq!(dialog.input.kind, PendingKind::Permission);
        assert_eq!(dialog.input.prompt, "Run `printf 'hi\\n' > hello.txt`?");
        assert_eq!(
            dialog.input.detail.as_deref(),
            Some(
                "Environment: local\n\nReason: May I create hello.txt in this read-only \
                 workspace?\n\n$ printf 'hi\\n' > hello.txt"
            )
        );
        assert_eq!(
            options(&dialog),
            [
                row("yes", Low, false, "1"),
                row("yes_always", Elevated, false, "2"),
                row("no_feedback", Low, true, "esc"),
            ]
        );
        let labels: Vec<&str> = dialog
            .input
            .options
            .as_slice()
            .iter()
            .map(|o| o.label.as_str())
            .collect();
        assert_eq!(labels[0], "Yes, proceed");
        assert_eq!(labels[2], "No, and tell Codex what to do differently");
        assert_eq!(confidence(&dialog), 100);
        assert!(is_screen_id(&dialog.input.id, id()));
    }

    #[test]
    fn codex_screens_without_an_approval_offer_no_chips() {
        for (name, screen) in [
            (
                "folder trust",
                include_str!("fixtures/codex_0_166_folder_trust.txt"),
            ),
            (
                "after declining",
                include_str!("fixtures/codex_0_166_after_decline.txt"),
            ),
        ] {
            let dialog = parse_codex(screen);
            assert_eq!(dialog.input.options, PendingOptions::default(), "{name}");
            assert_eq!(dialog.input.kind, PendingKind::Unknown, "{name}");
        }
    }

    #[test]
    fn a_codex_question_is_only_taken_from_the_dialog() {
        // An unknown Codex dialog below a question from the conversation:
        // the question is history (two blank lines away), not the prompt.
        let screen = "• Would you like to run the tests?\n\n\n  Pick one\n\n\
                      › 1. Yes, proceed (y)\n  2. No (esc)\n";
        let dialog = parse_codex(screen);
        assert_ne!(dialog.input.kind, PendingKind::Permission);
        assert!(
            !dialog.input.prompt.contains("tests"),
            "{}",
            dialog.input.prompt
        );
        assert_eq!(dialog.input.options, PendingOptions::default());
    }

    /// The synthetic Bash permission fixture with `command` and `question`.
    fn bash_dialog(command: &str, question: &str) -> String {
        include_str!("fixtures/synthetic_bash_permission.txt")
            .replace("cargo nextest run -p bridge\n", &format!("{command}\n"))
            .replace("Do you want to proceed?", question)
    }

    #[test]
    fn a_generic_command_question_is_named_by_its_command() {
        let named = parse(
            &bash_dialog("echo `date`  &&  ls", "Do you want to proceed?"),
            1,
        );
        assert_eq!(
            named.input.prompt, "Run `echo 'date' && ls`?",
            "one clean span"
        );
        assert_eq!(named.input.kind, PendingKind::Permission);

        let specific = parse(&bash_dialog("ls", "Do you want to run ls here?"), 1);
        assert_eq!(specific.input.prompt, "Do you want to run ls here?");

        let long = parse(&bash_dialog(&"x".repeat(300), "Do you want to proceed?"), 1);
        assert!(long.input.prompt.starts_with("Run `xxx"));
        assert!(long.input.prompt.ends_with("…`?"));
        assert!(long.input.prompt.chars().count() <= MAX_PROMPT_CHARS);
    }

    #[test]
    fn a_wrapped_or_multi_line_command_is_marked_cut_never_joined() {
        let narrow = parse(include_str!("fixtures/synthetic_bash_narrow.txt"), 1);
        assert_eq!(narrow.input.prompt, "Run `git push origin it2/pending`?");
        let wrapped = bash_dialog("cargo nextest run -p\n   bridge", "Do you want to proceed?");
        assert_eq!(
            parse(&wrapped, 1).input.prompt,
            "Run `cargo nextest run -p …`?"
        );

        let real = include_str!("fixtures/real_claude_2_1_317_bash_permission.txt");
        let two_lines = real.replace(" touch probe-file.txt\n", " touch probe-\n file.txt\n");
        assert_eq!(parse(&two_lines, 1).input.prompt, "Run `touch probe- …`?");

        let codex = include_str!("fixtures/codex_0_166_command_approval.txt");
        let wrapped = codex.replace(
            "  $ printf 'hi\\n' > hello.txt\n",
            "  $ printf 'hi\\n' >\n  hello.txt\n",
        );
        assert_ne!(wrapped, codex);
        assert_eq!(
            parse_codex(&wrapped).input.prompt,
            "Run `printf 'hi\\n' > …`?"
        );
    }

    #[test]
    fn agent_written_text_never_poses_as_the_command() {
        // Claude 2.1.317: the description (agent-written) draws a rule and
        // a command of its own above the real frame.
        let real = include_str!("fixtures/real_claude_2_1_317_bash_permission.txt");
        let spoof = real.replace(
            " Create empty probe file\n",
            " Create empty probe file\n ╌╌╌╌╌╌╌╌╌╌\n rm -rf ~/work\n",
        );
        assert_ne!(spoof, real);
        assert_eq!(parse(&spoof, 1).input.prompt, "Do you want to proceed?");

        // Codex: the reason (agent-written) holds a `$` line of its own.
        let codex = include_str!("fixtures/codex_0_166_command_approval.txt");
        let spoof = codex.replace(
            "  Reason: May I create hello.txt in this read-only workspace?\n",
            "  Reason: May I create hello.txt?\n  $ ls -la\n",
        );
        assert_ne!(spoof, codex);
        let dialog = parse_codex(&spoof);
        assert_eq!(dialog.input.kind, PendingKind::Permission);
        assert_eq!(
            dialog.input.prompt,
            "Would you like to run the following command?"
        );
    }

    #[test]
    fn a_generic_question_without_a_command_stays() {
        let screen = "Do you want to proceed?\n❯ 1. Yes\n  2. No\n";
        assert_eq!(parse(screen, 1).input.prompt, "Do you want to proceed?");
    }

    #[test]
    fn a_question_the_agent_wrote_is_never_the_prompt() {
        let real = include_str!("fixtures/codex_0_166_command_approval.txt");
        let no_chips = |screen: &str, why: &str| {
            assert_ne!(screen, real, "{why}: fixture edited");
            let dialog = parse_codex(screen);
            assert_eq!(dialog.input.options, PendingOptions::default(), "{why}");
            assert!(confidence(&dialog) < MIN_CONFIDENCE, "{why}");
        };
        // A question-like line in the command, nearer the menu than
        // Codex's own question: ambiguous.
        let sneaky = real.replace(
            "  $ printf 'hi\\n' > hello.txt",
            "  $ rm -rf ~/work\n  Would you like to run the tests?",
        );
        no_chips(&sneaky, "second question");
        // Even after a two-blank-line gap in a heredoc, which would make it
        // the top of its own block.
        let heredoc = real.replace(
            "  $ printf 'hi\\n' > hello.txt",
            "  $ cat <<EOF\n  rm -rf ~/work\n\n\n  Would you like to run the following command?\n  EOF",
        );
        no_chips(&heredoc, "heredoc gap");
        // Codex's question scrolled off: only indented dialog lines are in
        // sight, the agent's line among them.
        let scrolled = "  rm -rf ~/work\n  Would you like to run the following command?\n\n  \
                        echo done\n\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again \
                        for these commands (p)\n  3. No, and tell Codex what to do differently \
                        (esc)\n\n  Press enter to confirm or esc to cancel\n";
        let dialog = parse_codex(scrolled);
        assert_eq!(dialog.input.options, PendingOptions::default(), "scrolled");
        // A question-like Reason (indented on its own line) is the second
        // candidate too.
        let reason = real.replace(
            "  Reason: May I create hello.txt in this read-only workspace?",
            "  Reason: May I?\n  Do you want to allow everything?",
        );
        no_chips(&reason, "reason");
        // A narrow pane: Codex wraps its own question, so only the agent's
        // copy in the command matches. It is not the approval's first line.
        let narrow = real
            .replace(
                "  Would you like to run the following command?",
                "  Would you like to run the\n  following command?",
            )
            .replace(
                "  $ printf 'hi\\n' > hello.txt",
                "  $ rm -rf ~/work\n  Would you like to run the following command?",
            );
        no_chips(&narrow, "wrapped question");
    }

    #[test]
    fn codex_menus_without_an_approval_question_go_to_the_keypad() {
        // Two blank lines inside the request hide the question: Claude's
        // generic question rule would have found "Run it?" and offered chips.
        let screen = "  Would you like to run the following command?\n\n  $ a\n\n\n  \
                      Run it?\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again (p)\n  \
                      3. No (esc)\n";
        let dialog = parse_codex(screen);
        assert_eq!(dialog.input.options, PendingOptions::default());
        assert!(confidence(&dialog) < MIN_CONFIDENCE);
    }

    #[test]
    fn codex_risk_and_hints() {
        let risk = |label: &str| match classify(label, false, Dialect::Codex) {
            Class::Option { slug, risk, .. } => (slug, risk),
            Class::TextEntry => panic!("{label}"),
        };
        assert_eq!(risk("Yes, proceed"), (Some("yes"), Risk::Low));
        assert_eq!(risk("Yes, just this once"), (Some("yes"), Risk::Low));
        assert_eq!(
            risk("Yes, and don't ask again for commands that start with `rm`"),
            (Some("yes_always"), Risk::Elevated)
        );
        assert_eq!(
            risk("Yes, and allow this host for this session"),
            (Some("yes_always"), Risk::Elevated)
        );
        assert_eq!(risk("Trust and continue").1, Risk::Elevated);
        assert_eq!(
            strip_key_hint("Yes, proceed (y)", Dialect::Codex),
            ("Yes, proceed", Some("y"))
        );
        // Claude menus keep a trailing letter in parentheses as text.
        assert_eq!(
            strip_key_hint("Option (b)", Dialect::Claude),
            ("Option (b)", None)
        );
        assert_eq!(Dialect::for_kind(Some("codex")), Dialect::Codex);
        assert_eq!(Dialect::for_kind(Some("claude")), Dialect::Claude);
        assert_eq!(Dialect::for_kind(None), Dialect::Claude);
    }

    #[test]
    fn ids_hash_the_content() {
        let bash = include_str!("fixtures/synthetic_bash_permission.txt");
        let amend = include_str!("fixtures/synthetic_bash_amend.txt");
        let a = parse(bash, 7);
        assert!(is_screen_id(&a.input.id, 7), "{}", a.input.id);
        assert_eq!(parse(bash, 7).input.id, a.input.id, "stable");
        assert_ne!(
            parse(amend, 7).input.id,
            a.input.id,
            "other dialog, same seq"
        );
        assert_ne!(parse(bash, 8).input.id, a.input.id, "other seq");
        // The screen tail is not part of the identity.
        let noisy = format!("spinner frame\n{bash}");
        assert_eq!(parse(&noisy, 7).input.id, a.input.id);
        let garbage = parse("no dialog here", 7);
        assert!(is_screen_id(&garbage.input.id, 7));
    }

    fn confidence(dialog: &Dialog) -> u8 {
        match &dialog.input.source {
            PendingSource::Screen { confidence, .. } => confidence.percent(),
            other => panic!("not a screen source: {other:?}"),
        }
    }

    /// `(id, risk, accepts_feedback, key)` per option.
    fn options(dialog: &Dialog) -> Vec<(String, Risk, bool, &'static str)> {
        dialog
            .input
            .options
            .as_slice()
            .iter()
            .map(|o| {
                (
                    o.id.to_string(),
                    o.risk,
                    o.accepts_feedback,
                    dialog.key_for(&o.id).unwrap(),
                )
            })
            .collect()
    }

    fn row(
        id: &str,
        risk: Risk,
        feedback: bool,
        key: &'static str,
    ) -> (String, Risk, bool, &'static str) {
        (id.to_owned(), risk, feedback, key)
    }

    struct Case {
        name: &'static str,
        screen: &'static str,
        kind: PendingKind,
        prompt: &'static str,
        detail: Option<&'static str>,
        options: Vec<(String, Risk, bool, &'static str)>,
        min_confidence: u8,
    }

    #[test]
    #[allow(clippy::too_many_lines)] // a table, one row per fixture
    fn fixtures() {
        use Risk::{Elevated, Low};
        let cases = vec![
            Case {
                name: "real Claude Code 2.1.317 bash permission, narrow pane",
                screen: include_str!("fixtures/real_claude_2_1_317_bash_permission.txt"),
                kind: PendingKind::Permission,
                prompt: "Run `touch probe-file.txt`?",
                detail: Some("Create empty probe file\ntouch probe-file.txt"),
                options: vec![
                    row("yes", Low, false, "1"),
                    row("yes_always", Elevated, false, "2"),
                    row("yes_auto_mode", Elevated, false, "3"),
                    row("no", Low, false, "4"),
                ],
                min_confidence: 100,
            },
            Case {
                name: "bash permission",
                screen: include_str!("fixtures/synthetic_bash_permission.txt"),
                kind: PendingKind::Permission,
                prompt: "Run `cargo nextest run -p bridge`?",
                detail: Some("cargo nextest run -p bridge\nRun the bridge tests"),
                options: vec![
                    row("yes", Low, false, "1"),
                    row("yes_always", Elevated, false, "2"),
                    row("no_feedback", Low, true, "esc"),
                ],
                min_confidence: 100,
            },
            Case {
                name: "bash permission wrapped in a narrow pane",
                screen: include_str!("fixtures/synthetic_bash_narrow.txt"),
                kind: PendingKind::Permission,
                prompt: "Run `git push origin it2/pending`?",
                detail: Some("git push origin it2/pending\nPush the branch"),
                options: vec![
                    row("yes", Low, false, "1"),
                    row("yes_always", Elevated, false, "2"),
                    row("no_feedback", Low, true, "esc"),
                ],
                min_confidence: 100,
            },
            Case {
                name: "bash permission with a plain No and a hint footer",
                screen: include_str!("fixtures/synthetic_bash_amend.txt"),
                kind: PendingKind::Permission,
                prompt: "Run `rm -rf target/passporter`?",
                detail: Some("rm -rf target/passporter\nRemove the scratch output"),
                options: vec![
                    row("yes", Low, false, "1"),
                    row("yes_always", Elevated, false, "2"),
                    row("no", Low, false, "3"),
                ],
                min_confidence: 100,
            },
            Case {
                name: "edit file in a bordered box",
                screen: include_str!("fixtures/synthetic_edit_file.txt"),
                kind: PendingKind::Permission,
                prompt: "Do you want to make this edit to lib.rs?",
                detail: Some(
                    "crates/bridge/src/lib.rs\n\n 14  pub mod actions;\n 15  pub mod config;\n \
                     16 +pub mod pending;\n 17  pub mod registry;",
                ),
                options: vec![
                    row("yes", Low, false, "1"),
                    row("yes_always", Elevated, false, "2"),
                    row("no_feedback", Low, true, "esc"),
                ],
                min_confidence: 100,
            },
            Case {
                name: "plan approval",
                screen: include_str!("fixtures/synthetic_plan_approval.txt"),
                kind: PendingKind::PlanApproval,
                prompt: PLAN_PROMPT,
                detail: Some(
                    "Plan: screen-parsed dialogs\n\n1. Parse permission prompts into options\n\
                     2. Serve GET /v1/agents/:id/pending\n3. Enable choose with a stale guard",
                ),
                options: vec![
                    row("yes_auto_accept", Elevated, false, "1"),
                    row("yes_manual", Low, false, "2"),
                    row("no_keep_planning", Low, false, "3"),
                ],
                min_confidence: 100,
            },
            Case {
                name: "generic question menu",
                screen: include_str!("fixtures/synthetic_question_menu.txt"),
                kind: PendingKind::Question,
                prompt: "Which storage backend should the bridge use?",
                detail: None,
                options: vec![
                    row("opt_1", Elevated, false, "1"),
                    row("opt_2", Elevated, false, "2"),
                ],
                min_confidence: 70,
            },
            Case {
                name: "secret scrubbed from the command",
                screen: include_str!("fixtures/synthetic_bash_secret_scrubbed.txt"),
                kind: PendingKind::Permission,
                prompt: "Run `curl -fsS -H \"Authorization: Bearer [REDACTED]\" https://deploy.example.invalid/v1/run`?",
                detail: Some(
                    "curl -fsS -H \"Authorization: Bearer [REDACTED]\" \
                     https://deploy.example.invalid/v1/run\nTrigger the staging deploy",
                ),
                options: vec![
                    row("yes", Low, false, "1"),
                    row("no_feedback", Low, true, "esc"),
                ],
                min_confidence: 100,
            },
        ];
        for case in cases {
            let dialog = parse(case.screen, id());
            let name = case.name;
            assert_eq!(dialog.input.kind, case.kind, "{name}");
            assert_eq!(dialog.input.prompt, case.prompt, "{name}");
            assert_eq!(dialog.input.detail.as_deref(), case.detail, "{name}");
            assert_eq!(options(&dialog), case.options, "{name}");
            assert!(
                confidence(&dialog) >= case.min_confidence,
                "{name}: {}",
                confidence(&dialog)
            );
            assert!(dialog.input.is_dialog(), "{name}");
            assert!(is_screen_id(&dialog.input.id, id()), "{name}");
        }
    }

    #[test]
    fn real_2_1_317_labels() {
        let dialog = parse(
            include_str!("fixtures/real_claude_2_1_317_bash_permission.txt"),
            id(),
        );
        let labels: Vec<&str> = dialog
            .input
            .options
            .as_slice()
            .iter()
            .map(|o| o.label.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "Yes",
                "Yes, and always allow access to /home/user/widget-workspaces from this project",
                "Yes, and switch to auto mode",
                "No",
            ]
        );
    }

    #[test]
    fn unrecognized_labels_are_shown_in_full() {
        let dialog = parse(include_str!("fixtures/synthetic_question_menu.txt"), id());
        assert_eq!(
            dialog.input.options.as_slice()[0].label,
            "SQLite Embedded, single file, no server"
        );
        let long = "word ".repeat(40);
        let screen = format!(
            "Which one?\n❯ 1. Keep the cache · it is warm and {long}\n  2. Drop it · cold start\n"
        );
        let dialog = parse(&screen, id());
        let labels: Vec<&str> = dialog
            .input
            .options
            .as_slice()
            .iter()
            .map(|o| o.label.as_str())
            .collect();
        assert!(labels[0].starts_with("Keep the cache · it is warm and word word"));
        assert!(labels[0].chars().count() > MAX_LABEL_CHARS);
        assert!(labels[0].chars().count() <= MAX_FREE_LABEL_CHARS);
        assert_eq!(labels[1], "Drop it · cold start");
    }

    #[test]
    fn tips_are_not_detail() {
        let lines: Vec<Line> = [
            "Tip: auto mode handles these prompts",
            "for you — choose \"switch to auto",
            "mode\" below",
            "Create empty probe file",
            "Tip: one line",
            "",
            "touch x",
        ]
        .iter()
        .map(|l| Line::new(l))
        .collect();
        assert_eq!(
            detail(&lines).as_deref(),
            Some("Create empty probe file\n\ntouch x")
        );
    }

    #[test]
    fn labels_drop_key_hints_and_keep_wrapped_text() {
        let dialog = parse(include_str!("fixtures/synthetic_bash_narrow.txt"), id());
        let labels: Vec<&str> = dialog
            .input
            .options
            .as_slice()
            .iter()
            .map(|o| o.label.as_str())
            .collect();
        assert_eq!(labels[0], "Yes");
        assert!(
            labels[1].starts_with("Yes, and don't ask again for git push"),
            "{}",
            labels[1]
        );
        assert_eq!(labels[2], "No, and tell Claude what to do differently");
        let edit = parse(include_str!("fixtures/synthetic_edit_file.txt"), id());
        assert_eq!(
            edit.input.options.as_slice()[1].label,
            "Yes, allow all edits during this session"
        );
    }

    #[test]
    fn output_that_is_not_a_live_dialog_gets_no_options() {
        let dialog = parse(include_str!("fixtures/synthetic_not_a_dialog.txt"), id());
        assert_eq!(dialog.input.options, PendingOptions::default());
        assert_eq!(dialog.input.kind, PendingKind::Unknown);
        assert!(confidence(&dialog) < MIN_CONFIDENCE);
        assert!(dialog.input.is_dialog(), "still a dialog for policy");
        let PendingSource::Screen { screen_tail, .. } = &dialog.input.source else {
            panic!("screen source");
        };
        assert_eq!(
            screen_tail.lines().last().map(String::as_str),
            Some("  ⏵⏵ auto mode on")
        );
    }

    #[test]
    fn garbage_and_empty_screens_get_no_options() {
        for screen in [
            "",
            "\n\n\n",
            "just some output\nwith lines",
            "❯ 1. Yes",
            "Proceed?\n❯ 1. Yes\n  3. No",
            "Proceed?\n❯ 1. Yes\n❯ 2. No",
            "Proceed?\n  1. Yes\n  2. No",
            "Proceed?\n❯ 1. Yes\n\n  2. No",
            "\u{1b}[2J\u{202e}❯ 1. Yes\u{0}",
        ] {
            let dialog = parse(screen, id());
            assert_eq!(
                dialog.input.options,
                PendingOptions::default(),
                "{screen:?}"
            );
            assert_eq!(dialog.input.kind, PendingKind::Unknown, "{screen:?}");
            assert!(confidence(&dialog) < MIN_CONFIDENCE, "{screen:?}");
        }
    }

    #[test]
    fn a_menu_with_output_below_it_is_history() {
        let screen = format!(
            "{}\n⏺ Bash(cargo nextest run -p bridge)\n  ⎿  42 passed\n",
            include_str!("fixtures/synthetic_bash_permission.txt")
        );
        assert_eq!(
            parse(&screen, id()).input.options,
            PendingOptions::default()
        );
    }

    #[test]
    fn more_than_nine_options_are_not_offered() {
        let screen = "Pick one?\n❯ 1. a\n  2. b\n  3. c\n  4. d\n  5. e\n  6. f\n  7. g\n  \
                      8. h\n  9. i\n  10. j\n";
        assert_eq!(parse(screen, id()).input.options, PendingOptions::default());
    }

    #[test]
    fn risk_classification() {
        use Risk::{Elevated, Low};
        let risk = |label: &str| match classify(label, false, Dialect::Claude) {
            Class::Option { risk, .. } => risk,
            Class::TextEntry => panic!("{label} is a text entry"),
        };
        for (label, expected) in [
            ("Yes", Low),
            ("yes, proceed", Low),
            ("Yes, and manually approve edits", Low),
            ("No", Low),
            ("No, keep planning", Low),
            ("No, and tell Claude what to do differently", Low),
            ("No, something else", Low),
            ("Yes, and don't ask again for git commands", Elevated),
            ("Yes, and don\u{2019}t ask again", Elevated),
            ("Yes, allow all edits during this session", Elevated),
            ("Yes, and auto-accept edits", Elevated),
            ("Yes, and bypass permissions", Elevated),
            ("Yes, clear context and auto-accept edits", Elevated),
            ("Always allow", Elevated),
            ("No, and always deny", Elevated),
            ("SQLite", Elevated),
            ("Yes please", Elevated),
            ("Maybe", Elevated),
        ] {
            assert_eq!(risk(label), expected, "{label}");
        }
        assert_eq!(
            classify("Type something.", false, Dialect::Claude),
            Class::TextEntry
        );
        assert_eq!(
            classify(
                "Type here to tell Claude what to change",
                false,
                Dialect::Claude
            ),
            Class::TextEntry
        );
    }

    #[test]
    fn feedback_needs_the_esc_path() {
        let feedback = |esc| match classify(
            "No, and tell Claude what to do differently",
            esc,
            Dialect::Claude,
        ) {
            Class::Option {
                accepts_feedback, ..
            } => accepts_feedback,
            Class::TextEntry => panic!("not a text entry"),
        };
        assert!(feedback(true));
        assert!(!feedback(false));
    }

    #[test]
    fn key_hints() {
        assert_eq!(
            strip_key_hint("No (esc)", Dialect::Claude),
            ("No", Some("esc"))
        );
        assert_eq!(
            strip_key_hint("Yes (shift+tab)", Dialect::Claude),
            ("Yes", Some("shift+tab"))
        );
        assert_eq!(
            strip_key_hint("Yes (for now)", Dialect::Claude),
            ("Yes (for now)", None)
        );
        assert_eq!(strip_key_hint("Yes", Dialect::Claude), ("Yes", None));
    }

    #[test]
    fn long_text_is_bounded() {
        let long = "x".repeat(500);
        let screen = format!("Do you want to proceed {long}?\n❯ 1. Yes {long}\n  2. No\n");
        let dialog = parse(&screen, id());
        assert!(dialog.input.prompt.chars().count() <= MAX_PROMPT_CHARS);
        for option in dialog.input.options.as_slice() {
            assert!(option.label.chars().count() <= MAX_FREE_LABEL_CHARS);
        }
    }

    /// Fixtures must never carry real secrets: scan them for token shapes.
    #[test]
    fn fixtures_contain_nothing_secret_looking() {
        let fixtures = [
            include_str!("fixtures/synthetic_bash_permission.txt"),
            include_str!("fixtures/synthetic_bash_narrow.txt"),
            include_str!("fixtures/synthetic_bash_amend.txt"),
            include_str!("fixtures/synthetic_edit_file.txt"),
            include_str!("fixtures/synthetic_plan_approval.txt"),
            include_str!("fixtures/synthetic_question_menu.txt"),
            include_str!("fixtures/synthetic_not_a_dialog.txt"),
            include_str!("fixtures/synthetic_bash_secret_scrubbed.txt"),
            include_str!("fixtures/real_claude_2_1_317_bash_permission.txt"),
        ];
        for fixture in fixtures {
            for word in fixture.split(|c: char| c.is_whitespace() || c == '"') {
                let lower = word.to_lowercase();
                assert!(
                    !["sk-", "ghp_", "gho_", "github_pat_", "akia", "xox"]
                        .iter()
                        .any(|prefix| lower.starts_with(prefix)),
                    "secret-looking word {word:?}"
                );
                let run = word.chars().filter(char::is_ascii_alphanumeric).count();
                assert!(
                    run < 32 || word.contains('/'),
                    "long token-like word {word:?}"
                );
            }
        }
    }

    use super::super::hook::Shown;

    #[test]
    fn a_question_hook_keeps_the_screen_detail() {
        let mut screen = parse(include_str!("fixtures/synthetic_question_menu.txt"), 7);
        screen.input.detail = Some("Storage".to_owned());
        let payload = serde_json::json!({
            "session_id": "s", "hook_event_name": "PermissionRequest",
            "tool_name": "AskUserQuestion",
            "tool_input": {"questions": [{"question": "Which storage backend should the bridge use?"}]},
        });
        let super::super::hook::HookEventKind::Permission(hook) =
            super::super::hook::decode(payload.to_string().as_bytes())
                .expect("decodes")
                .kind
        else {
            unreachable!()
        };
        assert_eq!(
            hook.describes(&Shown::new(&screen.input)),
            Some(Match::Exact)
        );
        screen.describe_with(&hook, Match::Exact);
        assert_eq!(screen.input.detail.as_deref(), Some("Storage"));
    }

    fn nextest() -> HookPermission {
        hook(
            "Bash",
            &serde_json::json!({"command": "cargo nextest run -p bridge", "description": "Run the bridge tests"}),
        )
    }

    #[test]
    fn a_hook_matches_the_parsed_fixture_and_keeps_the_screen_dialog() {
        let screen = parse(include_str!("fixtures/synthetic_bash_permission.txt"), 7);
        let hook = nextest();
        assert_eq!(
            hook.describes(&Shown::new(&screen.input)),
            Some(Match::Exact),
            "{:?}",
            screen.input.detail
        );
        let mut described = screen.clone();
        described.describe_with(&hook, Match::Exact);
        assert_eq!(described.input.prompt, "Run `cargo nextest run -p bridge`?");
        assert_eq!(
            described.input.detail.as_deref(),
            Some("cargo nextest run -p bridge\nRun the bridge tests")
        );
        assert_eq!(described.input.id, screen.input.id, "the screen's id");
        assert_eq!(described.input.kind, screen.input.kind);
        assert_eq!(described.input.options, screen.input.options);
        assert_eq!(
            described.input.source, screen.input.source,
            "still screen-sourced"
        );
        for option in screen.input.options.as_slice() {
            assert_eq!(described.key_for(&option.id), screen.key_for(&option.id));
        }
        // Policy, and only policy, sees that a hook described it, and only
        // when the hook matched the whole dialog.
        assert_eq!(described.policy_input().source, PendingSource::Hook);
        let mut prefix = screen.clone();
        prefix.describe_with(&hook, Match::Prefix);
        assert_eq!(
            prefix.policy_input().source,
            screen.input.source,
            "a prefix match is not enough for the car"
        );
        assert_eq!(
            screen.policy_input().source,
            screen.input.source,
            "undescribed stays screen for policy too"
        );
    }

    #[test]
    fn ids_say_which_seq_they_were_parsed_at() {
        let dialog = parse(include_str!("fixtures/synthetic_bash_permission.txt"), 7);
        assert!(parsed_at(&dialog.input.id, 7));
        assert!(!parsed_at(&dialog.input.id, 8));
        assert!(!parsed_at(&dialog.input.id, 77));
        assert!(!parsed_at(&PendingId::new("screen-7").expect("id"), 7));
    }

    #[test]
    fn a_hook_does_not_match_other_fixtures() {
        let hook = nextest();
        for screen in [
            include_str!("fixtures/synthetic_bash_amend.txt"),
            include_str!("fixtures/synthetic_question_menu.txt"),
            include_str!("fixtures/synthetic_plan_approval.txt"),
            include_str!("fixtures/synthetic_not_a_dialog.txt"),
        ] {
            assert_eq!(hook.describes(&Shown::new(&parse(screen, 7).input)), None);
        }
    }

    #[test]
    fn an_unparsed_dialog_stays_an_unknown_screen_dialog() {
        let mut screen = parse(include_str!("fixtures/synthetic_not_a_dialog.txt"), 7);
        assert_eq!(screen.input.options.as_slice(), &[]);
        screen.input.detail = Some("cargo nextest run -p bridge".to_owned());
        let hook = nextest();
        assert_eq!(
            hook.describes(&Shown::new(&screen.input)),
            Some(Match::Exact)
        );
        let mut described = screen.clone();
        described.describe_with(&hook, Match::Exact);
        assert_eq!(described.input.kind, PendingKind::Unknown);
        assert_eq!(described.input.source, screen.input.source);
        assert!(described.input.is_dialog());
    }

    /// The permission a `PermissionRequest` for `tool` with `input` announces.
    fn hook(tool: &str, input: &serde_json::Value) -> HookPermission {
        let payload = serde_json::json!({
            "session_id": "s", "hook_event_name": "PermissionRequest",
            "tool_name": tool, "tool_input": input,
        });
        match super::super::hook::decode(payload.to_string().as_bytes())
            .expect("decodes")
            .kind
        {
            super::super::hook::HookEventKind::Permission(permission) => permission,
            super::super::hook::HookEventKind::Ignored => unreachable!(),
        }
    }

    /// `text` wrapped at spaces into lines of at most `width` characters,
    /// each starting with `indent`, as Claude draws a long command.
    fn wrapped(text: &str, width: usize, indent: &str) -> String {
        let mut lines = vec![String::new()];
        for word in text.split(' ') {
            let line = lines.last_mut().expect("a line");
            if !line.is_empty() && line.len() + 1 + word.len() > width {
                lines.push(word.to_owned());
            } else {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
        }
        lines
            .iter()
            .map(|line| format!("{indent}{line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_long_command_the_parser_cut_gets_the_hook_wording() {
        let command = (1..=100)
            .map(|i| format!("echo step-{i:03}"))
            .collect::<Vec<_>>()
            .join(" && ");
        let screen = format!(
            "{}\n Bash command\n\n{}\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No, and tell Claude what to do differently (esc)\n",
            "─".repeat(80),
            wrapped(&command, 70, "   "),
        );
        let screen = parse(&screen, 7);
        let shown = screen.input.detail.as_deref().expect("detail");
        assert!(shown.ends_with('…'), "the parser cut it: {shown}");
        let request = hook("Bash", &serde_json::json!({"command": command}));
        assert_eq!(
            request.describes(&Shown::new(&screen.input)),
            Some(Match::Prefix)
        );
        let mut described = screen.clone();
        described.describe_with(&request, Match::Exact);
        assert_eq!(described.input.detail.as_deref(), Some(command.as_str()));
        assert_eq!(described.input.id, screen.input.id);

        // A command that differs only after the cut also fits the cut
        // text, which is why a prefix match must be the only one stored.
        let other = format!("{command} && rm -rf ./build");
        let other = hook("Bash", &serde_json::json!({"command": other}));
        assert_eq!(
            other.describes(&Shown::new(&screen.input)),
            Some(Match::Prefix)
        );
        // One that differs before the cut does not.
        let before = command.replacen("step-005", "step-500", 1);
        let before = hook("Bash", &serde_json::json!({"command": before}));
        assert_eq!(before.describes(&Shown::new(&screen.input)), None);
    }

    #[test]
    fn a_long_plan_the_parser_cut_gets_the_hook_wording() {
        let steps: Vec<String> = (1..=30)
            .map(|i| format!("{i}. **Step {i}**: change module {i} and test it"))
            .collect();
        let plan = format!("# Plan: a long one\n\n{}", steps.join("\n"));
        let rendered = std::iter::once("Plan: a long one".to_owned())
            .chain(std::iter::once(String::new()))
            .chain(steps.iter().map(|step| step.replace("**", "")))
            .map(|line| format!("│ {line:<72} │"))
            .collect::<Vec<_>>()
            .join("\n");
        let screen = format!(
            " Ready to code?\n\n Here is Claude's plan:\n╭{rule}╮\n{rendered}\n╰{rule}╯\n\n Would you like to proceed?\n\n ❯ 1. Yes, and auto-accept edits\n   2. Yes, and manually approve edits\n   3. No, keep planning\n",
            rule = "─".repeat(74),
        );
        let screen = parse(&screen, 7);
        assert_eq!(screen.input.kind, PendingKind::PlanApproval);
        let shown = screen.input.detail.as_deref().expect("detail");
        assert!(shown.ends_with('…'), "the parser cut it: {shown}");
        let request = hook("ExitPlanMode", &serde_json::json!({"plan": plan}));
        assert_eq!(
            request.describes(&Shown::new(&screen.input)),
            Some(Match::Prefix)
        );
        let mut described = screen.clone();
        described.describe_with(&request, Match::Exact);
        assert_eq!(described.input.detail.as_deref(), Some(plan.as_str()));

        let edited = plan.replacen("module 3 ", "module 33 ", 1);
        let edited = hook("ExitPlanMode", &serde_json::json!({"plan": edited}));
        assert_eq!(edited.describes(&Shown::new(&screen.input)), None);
    }
}
