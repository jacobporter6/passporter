//! Command candidates in an agent's message: deterministic, rule-based
//! extraction, no model involved.
//!
//! The aim is reliable recall without silent picking: every plausible
//! command the message asks the user to run is returned, in message order,
//! so when the agent offers "run this, or this" every option shows and the
//! user picks. Prose mentions of commands the agent ran itself ("I ran
//! `cargo test`") are not asked of the user and are left out.
//!
//! The message is cleaned first: ANSI escape sequences, carriage returns,
//! control characters (other than line breaks; tabs become spaces) and
//! hidden characters (bidi controls, zero-width marks, joiners) are
//! removed, so the text a candidate shows is exactly the text that runs.
//!
//! A candidate comes from one of three places:
//!
//! - **A fenced code block** (```` ``` ```` or `~~~`), shown whole. A block
//!   labelled with a shell (`sh`, `bash`, `zsh`, `shell`, `fish`, …) always
//!   counts; a `console` (or `shell-session`) block counts by its `$ `
//!   lines, its output dropped; a block labelled with any other language
//!   (`rust`, `json`, `text`, …) never counts; an unlabelled block counts
//!   when every line reads like a shell command and either the first starts
//!   with a well-known command or the text before the block asks to run
//!   it. Prompt markers (`$ `) and comment lines are dropped. Only a block
//!   can give a multi-line candidate.
//! - **An inline code span**, when it is `!`-prefixed (`` `! gcloud auth
//!   login` ``), or when its sentence (or the line it is listed under, one
//!   ending in `:`) asks the user to run something ("run", "execute",
//!   "type", "paste", "try", … but not "I'll run" or "I ran") or says where
//!   ("in another terminal", "in this session"), and the span reads like a
//!   command (not a path, an identifier or code).
//! - **A line starting with `! `** (or `!` and a well-known command): Claude
//!   Code's shell-escape syntax, written as prose.
//!
//! Intent ([`CommandIntent`]): a `!` prefix is always in-session. Otherwise
//! the nearest cue in the candidate's sentence decides, else the line it is
//! listed under, else (for a block) the paragraph before it: "in another
//! terminal", "separately", "in your terminal", … mean a new terminal; "in
//! this session", "run it here", "so the output lands here", … mean
//! in-session; no cue leaves it unspecified.
//!
//! Candidates longer than [`MAX_COMMAND_CHARS`] or [`MAX_COMMAND_LINES`]
//! are never cut (a cut command is a different command): they are counted
//! as omitted, like any past [`MAX_CANDIDATES`]. A command found twice is
//! listed once, where it first appears, with the first specific intent.

use protocol::{
    CommandIntent, CommandSource, MAX_CANDIDATES, MAX_COMMAND_CHARS, MAX_COMMAND_LINES,
};

/// A command found in a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Extracted {
    /// The exact text to run (no `!` prefix).
    pub(crate) text: String,
    /// What the wording around it says.
    pub(crate) intent: CommandIntent,
    /// Where it was found.
    pub(crate) source: CommandSource,
}

/// What [`extract`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Extraction {
    /// The candidates, in message order, at most [`MAX_CANDIDATES`].
    pub(crate) candidates: Vec<Extracted>,
    /// Candidates left out: too long, or past [`MAX_CANDIDATES`].
    pub(crate) omitted: u32,
}

/// Every command candidate in `message` (Markdown), as the
/// [module docs](self) describe.
#[must_use]
pub(crate) fn extract(message: &str) -> Extraction {
    let clean = clean(message);
    let mut found = Found::default();
    let mut prose = Prose::default();
    let mut lines = clean.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(fence) = Fence::open(line) {
            let mut body = Vec::new();
            for next in lines.by_ref() {
                if fence.closes(next) {
                    break;
                }
                body.push(next);
            }
            if let Some(candidate) = block(fence.info, &body, &prose) {
                found.push(candidate);
            }
            prose.end_paragraph();
            continue;
        }
        if line.trim().is_empty() {
            prose.end_paragraph();
            continue;
        }
        prose_line(line, &mut prose, &mut found);
    }
    found.finish()
}

/// Candidates gathered so far.
#[derive(Debug, Default)]
struct Found {
    all: Vec<Extracted>,
    too_long: u32,
}

impl Found {
    fn push(&mut self, candidate: Extracted) {
        let lines = candidate.text.lines().count();
        if candidate.text.chars().count() > MAX_COMMAND_CHARS || lines > MAX_COMMAND_LINES {
            self.too_long += 1;
            return;
        }
        if let Some(earlier) = self.all.iter_mut().find(|c| c.text == candidate.text) {
            if earlier.intent == CommandIntent::Unspecified {
                earlier.intent = candidate.intent;
            }
            return;
        }
        self.all.push(candidate);
    }

    fn finish(mut self) -> Extraction {
        let past = self.all.len().saturating_sub(MAX_CANDIDATES);
        self.all.truncate(MAX_CANDIDATES);
        Extraction {
            candidates: self.all,
            omitted: self
                .too_long
                .saturating_add(u32::try_from(past).unwrap_or(u32::MAX)),
        }
    }
}

/// The prose read so far, for the context of what follows.
#[derive(Debug, Default)]
struct Prose {
    /// The current paragraph's lines, or the last one's if a block follows
    /// it directly.
    paragraph: Vec<String>,
    /// Whether `paragraph` has ended (a blank line or a block).
    ended: bool,
    /// The line a list is introduced by (one ending in `:`), while its
    /// items follow.
    lead_in: Option<String>,
}

impl Prose {
    fn end_paragraph(&mut self) {
        self.ended = true;
    }

    fn push(&mut self, line: &str) {
        if self.ended {
            self.paragraph.clear();
            self.ended = false;
        }
        self.paragraph.push(line.to_owned());
    }

    /// The text before a block: the paragraph just read.
    fn before_block(&self) -> String {
        self.paragraph.join("\n")
    }
}

/// One prose line: a `!` line, or inline code spans.
fn prose_line(line: &str, prose: &mut Prose, found: &mut Found) {
    let (item, body) = list_item(line);
    if !item {
        prose.lead_in = None;
    }
    if let Some(command) = bang_line(body) {
        found.push(Extracted {
            text: command,
            intent: CommandIntent::InSession,
            source: CommandSource::BangLine,
        });
    } else {
        let spans = code_spans(body);
        let masked = mask(body, &spans);
        for span in &spans {
            if let Some(candidate) = inline(span, &masked, item, prose.lead_in.as_deref()) {
                found.push(candidate);
            }
        }
    }
    if !item && body.trim_end().ends_with(':') {
        prose.lead_in = Some(mask(body, &code_spans(body)));
    }
    prose.push(body);
}

/// Strips a list marker (`- `, `* `, `+ `, `1. `, `1) `) or a quote marker
/// (`> `): whether it was a list item, and the rest.
fn list_item(line: &str) -> (bool, &str) {
    let trimmed = line.trim_start().trim_start_matches('>').trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return (true, rest);
        }
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let rest = &trimmed[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return (true, rest);
        }
    }
    (false, trimmed)
}

/// `! cmd` (or `!cmd` with a well-known command, or `` ! `cmd` ``) as a line
/// of prose: the command.
fn bang_line(body: &str) -> Option<String> {
    let rest = body.trim().strip_prefix('!')?;
    let spaced = rest.starts_with(' ');
    let rest = rest.trim();
    let rest = match rest.strip_prefix('`').and_then(|r| r.strip_suffix('`')) {
        Some(inner) if !inner.contains('`') => inner.trim(),
        _ => rest,
    };
    let plausible = looks_like_command(rest)
        && (spaced || first_word(rest).is_some_and(|w| KNOWN_COMMANDS.contains(&w)));
    plausible.then(|| rest.to_owned())
}

/// A code span in a line: its byte range (backticks included) and text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
    text: String,
}

/// The inline code spans of `line` (`CommonMark`: a run of N backticks up to
/// the next run of exactly N; one space each side is padding).
fn code_spans(line: &str) -> Vec<Span> {
    let bytes = line.as_bytes();
    let run_at = |at: usize| bytes[at..].iter().take_while(|&&b| b == b'`').count();
    let mut spans = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'`' {
            at += 1;
            continue;
        }
        let open = run_at(at);
        let inner = at + open;
        let mut close = inner;
        let mut matched = None;
        while close < bytes.len() {
            if bytes[close] == b'`' {
                let run = run_at(close);
                if run == open {
                    matched = Some(close);
                    break;
                }
                close += run;
            } else {
                close += 1;
            }
        }
        let Some(close) = matched else {
            at = inner;
            continue;
        };
        let raw = &line[inner..close];
        let padded = raw.len() >= 2 && raw.starts_with(' ') && raw.ends_with(' ');
        let text = if padded && !raw.trim().is_empty() {
            &raw[1..raw.len() - 1]
        } else {
            raw
        };
        spans.push(Span {
            start: at,
            end: close + open,
            text: text.to_owned(),
        });
        at = close + open;
    }
    spans
}

/// `line` with every span blanked out (same byte offsets, ASCII
/// lowercase): a span becomes a `` ` `` and spaces, so cues and sentence
/// ends are only found in prose, and [`tokens`] still sees where spans
/// were.
fn mask(line: &str, spans: &[Span]) -> String {
    let mut out = String::with_capacity(line.len());
    for (i, c) in line.char_indices() {
        match spans.iter().find(|s| (s.start..s.end).contains(&i)) {
            Some(span) if span.start == i => out.push('`'),
            Some(_) => out.extend(std::iter::repeat_n(' ', c.len_utf8())),
            None => out.push(c.to_ascii_lowercase()),
        }
    }
    out
}

/// An inline span as a candidate, judged by its sentence in `masked` (its
/// line, spans blanked), whether it opens a list item, and the line its
/// list is introduced by.
fn inline(span: &Span, masked: &str, item: bool, lead_in: Option<&str>) -> Option<Extracted> {
    let text = span.text.trim();
    if let Some(rest) = text.strip_prefix('!') {
        let spaced = rest.starts_with(' ');
        let rest = rest.trim();
        let known = first_word(rest).is_some_and(|w| KNOWN_COMMANDS.contains(&w));
        let multi = rest.split_whitespace().nth(1).is_some();
        return (looks_like_command(rest) && (spaced || known || multi)).then(|| Extracted {
            text: rest.to_owned(),
            intent: CommandIntent::InSession,
            source: CommandSource::Inline,
        });
    }
    if !looks_like_command(text) {
        return None;
    }
    let (from, to) = sentence(masked, span.start, span.end);
    let own = &masked[from..to];
    let at = (span.start - from, span.end - from);
    let before = tokens(&own[..at.0]);
    let cue = nearest_cue(own, at);
    let strong = is_strong(text);
    let opens_item = item && before.iter().all(|t| matches!(t, Tok::Punct));
    let listed = lead_in.filter(|lead| {
        opens_item && (asks_anywhere(&tokens(lead)) || nearest_cue(lead, end(lead)).is_some())
    });
    let asked = asks_for_span(&before) || (strong && placed_by_cue(own, at)) || listed.is_some();
    if !asked {
        return None;
    }
    let intent = cue
        .or_else(|| listed.and_then(|lead| nearest_cue(lead, end(lead))))
        .unwrap_or(CommandIntent::Unspecified);
    Some(Extracted {
        text: text.to_owned(),
        intent,
        source: CommandSource::Inline,
    })
}

/// Whether `text` is a well-known command or an executable path with
/// arguments: enough, with a cue right by it saying where to run it, to
/// count without a verb.
fn is_strong(text: &str) -> bool {
    let multi = text.split_whitespace().nth(1).is_some();
    multi
        && first_word(text).is_some_and(|w| {
            KNOWN_COMMANDS.contains(&w) || w.starts_with("./") || w.starts_with("../")
        })
}

/// A zero-width position at the end of `text`, for "the cue nearest to
/// what follows".
fn end(text: &str) -> (usize, usize) {
    (text.len(), text.len())
}

/// The byte range of the sentence holding `start..end` in `masked`: up to
/// the nearest `.`, `?`, `!` or `;` followed (past closing `*`, `_`, `)`,
/// quotes) by a space, the nearest `|` (a table cell), or the line's
/// ends. Spans are blanked, so their punctuation never counts.
fn sentence(masked: &str, start: usize, end: usize) -> (usize, usize) {
    let bytes = masked.as_bytes();
    let is_end = |i: usize| {
        if bytes[i] == b'|' {
            return true;
        }
        if !matches!(bytes[i], b'.' | b'?' | b'!' | b';') {
            return false;
        }
        let after = bytes[i + 1..]
            .iter()
            .find(|b| !matches!(b, b'*' | b'_' | b')' | b'"' | b'\''));
        after.is_none_or(u8::is_ascii_whitespace)
    };
    let from = (0..start).rev().find(|&i| is_end(i)).map_or(0, |i| i + 1);
    let to = (end..bytes.len())
        .find(|&i| is_end(i))
        .map_or(bytes.len(), |i| i + 1);
    (from, to)
}

/// A token of masked prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    /// A word: letters, digits and `'`.
    Word(&'a str),
    /// Where a code span was.
    Span,
    /// Any other visible character.
    Punct,
}

/// The tokens of `masked` prose.
fn tokens(masked: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in masked.char_indices() {
        let part = c.is_alphanumeric() || c == '\'';
        if let (false, Some(s)) = (part, start) {
            out.push(Tok::Word(&masked[s..i]));
            start = None;
        }
        match c {
            _ if part => start = start.or(Some(i)),
            '`' => out.push(Tok::Span),
            c if c.is_whitespace() => {}
            _ => out.push(Tok::Punct),
        }
    }
    if let Some(s) = start {
        out.push(Tok::Word(&masked[s..]));
    }
    out
}

/// Verbs that ask for a command to be run.
const RUN_VERBS: &[&str] = &["run", "rerun", "execute", "type", "paste", "try", "enter"];

/// Words that may stand between the start of a clause and a run verb
/// that asks the user: "please run", "then run", "you can run", "you'll
/// need to run". Anything else there ("I'll run", "don't run", "the dry
/// run", "every run") means the verb does not ask the user.
const IMPERATIVE_LEADS: &[&str] = &[
    "please", "then", "just", "now", "first", "next", "and", "or", "also", "simply", "so", "you",
    "you'll", "you'd", "can", "could", "should", "must", "need", "needs", "to", "will", "would",
    "want", "may", "might", "how", "manually", "yourself", "again", "re", "finally", "instead",
];

/// Words that may stand between a run verb and the span it asks for, or
/// between spans it asks for: "run either `a` or `b`", "run this: `a`".
const SPAN_LINKS: &[&str] = &[
    "and",
    "or",
    "then",
    "plus",
    "followed",
    "by",
    "either",
    "this",
    "these",
    "the",
    "following",
    "command",
    "commands",
    "one",
    "of",
    "both",
    "it",
    "them",
    "once",
    "again",
    "now",
    "just",
];

/// Whether the run verb at `tokens[verb]` asks the user: preceded, within
/// its clause, only by [`IMPERATIVE_LEADS`].
fn imperative(tokens: &[Tok<'_>], verb: usize) -> bool {
    let mut at = verb;
    while let Some(Tok::Word(word)) = at.checked_sub(1).map(|i| tokens[i]) {
        if !IMPERATIVE_LEADS.contains(&word) {
            return false;
        }
        at -= 1;
    }
    true
}

/// Whether the prose before a span (`tokens`) asks to run it: walking
/// back over other spans and [`SPAN_LINKS`] reaches an
/// [imperative](imperative) run verb.
fn asks_for_span(tokens: &[Tok<'_>]) -> bool {
    let mut at = tokens.len();
    while let Some(token) = at.checked_sub(1).map(|i| tokens[i]) {
        match token {
            Tok::Span | Tok::Punct => {}
            Tok::Word(word) if SPAN_LINKS.contains(&word) => {}
            Tok::Word(word) => return RUN_VERBS.contains(&word) && imperative(tokens, at - 1),
        }
        at -= 1;
    }
    false
}

/// Whether `tokens` hold an [imperative](imperative) run verb anywhere:
/// the text before a block or a list asks to run what follows.
fn asks_anywhere(tokens: &[Tok<'_>]) -> bool {
    tokens.iter().enumerate().any(|(i, token)| {
        matches!(token, Tok::Word(word) if RUN_VERBS.contains(word)) && imperative(tokens, i)
    })
}

/// The words of `prose` with their byte offsets (letters, digits and `'`).
fn words(prose: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in prose.char_indices() {
        let part = c.is_alphanumeric() || c == '\'';
        match (part, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push((s, &prose[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &prose[s..]));
    }
    out
}

/// Phrases that send a command to a terminal of its own.
const NEW_TERMINAL_CUES: &[&str] = &[
    "another terminal",
    "separate terminal",
    "new terminal",
    "second terminal",
    "other terminal",
    "different terminal",
    "another shell",
    "separate shell",
    "new shell",
    "another tab",
    "separate tab",
    "new tab",
    "another window",
    "separate window",
    "separately",
    "outside this session",
    "outside of this session",
    "outside the session",
    "outside claude",
    "outside codex",
    "in your terminal",
    "in your shell",
    "in a terminal",
    "in a shell",
    "your own terminal",
    "in the background",
];

/// Phrases that keep a command in the agent's session.
const IN_SESSION_CUES: &[&str] = &[
    "in this session",
    "in the session",
    "in our session",
    "into this session",
    "this conversation",
    "in this chat",
    "in the prompt",
    "at the prompt",
    "in this prompt",
    "lands here",
    "land here",
    "show up here",
    "shows up here",
    "prefixed with",
    "prefix it with",
];

/// A cue in prose: where it is, what it means, and whether it is a phrase
/// (rather than a bare "here").
#[derive(Debug, Clone, Copy)]
struct Cue {
    start: usize,
    stop: usize,
    intent: CommandIntent,
    phrase: bool,
}

impl Cue {
    /// Bytes between the cue and `at` (a byte range).
    fn distance(&self, at: (usize, usize)) -> usize {
        if self.stop <= at.0 {
            at.0 - self.stop
        } else {
            self.start.saturating_sub(at.1)
        }
    }
}

/// Every cue in `prose` (lowercase, spans blanked): the phrases, and
/// "here" as a word of its own but not "here is"/"here are" (which
/// introduce, not place).
fn cues(prose: &str) -> Vec<Cue> {
    let mut found = Vec::new();
    for (phrases, intent) in [
        (NEW_TERMINAL_CUES, CommandIntent::NewTerminal),
        (IN_SESSION_CUES, CommandIntent::InSession),
    ] {
        for phrase in phrases {
            for (i, _) in prose.match_indices(phrase) {
                found.push(Cue {
                    start: i,
                    stop: i + phrase.len(),
                    intent,
                    phrase: true,
                });
            }
        }
    }
    let all = words(prose);
    for (n, (i, word)) in all.iter().enumerate() {
        let next = all.get(n + 1).map(|(_, w)| *w);
        if *word == "here" && !matches!(next, Some("is" | "are")) {
            found.push(Cue {
                start: *i,
                stop: i + word.len(),
                intent: CommandIntent::InSession,
                phrase: false,
            });
        }
    }
    found
}

/// The intent of the cue nearest to `at` (a byte range) in `prose`.
fn nearest_cue(prose: &str, at: (usize, usize)) -> Option<CommandIntent> {
    cues(prose)
        .into_iter()
        .min_by_key(|cue| cue.distance(at))
        .map(|cue| cue.intent)
}

/// Most bytes between a span and a phrase cue that, on its own, makes the
/// span a command to run ("`npm run dev` in another terminal").
const CUE_REACH: usize = 40;

/// Whether a phrase cue lies within [`CUE_REACH`] of `at` in `prose`.
fn placed_by_cue(prose: &str, at: (usize, usize)) -> bool {
    cues(prose)
        .iter()
        .any(|cue| cue.phrase && cue.distance(at) <= CUE_REACH)
}

/// A fenced code block's opening line.
#[derive(Debug)]
struct Fence<'a> {
    marker: u8,
    len: usize,
    info: &'a str,
}

impl<'a> Fence<'a> {
    /// `line` as an opening fence: at most three spaces of indent, then
    /// three or more backticks or tildes, then the info string.
    fn open(line: &'a str) -> Option<Self> {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let rest = &line[indent..];
        let marker = *rest.as_bytes().first()?;
        if indent > 3 || !matches!(marker, b'`' | b'~') {
            return None;
        }
        let len = rest.bytes().take_while(|&b| b == marker).count();
        let info = rest[len..].trim();
        (len >= 3 && !(marker == b'`' && info.contains('`'))).then_some(Self { marker, len, info })
    }

    /// Whether `line` closes this block.
    fn closes(&self, line: &str) -> bool {
        let trimmed = line.trim();
        let run = trimmed.bytes().take_while(|&b| b == self.marker).count();
        run >= self.len && run == trimmed.len()
    }
}

/// Info strings of blocks that hold shell commands.
const SHELL_LANGUAGES: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "shell",
    "fish",
    "ksh",
    "console",
    "shell-session",
    "shellsession",
];

/// Of those, the ones that show a session: `$ ` lines are commands, the
/// rest output.
const SESSION_LANGUAGES: &[&str] = &["console", "shell-session", "shellsession"];

/// A fenced block as a candidate, if it holds commands; see the
/// [module docs](self).
fn block(info: &str, body: &[&str], prose: &Prose) -> Option<Extracted> {
    let language = info
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|c| c == '{' || c == '}' || c == '.')
        .to_ascii_lowercase();
    let labelled = !language.is_empty();
    if labelled && !SHELL_LANGUAGES.contains(&language.as_str()) {
        return None;
    }
    let session =
        SESSION_LANGUAGES.contains(&language.as_str()) || body.iter().any(|l| prompt(l).is_some());
    let mut lines: Vec<&str> = Vec::new();
    let mut continued = false;
    for line in body {
        let command = match prompt(line) {
            Some(command) => command,
            // Output in a session; a continuation is part of the command.
            None if session && !continued => continue,
            None => line.trim_end(),
        };
        let blank_or_comment = command.trim().is_empty() || command.trim_start().starts_with('#');
        if blank_or_comment && !continued {
            continue;
        }
        continued = command.ends_with('\\');
        lines.push(command);
    }
    let first = lines.first()?.trim();
    let (bang, first) = match first.strip_prefix('!') {
        Some(rest) if lines.len() == 1 => (true, rest.trim()),
        _ => (false, first),
    };
    let context = prose.before_block();
    let masked = mask(&context, &code_spans(&context));
    if !labelled {
        let commands = bang && looks_like_command(first)
            || lines.iter().enumerate().all(|(i, line)| {
                i > 0 && lines[i - 1].ends_with('\\') || looks_like_command(line.trim())
            });
        let known = first_word(first).is_some_and(|w| KNOWN_COMMANDS.contains(&w));
        let body = masked.trim_end();
        let cut = body.len().saturating_sub(1);
        let last = &body[sentence(body, cut, cut).0..];
        let asked = asks_anywhere(&tokens(last)) || nearest_cue(&masked, end(&masked)).is_some();
        if !commands || !(known || asked || bang) {
            return None;
        }
    }
    let text = if bang {
        first.to_owned()
    } else {
        lines
            .iter()
            .map(|l| l.trim_start())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let intent = if bang {
        CommandIntent::InSession
    } else {
        nearest_cue(&masked, end(&masked)).unwrap_or(CommandIntent::Unspecified)
    };
    Some(Extracted {
        text,
        intent,
        source: CommandSource::Block,
    })
}

/// The command after a `$ ` (or `% `) prompt marker, if `line` has one.
fn prompt(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("$ ")
        .or_else(|| trimmed.strip_prefix("% "))
        .map(str::trim_end)
}

/// Commands common enough that an unlabelled block or a `!cmd` line
/// starting with one is taken for a command.
const KNOWN_COMMANDS: &[&str] = &[
    "apt",
    "apt-get",
    "aws",
    "clear",
    "date",
    "df",
    "du",
    "exit",
    "free",
    "history",
    "ps",
    "pwd",
    "reset",
    "top",
    "uname",
    "which",
    "whoami",
    "az",
    "bash",
    "brew",
    "bun",
    "bunx",
    "cargo",
    "cat",
    "cd",
    "chmod",
    "chown",
    "claude",
    "codex",
    "cp",
    "curl",
    "deno",
    "dig",
    "direnv",
    "docker",
    "docker-compose",
    "dotnet",
    "echo",
    "env",
    "export",
    "find",
    "firebase",
    "flutter",
    "gcloud",
    "gh",
    "git",
    "go",
    "gradle",
    "grep",
    "helm",
    "herdr",
    "htop",
    "journalctl",
    "jq",
    "just",
    "kill",
    "kubectl",
    "ln",
    "ls",
    "make",
    "mise",
    "mkdir",
    "mv",
    "mvn",
    "nix",
    "node",
    "npm",
    "npx",
    "nvm",
    "op",
    "pass",
    "passporter",
    "pip",
    "pip3",
    "pipx",
    "pnpm",
    "podman",
    "poetry",
    "psql",
    "pytest",
    "python",
    "python3",
    "rails",
    "rake",
    "redis-cli",
    "rm",
    "rsync",
    "ruby",
    "rustup",
    "scp",
    "sed",
    "sh",
    "source",
    "ssh",
    "ssh-add",
    "ssh-keygen",
    "sudo",
    "systemctl",
    "tail",
    "tar",
    "terraform",
    "tmux",
    "touch",
    "trunk",
    "uv",
    "vercel",
    "wget",
    "yarn",
    "zsh",
];

/// Words that start code in other languages, not shell commands.
const CODE_KEYWORDS: &[&str] = &[
    "fn",
    "let",
    "const",
    "var",
    "def",
    "class",
    "import",
    "from",
    "use",
    "pub",
    "return",
    "impl",
    "struct",
    "enum",
    "async",
    "await",
    "function",
    "package",
    "public",
    "private",
    "interface",
    "type",
    "match",
    "#include",
];

/// The first word of a command, past `sudo` and `NAME=value` settings.
fn first_word(command: &str) -> Option<&str> {
    command
        .split_whitespace()
        .find(|word| (word.starts_with('=') || !word.contains('=')) && *word != "sudo")
}

/// Whether `text` (one line) reads like a shell command rather than a
/// path, an identifier, prose or code in another language.
fn looks_like_command(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || text.contains('\n') {
        return false;
    }
    let Some(first) = first_word(text) else {
        return false;
    };
    let smells = ["::", "=>", "()", "{}", "</", "/>"];
    if smells.iter().any(|s| text.contains(s))
        || text.ends_with(['{', ';', ','])
        || CODE_KEYWORDS.contains(&first)
    {
        return false;
    }
    let word_chars = first
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_.-/~+@".contains(c));
    let starts_well = first
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "./~_".contains(c));
    if !word_chars || !starts_well {
        return false;
    }
    let executable = ["./", "../", "~/", "/"]
        .iter()
        .any(|p| first.starts_with(p));
    let single = text.split_whitespace().nth(1).is_none();
    if first.contains('/') && !executable {
        // A relative path such as `src/main.rs`.
        return false;
    }
    if first.contains('.') && !executable && !KNOWN_COMMANDS.contains(&first) {
        // A file name such as `README.md`, or `foo.bar` code.
        return false;
    }
    // A lone word is a command only if it is a well-known one or a
    // relative executable (`./configure`): `main`, `success` or `apply`
    // in backticks are names, and a lone absolute or home path names a
    // file more often than it runs one.
    !single
        || KNOWN_COMMANDS.contains(&first)
        || first.starts_with("./")
        || first.starts_with("../")
}

/// `message` cleaned for extraction: ANSI escape sequences removed (CSI,
/// OSC and two-byte escapes), `\r` dropped, tabs made spaces, and every
/// other control and hidden character (bidi controls, zero-width marks
/// and joiners) removed.
pub(crate) fn clean(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            '\u{200c}' | '\u{200d}' => {}
            c if c.is_control() || protocol::text::is_hidden_char(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// Skips the rest of an escape sequence after its `ESC`.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        Some(']' | 'P' | '_' | '^') => {
            // OSC/DCS/APC/PM: up to BEL or ST (`ESC \`).
            while let Some(c) = chars.next() {
                if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                    break;
                }
            }
        }
        // A two-byte escape (or a lone ESC at the end).
        _ => {}
    }
}

/// Skips a CSI sequence's parameters and final byte.
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        if ('@'..='~').contains(&c) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use CommandIntent::{InSession, NewTerminal, Unspecified};
    use CommandSource::{BangLine, Block, Inline};

    fn found(message: &str) -> Vec<(String, CommandIntent, CommandSource)> {
        extract(message)
            .candidates
            .into_iter()
            .map(|c| (c.text, c.intent, c.source))
            .collect()
    }

    const NONE: [&str; 0] = [];

    fn texts(message: &str) -> Vec<String> {
        extract(message)
            .candidates
            .into_iter()
            .map(|c| c.text)
            .collect()
    }

    fn c(
        text: &str,
        intent: CommandIntent,
        source: CommandSource,
    ) -> (String, CommandIntent, CommandSource) {
        (text.to_owned(), intent, source)
    }

    #[test]
    fn claude_asking_for_a_bang_command_in_session() {
        let message = "I can't open a browser from here. Please run `! gcloud auth login` so \
                       the output lands in this session, then tell me when it's done.";
        assert_eq!(found(message), [c("gcloud auth login", InSession, Inline)]);
    }

    #[test]
    fn another_terminal_is_a_new_terminal() {
        let message = "The dev server has to keep running. In another terminal, run \
                       `npm run dev` and leave it open.";
        assert_eq!(found(message), [c("npm run dev", NewTerminal, Inline)]);
        let block = "Start the server separately:\n\n```bash\nnpm run dev -- --port 3001\n```\n";
        assert_eq!(
            found(block),
            [c("npm run dev -- --port 3001", NewTerminal, Block)]
        );
    }

    #[test]
    fn decoys_are_all_listed_in_order_never_collapsed() {
        let message = "You can either run `! gcloud auth login` here, or \
                       `gcloud auth login --no-browser` in another terminal and paste \
                       the code back.";
        assert_eq!(
            found(message),
            [
                c("gcloud auth login", InSession, Inline),
                c("gcloud auth login --no-browser", NewTerminal, Inline),
            ]
        );
        let either = "Run `make test` or `make test-fast`, whichever you prefer.";
        assert_eq!(texts(either), ["make test", "make test-fast"]);
    }

    #[test]
    fn a_list_under_a_lead_in_line_takes_its_cue() {
        let message = "To fix the login, run one of these in a separate terminal:\n\
                       - `gh auth login`\n\
                       - `gh auth refresh -s repo`\n\
                       \n\
                       Then come back.";
        assert_eq!(
            found(message),
            [
                c("gh auth login", NewTerminal, Inline),
                c("gh auth refresh -s repo", NewTerminal, Inline),
            ]
        );
        // The lead-in ends with the list: a later paragraph is on its own.
        let later = "Run these:\n- `just verify`\n\nThe file is `src/lib.rs`, see `cargo doc`.";
        assert_eq!(texts(later), ["just verify"]);
    }

    #[test]
    fn bang_lines_in_prose_are_in_session() {
        let message =
            "Paste this into the prompt:\n\n! gcloud auth application-default login\n\nThanks.";
        assert_eq!(
            found(message),
            [c(
                "gcloud auth application-default login",
                InSession,
                BangLine
            )]
        );
        assert_eq!(texts("!ls -la"), ["ls -la"]);
        assert_eq!(texts("- ! `docker ps`"), ["docker ps"]);
        // Exclamations are not commands.
        for prose in ["! Important: read this.", "!!!", "!Note"] {
            assert_eq!(texts(prose), NONE, "{prose:?}");
        }
    }

    #[test]
    fn mentions_of_what_the_agent_ran_are_not_asked_of_the_user() {
        for message in [
            "I ran `cargo test` and everything passed.",
            "I'll run `just verify` next.",
            "Tests pass (`cargo nextest run`), and `cargo clippy` is clean.",
            "We can run `npm audit` later if you like.",
            "The failure came from `git push` being rejected.",
            "If you want, I can run `make deploy` for you.",
        ] {
            assert!(
                texts(message).is_empty(),
                "{message:?}: {:?}",
                texts(message)
            );
        }
    }

    #[test]
    fn paths_identifiers_and_code_spans_are_not_commands() {
        let message = "Run the tests after editing `src/main.rs`, `README.md`, \
                       `~/.config/app/config.toml`, `Config::load()`, `fn main() {}`, \
                       `HashMap<String, u32>`, `--release`, `/etc/hosts` and `FooBar`.";
        assert_eq!(texts(message), NONE, "{:?}", texts(message));
    }

    #[test]
    fn executables_and_env_prefixes_count() {
        let message = "Run `./scripts/setup.sh --dev`, `RUST_LOG=debug cargo run`, \
                       `sudo systemctl restart nginx` and `make`.";
        assert_eq!(
            texts(message),
            [
                "./scripts/setup.sh --dev",
                "RUST_LOG=debug cargo run",
                "sudo systemctl restart nginx",
                "make"
            ]
        );
    }

    #[test]
    fn blocks_in_other_languages_never_count() {
        let message = "Run this to see the bug:\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n\n\
                       And the config:\n\n```toml\n[server]\nport = 8080\n```\n\n\
                       ```text\nnpm ERR! missing script: dev\n```\n\n\
                       ```json\n{\"a\": 1}\n```\n\n\
                       ```python\nimport os\nos.system(\"ls\")\n```\n";
        assert_eq!(texts(message), NONE, "{:?}", texts(message));
    }

    #[test]
    fn unlabelled_blocks_count_only_when_they_read_like_commands() {
        let command = "```\ngit fetch origin\ngit rebase origin/main\n```";
        assert_eq!(
            found(command),
            [c(
                "git fetch origin\ngit rebase origin/main",
                Unspecified,
                Block
            )]
        );
        let tree = "The layout:\n\n```\nsrc/\n  main.rs\n  lib.rs\n```";
        assert_eq!(texts(tree), NONE);
        let output = "It printed:\n\n```\nCompiling bridge v0.0.0\nerror: could not compile\n```";
        assert_eq!(texts(output), NONE);
        let code = "```\nconst x = await fetch(url);\n```";
        assert_eq!(texts(code), NONE);
        // An unknown command counts when the text before asks to run it.
        let asked = "Please run:\n\n```\nfrobnicate --all\n```";
        assert_eq!(texts(asked), ["frobnicate --all"]);
        let unasked = "Here is the output of frobnicate:\n\n```\nfrobnicate --all\n```";
        assert_eq!(texts(unasked), NONE);
    }

    #[test]
    fn console_blocks_keep_their_commands_and_drop_output() {
        let message = "```console\n$ npm ci\nadded 312 packages\n$ npm test\n> jest\nPASS\n```";
        assert_eq!(found(message), [c("npm ci\nnpm test", Unspecified, Block)]);
        let unlabelled = "```\n$ cargo build\n   Compiling foo\n```";
        assert_eq!(texts(unlabelled), ["cargo build"]);
    }

    #[test]
    fn shell_blocks_drop_comments_and_keep_continuations() {
        let message = "```bash\n# install first\npip install -r requirements.txt\n\ndocker build \\\n  -t app .\n```";
        assert_eq!(
            texts(message),
            ["pip install -r requirements.txt\ndocker build \\\n-t app ."]
        );
        // With prompts, lines without one are output, even in a bash block.
        let session = "```bash\n$ ls\nfoo.txt\n$ cat foo.txt \\\n  | wc -l\n3\n```";
        assert_eq!(texts(session), ["ls\ncat foo.txt \\\n| wc -l"]);
    }

    #[test]
    fn a_single_bang_line_block_is_in_session() {
        let message = "Run this so I can see the output:\n\n```\n! kubectl get pods -A\n```";
        assert_eq!(found(message), [c("kubectl get pods -A", InSession, Block)]);
    }

    #[test]
    fn mixed_prose_and_several_blocks_keep_message_order() {
        let message = "\
Two things.

First, in this session run `! docker compose ps` so I can see what's up.

Then start the worker in another terminal:

```sh
cd worker && npm start
```

Finally:

```bash
curl -s localhost:8080/health
```
";
        assert_eq!(
            found(message),
            [
                c("docker compose ps", InSession, Inline),
                c("cd worker && npm start", NewTerminal, Block),
                c("curl -s localhost:8080/health", Unspecified, Block),
            ]
        );
    }

    #[test]
    fn here_places_but_here_is_only_introduces() {
        assert_eq!(
            found("Run `make check` here."),
            [c("make check", InSession, Inline)]
        );
        let intro = "Here is the command; run `make check`.";
        assert_eq!(found(intro), [c("make check", Unspecified, Inline)]);
    }

    #[test]
    fn duplicates_are_listed_once_with_the_first_specific_intent() {
        let message = "Run `npm run dev`.\n\nIn another terminal:\n\n```\nnpm run dev\n```";
        assert_eq!(found(message), [c("npm run dev", NewTerminal, Inline)]);
    }

    #[test]
    fn ansi_and_hidden_characters_are_stripped_from_what_runs() {
        let message = "Run `git\u{200b} status\u{202e}` and `\u{1b}[31mls\u{1b}[0m -la`.";
        assert_eq!(texts(message), ["git status", "ls -la"]);
        assert_eq!(clean("a\u{1b}]0;title\u{7}b\r\tc\u{200d}"), "ab c");
        assert_eq!(clean("x\u{1b}]8;;http://e\u{1b}\\y"), "xy");
    }

    #[test]
    fn long_and_numerous_candidates_are_counted_not_cut() {
        let long = format!("Run `echo {}`.", "a".repeat(MAX_COMMAND_CHARS));
        let extraction = extract(&long);
        assert_eq!(extraction.candidates, []);
        assert_eq!(extraction.omitted, 1);
        let lines = (0..=MAX_COMMAND_LINES)
            .map(|i| format!("echo {i}"))
            .collect::<Vec<_>>();
        let block = format!("```bash\n{}\n```", lines.join("\n"));
        assert_eq!(extract(&block).omitted, 1);
        let many = (0..MAX_CANDIDATES + 3)
            .map(|i| format!("- `echo {i}`"))
            .collect::<Vec<_>>()
            .join("\n");
        let extraction = extract(&format!("Run any of:\n{many}"));
        assert_eq!(extraction.candidates.len(), MAX_CANDIDATES);
        assert_eq!(extraction.omitted, 3);
        assert_eq!(extraction.candidates[0].text, "echo 0");
    }

    #[test]
    fn unclosed_fences_run_to_the_end_and_spans_need_their_closer() {
        assert_eq!(texts("Run:\n```bash\nmake all"), ["make all"]);
        assert_eq!(texts("Run `make all"), NONE);
        assert_eq!(
            texts("Run ``git log --format=`%h` `` now."),
            ["git log --format=`%h`"]
        );
    }

    #[test]
    fn the_run_verb_must_ask_the_user() {
        for message in [
            "Don't run `./run deploy api` from main until the PR merges.",
            "Never run `git push --force` on main.",
            "It came from a local tool run, then got swept up by `git commit -a`.",
            "**Release run:** run `42368991124` on `1f5045a`, `success`.",
            "Every run checks `deploy status`, but only acts once.",
            "The dry run printed `terraform plan -out x` as its last step.",
            "Here's how the REPL's `watch` trace works.",
        ] {
            assert!(
                texts(message).is_empty(),
                "{message:?}: {:?}",
                texts(message)
            );
        }
        for (message, want) in [
            (
                "When you get a moment, run `ssh-add` and I'll push.",
                "ssh-add",
            ),
            (
                "You'll need to run `gh auth refresh -s repo` first.",
                "gh auth refresh -s repo",
            ),
            (
                "Can you run `docker compose logs api`?",
                "docker compose logs api",
            ),
            ("If it fails, rerun `just verify`.", "just verify"),
            ("**Then run** `make deploy`.", "make deploy"),
        ] {
            assert_eq!(texts(message), [want], "{message:?}");
        }
    }

    #[test]
    fn tables_and_numbered_steps_are_read_cell_by_cell() {
        let table = "| Step | Who |\n|---|---|\n| Merge #54 | you: `! gh pr merge 54 --squash` |\n\
                     | Tag | me: `git tag v1` |";
        assert_eq!(
            found(table),
            [c("gh pr merge 54 --squash", InSession, Inline)]
        );
        let steps = "1. Merge: `! gh pr merge 60 --squash`\n2. Then run `git pull` on main.";
        assert_eq!(
            found(steps),
            [
                c("gh pr merge 60 --squash", InSession, Inline),
                c("git pull", Unspecified, Inline),
            ]
        );
    }

    #[test]
    fn a_cue_alone_places_only_a_command_right_by_it() {
        assert_eq!(
            found("Running `docker ps -a` in your own terminal shows it."),
            [c("docker ps -a", NewTerminal, Inline)]
        );
        // Far from the cue, or a lone word: not asked.
        assert_eq!(
            texts("Every command will `cd` into it, outside this session."),
            NONE
        );
        let far = "The test suite (`cargo test --workspace`) covers the parser, the store and \
                   the server, and it all runs in another terminal.";
        assert_eq!(texts(far), NONE);
        // A bare "here" never asks on its own.
        assert_eq!(
            texts("All of the logic goes here, tested with `cargo test --lib`."),
            NONE
        );
    }

    #[test]
    fn a_bang_block_after_a_request_is_in_session() {
        let message = "That needs a root apply, which auto mode blocks. Please apply it:\n\n\
                       ```\n! cd ~/git/infra/deploy && ./run apply updates\n```\n\n\
                       Then I'll send the test email.";
        assert_eq!(
            found(message),
            [c(
                "cd ~/git/infra/deploy && ./run apply updates",
                InSession,
                Block
            )]
        );
    }

    #[test]
    fn codex_style_messages_work_too() {
        let message = "I need you to authenticate first. Run this in your terminal:\n\n\
                       ```bash\nop signin\n```\n\nOr, to keep it here, type `!op whoami`.";
        assert_eq!(
            found(message),
            [
                c("op signin", NewTerminal, Block),
                c("op whoami", InSession, Inline),
            ]
        );
    }
}

/// Corpus check (dev workstation only, `$PASSPORTER_CORPUS`, see
/// docs/corpus.md): extraction over every assistant message of the
/// scrubbed corpus never breaks its invariants. Prints counts only, never
/// transcript content.
#[cfg(test)]
mod corpus {
    use std::path::{Path, PathBuf};

    use protocol::ThreadContent;
    use transcripts::{Scope, TranscriptReader};

    use super::*;

    fn transcripts(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let in_dir = |name: &str| path.components().any(|c| c.as_os_str() == name);
            if path.is_dir() {
                transcripts(&path, out);
            } else if path.extension().is_some_and(|e| e == "jsonl")
                && in_dir("projects")
                && !in_dir("subagents")
            {
                out.push(path);
            }
        }
    }

    #[test]
    fn corpus_messages_extract_within_the_rules() {
        let Some(root) = std::env::var_os("PASSPORTER_CORPUS").map(PathBuf::from) else {
            eprintln!("corpus: PASSPORTER_CORPUS is unset; skipping (see docs/corpus.md)");
            return;
        };
        let mut files = Vec::new();
        transcripts(&root, &mut files);
        let (mut messages, mut with, mut found, mut omitted) = (0_u64, 0_u64, 0_u64, 0_u64);
        let mut by_intent = [0_u64; 3];
        for path in &files {
            let mut reader = TranscriptReader::new(path, Scope::Main);
            if reader.refresh().is_err() {
                continue;
            }
            for item in reader.thread().items() {
                let ThreadContent::Assistant { text } = &item.content else {
                    continue;
                };
                messages += 1;
                let extraction = extract(text);
                omitted += u64::from(extraction.omitted);
                with += u64::from(!extraction.candidates.is_empty());
                for candidate in &extraction.candidates {
                    found += 1;
                    by_intent[match candidate.intent {
                        CommandIntent::InSession => 0,
                        CommandIntent::NewTerminal => 1,
                        CommandIntent::Unspecified => 2,
                    }] += 1;
                    let text = &candidate.text;
                    assert_ne!(text.trim(), "");
                    assert!(text.chars().count() <= MAX_COMMAND_CHARS);
                    assert!(text.lines().count() <= MAX_COMMAND_LINES);
                    assert!(candidate.source == CommandSource::Block || !text.contains('\n'));
                    assert!(!text.chars().any(|c| {
                        (c.is_control() && c != '\n') || protocol::text::is_hidden_char(c)
                    }));
                    assert!(!text.starts_with('!'));
                }
            }
        }
        let [here, terminal, unspecified] = by_intent;
        eprintln!(
            "corpus: {} transcripts, {messages} assistant messages, {with} with candidates, \
             {found} candidates (in-session {here}, new terminal {terminal}, unspecified \
             {unspecified}), {omitted} omitted",
            files.len()
        );
    }
}
