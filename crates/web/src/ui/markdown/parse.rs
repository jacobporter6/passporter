//! A small Markdown parser for chat text: the common CommonMark/GFM subset
//! agents write, into a [`Node`] tree. Pure, so it is tested on the host.
//!
//! Blocks: paragraphs, ATX and setext headings, fenced and indented code,
//! block quotes, bullet and ordered lists (nested, tight or loose, with task
//! markers), GFM tables and thematic breaks. Inlines: emphasis, strong,
//! strikethrough, code spans, links, images (alt text only), `<scheme:…>`
//! autolinks, bare `http(s)://` URLs, hard breaks, backslash escapes and
//! the common entities.
//!
//! There is no HTML in this grammar at all: `<tag>` is text. Reference
//! links, footnotes and HTML blocks are not supported; they show as the text
//! that was written.
//!
//! Every scan is bounded (input size, nesting, node count, span lengths,
//! emphasis look-back), so hostile input stays linear-ish and shallow.

/// Deepest block-container nesting (quotes, list items) parsed; deeper
/// content is shown as plain paragraph text.
pub const MAX_DEPTH: usize = 8;
/// Deepest emphasis nesting; deeper delimiters stay literal.
pub const MAX_INLINE_DEPTH: usize = 6;
/// Most nodes built for one text; the rest is cut with a note.
pub const MAX_NODES: usize = 8_000;
/// Longest input parsed, in UTF-8 bytes; longer text is cut with a note.
pub const MAX_INPUT_BYTES: usize = 128 * 1024;
/// Longest URL that can be opened.
pub const MAX_URL_BYTES: usize = 2_048;
/// Longest code span or link text/destination searched for, in chars.
const MAX_SPAN: usize = 2_000;
/// How many tokens back a closing emphasis delimiter looks for its opener.
const MAX_LOOKBACK: usize = 256;
/// Most table columns kept; extra cells are dropped.
const MAX_TABLE_COLUMNS: usize = 64;
/// Most table body rows kept; the rest ends the table.
const MAX_TABLE_ROWS: usize = 1_000;
/// A failed link destination scan that went this far past its start sets a
/// floor: no later link may start a destination before where it stopped.
const LINK_FLOOR_MIN: usize = 64;

/// What a [`Node`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// The top of a parsed text.
    Root,
    /// A paragraph.
    Paragraph,
    /// A heading, level 1..=6 as written.
    Heading(u8),
    /// A block quote.
    BlockQuote,
    /// A fenced or indented code block; its child is one [`Kind::Text`].
    CodeBlock,
    /// A list; `start` is set for ordered lists.
    List {
        /// The first number of an ordered list.
        start: Option<u64>,
    },
    /// A list item.
    Item,
    /// A task list checkbox, shown as a mark (never an input).
    Task(bool),
    /// A table: a [`Kind::TableHead`] of cells, then [`Kind::TableRow`]s.
    Table,
    /// The header cells of a table.
    TableHead,
    /// A body row.
    TableRow,
    /// A cell, with its column's alignment.
    TableCell(Align),
    /// Emphasis.
    Emphasis,
    /// Strong emphasis.
    Strong,
    /// Strikethrough.
    Strike,
    /// A link: the children are its text.
    Link {
        /// The URL as shown: hidden and control characters replaced by
        /// U+FFFD so the visible URL is exactly what would be opened.
        shown: String,
        /// Whether it may be opened (after confirmation).
        openable: bool,
    },
    /// An image: the children are its alt text. The URL is dropped.
    Image,
    /// A run of text.
    Text(String),
    /// Inline code.
    Code(String),
    /// A hard line break.
    Break,
    /// A thematic break.
    Rule,
    /// Where the text was cut.
    Truncated,
}

/// A table column's alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Unspecified.
    None,
    /// Left.
    Left,
    /// Centered.
    Center,
    /// Right.
    Right,
}

/// One element of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// What it is.
    pub kind: Kind,
    /// Its content, in order.
    pub children: Vec<Self>,
}

impl Node {
    const fn leaf(kind: Kind) -> Self {
        Self {
            kind,
            children: Vec::new(),
        }
    }

    /// The node's text content, concatenated.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out
    }

    fn collect_text(&self, out: &mut String) {
        match &self.kind {
            Kind::Text(text) | Kind::Code(text) => out.push_str(text),
            _ => self.children.iter().for_each(|c| c.collect_text(out)),
        }
    }
}

/// Whether `url` may be opened: `http:` or `https:` (any case), at most
/// [`MAX_URL_BYTES`], with no whitespace, control or hidden characters (which
/// browsers strip or render invisibly, so the shown URL could differ from
/// the one opened), and no backslash (browsers read it as `/`). Its
/// authority (after `://`, up to the first `/`, `?` or `#`) must be
/// non-empty printable ASCII without `@`: no userinfo to hide the real
/// host behind (`https://good.com@evil.com`), and no look-alike slashes
/// (U+2215, U+2044), homographs or blank Braille in the host, so the host
/// the user reads is the host that opens. Everything else (`javascript:`,
/// `data:`, `file:`, relative links, `mailto:`) is shown but never opened.
#[must_use]
pub fn is_openable(url: &str) -> bool {
    let Some(scheme) = ["https://", "http://"].into_iter().find(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    }) else {
        return false;
    };
    let rest = &url[scheme.len()..];
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    !authority.is_empty()
        && authority.chars().all(|c| c.is_ascii_graphic() && c != '@')
        && url.len() <= MAX_URL_BYTES
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c == '\\' || is_unsafe_char(c))
}

/// The host part of an openable `url`'s authority (no port), for the
/// confirmation dialog; `None` if it isn't openable.
#[must_use]
pub fn url_host(url: &str) -> Option<&str> {
    if !is_openable(url) {
        return None;
    }
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    // `[v6]:port` keeps its brackets; `host:port` drops the port.
    Some(match authority.rfind(':') {
        Some(at) if !authority[at..].contains(']') => &authority[..at],
        _ => authority,
    })
}

/// `url` with every control or hidden character replaced by U+FFFD.
#[must_use]
pub fn shown_url(url: &str) -> String {
    url.chars()
        .map(|c| if is_unsafe_char(c) { '\u{FFFD}' } else { c })
        .collect()
}

fn is_unsafe_char(c: char) -> bool {
    c.is_control() || protocol::text::is_hidden_char(c)
}

fn link_node(url: &str, children: Vec<Node>) -> Node {
    Node {
        kind: Kind::Link {
            shown: shown_url(url),
            openable: is_openable(url),
        },
        children,
    }
}

/// Parses `source` into a tree under a [`Kind::Root`].
#[must_use]
pub fn parse(source: &str) -> Node {
    let (clipped, input_cut) = clip(source, MAX_INPUT_BYTES);
    // NUL is reserved as the hard-break marker; tabs become spaces so
    // indentation is plain slicing.
    let mut clean = String::with_capacity(clipped.len());
    for c in clipped.chars() {
        match c {
            '\0' => clean.push('\u{FFFD}'),
            '\t' => clean.push_str("    "),
            _ => clean.push(c),
        }
    }
    let lines: Vec<&str> = clean.lines().collect();
    let mut ctx = Ctx::default();
    let mut children = blocks(&mut ctx, &lines, 0);
    if ctx.cut || input_cut {
        children.push(Node::leaf(Kind::Truncated));
    }
    Node {
        kind: Kind::Root,
        children,
    }
}

/// `text` cut to at most `max` bytes on a char boundary, and whether it was.
fn clip(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// The node budget.
#[derive(Debug, Default)]
struct Ctx {
    nodes: usize,
    cut: bool,
}

impl Ctx {
    fn node(&mut self, kind: Kind, children: Vec<Node>) -> Node {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            self.cut = true;
        }
        let kind = match kind {
            Kind::Text(text) => Kind::Text(visible(text)),
            Kind::Code(code) => Kind::Code(visible(code)),
            other => other,
        };
        Node { kind, children }
    }
}

/// `text` with hidden characters (bidi controls, zero-width marks, the
/// list protocol uses for prompts) and control characters other than `\n`
/// replaced by U+FFFD, so agent text can't reorder or hide what is shown.
/// Runs after entity decoding, so `&#x202E;` is caught too.
fn visible(text: String) -> String {
    replace_hidden(text, |c| c == '\n')
}

/// [`visible`] for plain text shown as code (tool input and output, diffs):
/// tabs are kept as well, and `\r\n` becomes `\n`; any other `\r`, like
/// every other control character, is replaced.
#[must_use]
pub fn visible_code(text: String) -> String {
    let text = if text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text
    };
    replace_hidden(text, |c| matches!(c, '\n' | '\t'))
}

/// `text` with every hidden or control character not `kept` replaced by
/// U+FFFD.
fn replace_hidden(text: String, kept: impl Fn(char) -> bool) -> String {
    let hidden = |c: char| (c.is_control() && !kept(c)) || protocol::text::is_hidden_char(c);
    if !text.chars().any(hidden) {
        return text;
    }
    text.chars()
        .map(|c| if hidden(c) { '\u{FFFD}' } else { c })
        .collect()
}

// ---------- Blocks ----------

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// Leading spaces, and the rest.
fn indentation(line: &str) -> (usize, &str) {
    let rest = line.trim_start_matches(' ');
    (line.len() - rest.len(), rest)
}

/// `line` without up to `n` leading spaces.
fn strip_spaces(line: &str, n: usize) -> &str {
    let (indent, _) = indentation(line);
    &line[indent.min(n)..]
}

/// A code fence: its char and length.
fn fence_open(rest: &str) -> Option<(char, usize)> {
    let ch = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = rest.chars().take_while(|&c| c == ch).count();
    let info = &rest[len..];
    (len >= 3 && !(ch == '`' && info.contains('`'))).then_some((ch, len))
}

fn fence_close(line: &str, ch: char, len: usize) -> bool {
    let (indent, rest) = indentation(line);
    let run = rest.chars().take_while(|&c| c == ch).count();
    indent < 4 && run >= len && rest[run..].trim().is_empty()
}

/// An ATX heading: level and text.
fn atx(rest: &str) -> Option<(u8, &str)> {
    let hashes = rest.chars().take_while(|&c| c == '#').count();
    let after = &rest[hashes..];
    if !(1..=6).contains(&hashes) || !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    let text = after.trim();
    // An optional closing run of #s, after a space.
    let trimmed = text.trim_end_matches('#');
    let text = if trimmed.is_empty() || trimmed.ends_with(' ') {
        trimmed.trim_end()
    } else {
        text
    };
    u8::try_from(hashes).ok().map(|level| (level, text))
}

/// `---`, `***` or `___` (three or more, spaces allowed).
fn is_rule(rest: &str) -> bool {
    let mut chars = rest.chars().filter(|c| *c != ' ');
    let Some(first) = chars.next().filter(|c| matches!(c, '-' | '*' | '_')) else {
        return false;
    };
    let mut count = 1;
    for c in chars {
        if c != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}

/// A setext underline: `=` (level 1) or `-` (level 2).
fn setext(line: &str) -> Option<u8> {
    let (indent, rest) = indentation(line);
    let rest = rest.trim_end();
    if indent >= 4 || rest.is_empty() {
        return None;
    }
    if rest.chars().all(|c| c == '=') {
        Some(1)
    } else if rest.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

/// A list item marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Marker<'a> {
    /// `-`, `*`, `+`, or `.`/`)` for ordered items.
    kind: char,
    /// The number of an ordered item.
    number: Option<u64>,
    /// The column the item's content starts at.
    width: usize,
    /// The first line's content.
    content: &'a str,
}

fn list_marker(line: &str) -> Option<Marker<'_>> {
    let (indent, rest) = indentation(line);
    if indent >= 4 {
        return None;
    }
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let (kind, number, marker_len) = if digits == 0 {
        let c = rest
            .chars()
            .next()
            .filter(|c| matches!(c, '-' | '*' | '+'))?;
        (c, None, 1)
    } else {
        let delimiter = rest[digits..]
            .chars()
            .next()
            .filter(|c| matches!(c, '.' | ')'))?;
        let number = rest[..digits].parse::<u64>().ok().filter(|_| digits <= 9)?;
        (delimiter, Some(number), digits + 1)
    };
    let after = &rest[marker_len..];
    if !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    let spaces = after.len() - after.trim_start_matches(' ').len();
    // Five or more spaces: the content is indented code; the item starts
    // one space after the marker.
    let spaces = if spaces == 0 || spaces > 4 || after.trim().is_empty() {
        1
    } else {
        spaces
    };
    Some(Marker {
        kind,
        number,
        width: indent + marker_len + spaces,
        content: after.get(spaces..).unwrap_or(""),
    })
}

/// Whether `line` starts a block that ends a paragraph.
fn interrupts(line: &str) -> bool {
    let (indent, rest) = indentation(line);
    if indent >= 4 {
        return false;
    }
    fence_open(rest).is_some()
        || atx(rest).is_some()
        || is_rule(rest)
        || rest.starts_with('>')
        || list_marker(line)
            .is_some_and(|m| !m.content.trim().is_empty() && m.number.is_none_or(|n| n == 1))
}

/// The cells of a table row, unescaped pipes as separators (not inside
/// code spans).
fn split_row(line: &str) -> Vec<&str> {
    let row = line.trim();
    let row = row.strip_prefix('|').unwrap_or(row);
    let row = if row.ends_with('|') && !row.ends_with("\\|") {
        &row[..row.len() - 1]
    } else {
        row
    };
    let mut cells = Vec::new();
    let (mut start, mut escaped, mut in_code) = (0, false, false);
    for (i, c) in row.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '`' => in_code = !in_code,
            '|' if !in_code => {
                cells.push(row[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    cells.push(row[start..].trim());
    cells
}

fn delimiter_row(line: &str) -> Option<Vec<Align>> {
    if !line.contains('-') {
        return None;
    }
    split_row(line)
        .into_iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.ends_with(':') && cell.len() > 1;
            let dashes = cell.trim_start_matches(':').trim_end_matches(':');
            (!dashes.is_empty() && dashes.chars().all(|c| c == '-')).then_some(
                match (left, right) {
                    (true, true) => Align::Center,
                    (true, false) => Align::Left,
                    (false, true) => Align::Right,
                    (false, false) => Align::None,
                },
            )
        })
        .collect()
}

/// A table starting at `lines[0]`: its alignments, when `lines[1]` is a
/// matching delimiter row.
fn table_start(lines: &[&str]) -> Option<Vec<Align>> {
    let (head, delimiter) = (lines.first()?, lines.get(1)?);
    if !head.contains('|') || indentation(head).0 >= 4 {
        return None;
    }
    let mut aligns = delimiter_row(delimiter)?;
    if aligns.len() != split_row(head).len() {
        return None;
    }
    aligns.truncate(MAX_TABLE_COLUMNS);
    Some(aligns)
}

fn blocks(ctx: &mut Ctx, lines: &[&str], depth: usize) -> Vec<Node> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() && !ctx.cut {
        let line = lines[i];
        if is_blank(line) {
            i += 1;
            continue;
        }
        let (indent, rest) = indentation(line);
        if indent >= 4 {
            let start = i;
            while i < lines.len() && (is_blank(lines[i]) || indentation(lines[i]).0 >= 4) {
                i += 1;
            }
            let mut end = i;
            while end > start && is_blank(lines[end - 1]) {
                end -= 1;
            }
            let body: Vec<&str> = lines[start..end]
                .iter()
                .map(|l| strip_spaces(l, 4))
                .collect();
            out.push(code_block(ctx, &body));
        } else if let Some((ch, len)) = fence_open(rest) {
            i += 1;
            let start = i;
            while i < lines.len() && !fence_close(lines[i], ch, len) {
                i += 1;
            }
            let body: Vec<&str> = lines[start..i]
                .iter()
                .map(|l| strip_spaces(l, indent))
                .collect();
            out.push(code_block(ctx, &body));
            i += 1; // the closing fence
        } else if let Some((level, text)) = atx(rest) {
            let children = inlines(ctx, text);
            out.push(ctx.node(Kind::Heading(level), children));
            i += 1;
        } else if is_rule(rest) {
            out.push(ctx.node(Kind::Rule, Vec::new()));
            i += 1;
        } else if rest.starts_with('>') {
            let mut inner = Vec::new();
            while let Some(quoted) = lines.get(i).and_then(|l| {
                let (indent, rest) = indentation(l);
                (indent < 4).then_some(rest)?.strip_prefix('>')
            }) {
                inner.push(quoted.strip_prefix(' ').unwrap_or(quoted));
                i += 1;
            }
            out.push(container(ctx, Kind::BlockQuote, &inner, depth));
        } else if let Some(marker) = list_marker(line) {
            i += list(ctx, &lines[i..], marker, depth, &mut out);
        } else if let Some(aligns) = table_start(&lines[i..]) {
            i += table(ctx, &lines[i..], &aligns, &mut out);
        } else {
            i += paragraph(ctx, &lines[i..], &mut out);
        }
    }
    out
}

fn code_block(ctx: &mut Ctx, body: &[&str]) -> Node {
    let mut text = body.join("\n");
    if !body.is_empty() {
        text.push('\n');
    }
    let child = ctx.node(Kind::Text(text), Vec::new());
    ctx.node(Kind::CodeBlock, vec![child])
}

/// A container of `lines`, parsed as blocks while within [`MAX_DEPTH`],
/// else as one plain paragraph.
fn container(ctx: &mut Ctx, kind: Kind, lines: &[&str], depth: usize) -> Node {
    let children = if depth < MAX_DEPTH {
        blocks(ctx, lines, depth + 1)
    } else {
        let text = lines
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join("\n");
        let child = ctx.node(Kind::Text(text), Vec::new());
        vec![ctx.node(Kind::Paragraph, vec![child])]
    };
    ctx.node(kind, children)
}

/// Lines from a paragraph start; returns how many were used.
fn paragraph(ctx: &mut Ctx, lines: &[&str], out: &mut Vec<Node>) -> usize {
    let mut used = 0;
    let mut heading = None;
    while let Some(line) = lines.get(used) {
        if is_blank(line) || (used > 0 && interrupts(line) && setext(line).is_none()) {
            break;
        }
        if used > 0
            && let Some(level) = setext(line)
        {
            heading = Some(level);
            break;
        }
        used += 1;
    }
    let text = join_lines(&lines[..used]);
    let children = inlines(ctx, &text);
    let kind = heading.map_or(Kind::Paragraph, Kind::Heading);
    out.push(ctx.node(kind, children));
    used + usize::from(heading.is_some())
}

/// Paragraph lines joined with `\n`, with `\0` marking hard breaks (two
/// trailing spaces or a trailing backslash).
fn join_lines(lines: &[&str]) -> String {
    let mut text = String::new();
    for (n, line) in lines.iter().enumerate() {
        let line = line.trim_start();
        if n + 1 == lines.len() {
            text.push_str(line.trim_end());
        } else if line.ends_with("  ") {
            text.push_str(line.trim_end());
            text.push('\0');
        } else if let Some(stripped) = line.strip_suffix('\\') {
            text.push_str(stripped);
            text.push('\0');
        } else {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

/// A list starting with `first` at `lines[0]`; returns lines used.
fn list(
    ctx: &mut Ctx,
    lines: &[&str],
    first: Marker<'_>,
    depth: usize,
    out: &mut Vec<Node>,
) -> usize {
    let mut items = Vec::new();
    let mut tight = true;
    let mut i = 0;
    let mut marker = first;
    loop {
        // One item: its first line, then indented or lazy lines.
        let mut body = vec![marker.content];
        i += 1;
        while let Some(line) = lines.get(i) {
            if is_blank(line) {
                let next = (i..lines.len()).find(|&j| !is_blank(lines[j]));
                match next {
                    Some(j) if indentation(lines[j]).0 >= marker.width => {
                        body.extend(lines[i..j].iter().map(|_| ""));
                        i = j;
                    }
                    _ => break,
                }
            } else if indentation(line).0 >= marker.width {
                body.push(&line[marker.width..]);
                i += 1;
            } else if list_marker(line).is_some()
                || interrupts(line)
                || body.last().is_some_and(|l| l.is_empty())
            {
                break;
            } else {
                body.push(line.trim_start());
                i += 1;
            }
        }
        items.push(item(ctx, &body, depth));
        if ctx.cut {
            break;
        }
        // The list goes on with another item of the same kind, perhaps
        // after blank lines (which make it loose).
        let next = (i..lines.len()).find(|&j| !is_blank(lines[j]));
        match next.and_then(|j| Some((j, list_marker(lines[j])?))) {
            Some((j, next)) if next.kind == first.kind => {
                tight &= j == i;
                i = j;
                marker = next;
            }
            _ => break,
        }
    }
    if tight {
        for item in &mut items {
            let children = core::mem::take(&mut item.children);
            for child in children {
                if child.kind == Kind::Paragraph {
                    item.children.extend(child.children);
                } else {
                    item.children.push(child);
                }
            }
        }
    }
    out.push(ctx.node(
        Kind::List {
            start: first.number,
        },
        items,
    ));
    i
}

/// One list item, with an optional task marker.
fn item(ctx: &mut Ctx, body: &[&str], depth: usize) -> Node {
    let first = body.first().copied().unwrap_or("");
    let task = ["[ ] ", "[x] ", "[X] "]
        .iter()
        .find(|t| first.starts_with(*t) || first == t.trim_end());
    let mut lines = body.to_vec();
    let mut node = if let Some(task) = task {
        lines[0] = first.get(task.len()..).unwrap_or("");
        let mark = ctx.node(Kind::Task(task.contains(['x', 'X'])), Vec::new());
        let mut node = container(ctx, Kind::Item, &lines, depth);
        node.children.insert(0, mark);
        node
    } else {
        container(ctx, Kind::Item, &lines, depth)
    };
    node.children
        .retain(|c| c.kind != Kind::Paragraph || !c.children.is_empty());
    node
}

/// A table at `lines[0]` (head) and `lines[1]` (delimiter); returns lines
/// used.
fn table(ctx: &mut Ctx, lines: &[&str], aligns: &[Align], out: &mut Vec<Node>) -> usize {
    let row_cells = |ctx: &mut Ctx, line: &str| -> Vec<Node> {
        let cells = split_row(line);
        let mut row = Vec::new();
        for (n, align) in aligns.iter().enumerate() {
            if ctx.cut {
                break;
            }
            let children = cells.get(n).map_or_else(Vec::new, |c| inlines(ctx, c));
            row.push(ctx.node(Kind::TableCell(*align), children));
        }
        row
    };
    let head_cells = row_cells(ctx, lines[0]);
    let mut rows = vec![ctx.node(Kind::TableHead, head_cells)];
    let mut used = 2;
    while let Some(line) = lines.get(used) {
        if is_blank(line) || !line.contains('|') || ctx.cut || used - 2 >= MAX_TABLE_ROWS {
            break;
        }
        let cells = row_cells(ctx, line);
        rows.push(ctx.node(Kind::TableRow, cells));
        used += 1;
    }
    out.push(ctx.node(Kind::Table, rows));
    used
}

// ---------- Inlines ----------

#[derive(Debug)]
enum Tok {
    Text(String),
    Node(Node, usize),
    Delim {
        ch: char,
        count: usize,
        open: bool,
        close: bool,
    },
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace() && !c.is_control())
}

/// Inline content of `text` (a paragraph, heading or cell).
fn inlines(ctx: &mut Ctx, text: &str) -> Vec<Node> {
    let chars: Vec<char> = text.chars().collect();
    let toks = tokens(ctx, &chars, true);
    let paired = emphasis(ctx, toks);
    finish(ctx, paired)
}

/// Splits `chars` into text, finished nodes and emphasis delimiters.
fn tokens(ctx: &mut Ctx, chars: &[char], links: bool) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    let n = chars.len();
    let brackets = if links {
        match_brackets(chars)
    } else {
        Vec::new()
    };
    let mut floor = 0_usize;
    let flush = |buf: &mut String, toks: &mut Vec<Tok>| {
        if !buf.is_empty() {
            toks.push(Tok::Text(core::mem::take(buf)));
        }
    };
    while i < n && !ctx.cut {
        let c = chars[i];
        let prev = if i == 0 { ' ' } else { chars[i - 1] };
        match c {
            '\\' if chars.get(i + 1).is_some_and(char::is_ascii_punctuation) => {
                buf.push(chars[i + 1]);
                i += 2;
            }
            '\0' => {
                flush(&mut buf, &mut toks);
                toks.push(Tok::Node(ctx.node(Kind::Break, Vec::new()), 0));
                i += 1;
            }
            '`' => {
                let run = chars[i..].iter().take_while(|&&c| c == '`').count();
                if let Some(end) = code_close(chars, i + run, run) {
                    flush(&mut buf, &mut toks);
                    let code = code_text(&chars[i + run..end]);
                    toks.push(Tok::Node(ctx.node(Kind::Code(code), Vec::new()), 0));
                    i = end + run;
                } else {
                    buf.extend(core::iter::repeat_n('`', run));
                    i += run;
                }
            }
            '[' | '!' | '<' | 'h' | 'H' if links || c == '<' => {
                if let Some((tok, end)) = link_token(ctx, chars, &brackets, &mut floor, i, prev) {
                    flush(&mut buf, &mut toks);
                    toks.push(tok);
                    i = end;
                } else {
                    buf.push(c);
                    i += 1;
                }
            }
            '*' | '_' | '~' => {
                flush(&mut buf, &mut toks);
                let (tok, count) = delimiter(chars, i, prev);
                toks.push(tok);
                i += count;
            }
            '&' => {
                if let Some((decoded, len)) = entity(&chars[i..]) {
                    buf.push(decoded);
                    i += len;
                } else {
                    buf.push('&');
                    i += 1;
                }
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(&mut buf, &mut toks);
    toks
}

/// A link, image, autolink or bare URL starting at `chars[i]`, and the
/// index after it.
fn link_token(
    ctx: &mut Ctx,
    chars: &[char],
    brackets: &[u32],
    floor: &mut usize,
    i: usize,
    prev: char,
) -> Option<(Tok, usize)> {
    let url_link = |ctx: &mut Ctx, url: String| {
        let text = ctx.node(Kind::Text(url.clone()), Vec::new());
        ctx.nodes += 1;
        Tok::Node(link_node(&url, vec![text]), 1)
    };
    match chars[i] {
        '<' => autolink(chars, i).map(|(url, end)| (url_link(ctx, url), end)),
        'h' | 'H' if !prev.is_alphanumeric() => {
            let end = bare_url(chars, i)?;
            Some((url_link(ctx, chars[i..end].iter().collect()), end))
        }
        '[' | '!' => {
            let image = chars[i] == '!';
            let open = if image { i + 1 } else { i };
            if image && chars.get(open) != Some(&'[') {
                return None;
            }
            let link = match link_at(chars, brackets, *floor, open) {
                Ok(link) => link,
                Err(Some((start, reached))) => {
                    if reached.saturating_sub(start) >= LINK_FLOOR_MIN {
                        *floor = (*floor).max(reached);
                    }
                    return None;
                }
                Err(None) => return None,
            };
            let label = tokens(ctx, &chars[open + 1..link.close], false);
            let label = emphasis(ctx, label);
            let label = finish(ctx, label);
            let node = if image {
                ctx.node(Kind::Image, label)
            } else {
                ctx.nodes += 1;
                link_node(&link.url, label)
            };
            Some((Tok::Node(node, 1), link.end))
        }
        _ => None,
    }
}

/// The emphasis delimiter run at `chars[i]` (`*`, `_` or `~`), with its
/// flanking rules, and its length.
fn delimiter(chars: &[char], i: usize, prev: char) -> (Tok, usize) {
    let ch = chars[i];
    let count = chars[i..].iter().take_while(|&&d| d == ch).count();
    let next = chars.get(i + count).copied().unwrap_or(' ');
    let left = !next.is_whitespace() && (!is_punct(next) || prev.is_whitespace() || is_punct(prev));
    let right =
        !prev.is_whitespace() && (!is_punct(prev) || next.is_whitespace() || is_punct(next));
    let (open, close) = match ch {
        '_' => (
            left && (!right || is_punct(prev)),
            right && (!left || is_punct(next)),
        ),
        '~' if count != 2 => (false, false),
        _ => (left, right),
    };
    (
        Tok::Delim {
            ch,
            count,
            open,
            close,
        },
        count,
    )
}

/// The index of the closing backtick run of exactly `run` after `from`.
fn code_close(chars: &[char], from: usize, run: usize) -> Option<usize> {
    let limit = chars.len().min(from.saturating_add(MAX_SPAN));
    let mut j = from;
    while j < limit {
        if chars[j] == '`' {
            let len = chars[j..].iter().take_while(|&&c| c == '`').count();
            if len == run {
                return Some(j);
            }
            j += len;
        } else {
            j += 1;
        }
    }
    None
}

/// A code span's text: line breaks become spaces, and one space is
/// stripped from both ends when both have one (and it isn't all spaces).
fn code_text(chars: &[char]) -> String {
    let text: String = chars
        .iter()
        .map(|&c| if matches!(c, '\n' | '\0') { ' ' } else { c })
        .collect();
    if text.len() >= 2 && text.starts_with(' ') && text.ends_with(' ') && !text.trim().is_empty() {
        text[1..text.len() - 1].to_owned()
    } else {
        text
    }
}

struct LinkAt {
    /// Index of the `]`.
    close: usize,
    url: String,
    /// Index after the `)`.
    end: usize,
}

/// For each unescaped `[`, the index of its matching `]` (`u32::MAX`
/// for none), in one pass, so link parsing never rescans.
fn match_brackets(chars: &[char]) -> Vec<u32> {
    let mut matches = vec![u32::MAX; chars.len()];
    let mut open = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '[' => open.push(i),
            ']' => {
                if let Some(start) = open.pop() {
                    matches[start] = u32::try_from(i).unwrap_or(u32::MAX);
                }
            }
            _ => {}
        }
        i += 1;
    }
    matches
}

/// `[text](dest "title")` with `chars[open] == '['`; `brackets` from
/// [`match_brackets`]. A failure after the `(` reports where the
/// destination started and how far the scan got, so the caller can skip
/// rescanning the same span from later brackets.
fn link_at(
    chars: &[char],
    brackets: &[u32],
    floor: usize,
    open: usize,
) -> Result<LinkAt, Option<(usize, usize)>> {
    let close = brackets
        .get(open)
        .and_then(|&close| usize::try_from(close).ok())
        .filter(|&close| close < chars.len() && close - open <= MAX_SPAN)
        .ok_or(None)?;
    if chars.get(close + 1) != Some(&'(') {
        return Err(None);
    }
    let start = close + 2;
    if start < floor {
        return Err(None);
    }
    destination(chars, close, start).map_err(|reached| Some((start, reached)))
}

/// The rest of [`link_at`] from the destination at `start`; `Err` is how
/// far the scan got.
fn destination(chars: &[char], close: usize, start: usize) -> Result<LinkAt, usize> {
    let skip_space = |mut k: usize| {
        while chars
            .get(k)
            .is_some_and(|c| c.is_whitespace() || *c == '\0')
        {
            k += 1;
        }
        k
    };
    let mut k = skip_space(start);
    let mut url = String::new();
    if chars.get(k) == Some(&'<') {
        k += 1;
        loop {
            match chars.get(k).ok_or(k)? {
                '>' => break,
                '<' | '\n' | '\0' => return Err(k),
                '\\' if chars.get(k + 1).is_some_and(char::is_ascii_punctuation) => {
                    url.push(chars[k + 1]);
                    k += 1;
                }
                c => url.push(*c),
            }
            k += 1;
            if url.len() > MAX_URL_BYTES {
                return Err(k);
            }
        }
        k += 1;
    } else {
        let mut parens = 0_usize;
        while let Some(&c) = chars.get(k) {
            match c {
                _ if c.is_whitespace() || c == '\0' => break,
                '(' => parens += 1,
                ')' if parens == 0 => break,
                ')' => parens -= 1,
                '\\' if chars.get(k + 1).is_some_and(char::is_ascii_punctuation) => {
                    k += 1;
                    url.push(chars[k]);
                    k += 1;
                    continue;
                }
                _ => {}
            }
            url.push(c);
            k += 1;
            if url.len() > MAX_URL_BYTES {
                return Err(k);
            }
        }
    }
    k = skip_space(k);
    if let Some(&quote) = chars.get(k).filter(|c| matches!(c, '"' | '\'' | '(')) {
        let end = if quote == '(' { ')' } else { quote };
        let mut m = k + 1;
        while chars.get(m).ok_or(m)? != &end {
            if m - k > MAX_SPAN {
                return Err(m);
            }
            m += if chars[m] == '\\' { 2 } else { 1 };
        }
        k = skip_space(m + 1);
    }
    if chars.get(k) == Some(&')') {
        Ok(LinkAt {
            close,
            url,
            end: k + 1,
        })
    } else {
        Err(k)
    }
}

/// `<scheme:…>` at `chars[open]`: the URL and the index after `>`.
fn autolink(chars: &[char], open: usize) -> Option<(String, usize)> {
    let limit = chars.len().min(open.saturating_add(MAX_SPAN));
    let close =
        (open + 1..limit).find(|&j| matches!(chars[j], '>' | '<') || chars[j].is_whitespace())?;
    if chars[close] != '>' {
        return None;
    }
    let url: String = chars[open + 1..close].iter().collect();
    let scheme = url.split_once(':')?.0;
    let valid = (2..=32).contains(&scheme.len())
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    valid.then_some((url, close + 1))
}

/// A bare `http(s)://` URL at `chars[start]`: the index after it, with
/// trailing punctuation (and an unbalanced `)`) left out.
fn bare_url(chars: &[char], start: usize) -> Option<usize> {
    let head: String = chars[start..]
        .iter()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    let scheme = ["https://", "http://"]
        .into_iter()
        .find(|s| head.starts_with(s))?;
    let limit = chars.len().min(start + MAX_URL_BYTES);
    let mut end = (start..limit)
        .find(|&j| chars[j].is_whitespace() || matches!(chars[j], '<' | '\0'))
        .unwrap_or(limit);
    // Unclosed `)`s at the end belong to the surrounding text.
    let mut unclosed = chars[start..end].iter().fold(0_isize, |n, &c| match c {
        ')' => n + 1,
        '(' => n - 1,
        _ => n,
    });
    while end > start + scheme.len() {
        let last = chars[end - 1];
        let trailing = matches!(
            last,
            '.' | ',' | ':' | ';' | '!' | '?' | '"' | '\'' | '*' | '_' | '~'
        );
        if last == ')' && unclosed > 0 {
            unclosed -= 1;
        } else if !trailing {
            break;
        }
        end -= 1;
    }
    (end > start + scheme.len()).then_some(end)
}

/// `&name;`, `&#123;` or `&#x7B;` at `chars[0]`: the char and its length.
fn entity(chars: &[char]) -> Option<(char, usize)> {
    let semi = chars.iter().take(12).position(|&c| c == ';')?;
    let body: String = chars[1..semi].iter().collect();
    let decoded = if let Some(number) = body.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse::<u32>().ok()?,
        };
        char::from_u32(code)
            .filter(|&c| c != '\0')
            .unwrap_or('\u{FFFD}')
    } else {
        match body.as_str() {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => '\u{A0}',
            "copy" => '©',
            "reg" => '®',
            "mdash" => '—',
            "ndash" => '–',
            "hellip" => '…',
            "rarr" => '→',
            "larr" => '←',
            "times" => '×',
            _ => return None,
        }
    };
    Some((decoded, semi + 1))
}

/// Pairs emphasis delimiters into nodes (closest opener first, bounded
/// look-back and depth); unpaired delimiters stay literal. Works on an
/// output stack, so pairing only ever drains the stack's tail.
fn emphasis(ctx: &mut Ctx, toks: Vec<Tok>) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    for tok in toks {
        let Tok::Delim {
            ch,
            mut count,
            open,
            close: true,
        } = tok
        else {
            out.push(tok);
            continue;
        };
        while count > 0 && (ch != '~' || count >= 2) && !ctx.cut {
            let lo = out.len().saturating_sub(MAX_LOOKBACK);
            let opener = (lo..out.len()).rev().find(|&j| {
                matches!(out[j], Tok::Delim { ch: c, count: n, open: true, .. }
                    if c == ch && n > 0 && (ch != '~' || n >= 2))
            });
            let Some(j) = opener else {
                break;
            };
            let depth = 1 + out[j + 1..]
                .iter()
                .map(|t| if let Tok::Node(_, d) = t { *d } else { 0 })
                .max()
                .unwrap_or(0);
            let Tok::Delim {
                count: opener_count,
                open: opener_open,
                ..
            } = &mut out[j]
            else {
                break;
            };
            if depth > MAX_INLINE_DEPTH {
                *opener_open = false;
                continue;
            }
            let used = if ch == '~' || (*opener_count >= 2 && count >= 2) {
                2
            } else {
                1
            };
            *opener_count -= used;
            count -= used;
            let inner: Vec<Tok> = out.drain(j + 1..).collect();
            let children = finish(ctx, inner);
            let kind = match (ch, used) {
                ('~', _) => Kind::Strike,
                (_, 2) => Kind::Strong,
                _ => Kind::Emphasis,
            };
            out.push(Tok::Node(ctx.node(kind, children), depth));
        }
        out.push(Tok::Delim {
            ch,
            count,
            open,
            close: true,
        });
    }
    out
}

/// Tokens to nodes: leftover delimiters become text; adjacent text merges.
fn finish(ctx: &mut Ctx, toks: Vec<Tok>) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::new();
    for tok in toks {
        let piece = match tok {
            Tok::Node(node, _) => {
                out.push(node);
                continue;
            }
            Tok::Text(text) => text,
            Tok::Delim { count: 0, .. } => continue,
            Tok::Delim { ch, count, .. } => core::iter::repeat_n(ch, count).collect(),
        };
        if let Some(Node {
            kind: Kind::Text(last),
            ..
        }) = out.last_mut()
        {
            last.push_str(&visible(piece));
        } else if !ctx.cut {
            out.push(ctx.node(Kind::Text(piece), Vec::new()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(node: &Node) -> Vec<&Kind> {
        node.children.iter().map(|c| &c.kind).collect()
    }

    fn text(t: &str) -> Kind {
        Kind::Text(t.to_owned())
    }

    /// Every node in the tree, depth first.
    fn walk(node: &Node) -> Vec<&Node> {
        let mut out = vec![node];
        for child in &node.children {
            out.extend(walk(child));
        }
        out
    }

    fn depth(node: &Node) -> usize {
        1 + node.children.iter().map(depth).max().unwrap_or(0)
    }

    fn links(node: &Node) -> Vec<(String, bool)> {
        walk(node)
            .into_iter()
            .filter_map(|n| match &n.kind {
                Kind::Link { shown, openable } => Some((shown.clone(), *openable)),
                _ => None,
            })
            .collect()
    }

    /// Bound on the whole tree's depth: root, nested containers (a list
    /// level is two), a table or heading, and emphasis.
    const DEPTH_BOUND: usize = 2 * MAX_DEPTH + MAX_INLINE_DEPTH + 8;

    #[test]
    fn paragraphs_emphasis_strong_and_code() {
        let tree = parse("Hello *you* and **me** with `x < y`.\n\nSecond.");
        assert_eq!(kinds(&tree), [&Kind::Paragraph, &Kind::Paragraph]);
        let p = &tree.children[0];
        assert_eq!(
            kinds(p),
            [
                &text("Hello "),
                &Kind::Emphasis,
                &text(" and "),
                &Kind::Strong,
                &text(" with "),
                &Kind::Code("x < y".into()),
                &text("."),
            ]
        );
        assert_eq!(p.children[1].text(), "you");
        assert_eq!(p.children[3].text(), "me");
        assert_eq!(tree.children[1].text(), "Second.");
    }

    #[test]
    fn emphasis_edge_cases() {
        let p = |s: &str| parse(s).children.remove(0);
        // Intraword underscores are not emphasis.
        assert_eq!(kinds(&p("snake_case_name")), [&text("snake_case_name")]);
        // Unmatched delimiters stay literal.
        assert_eq!(kinds(&p("2 * 3 = 6")), [&text("2 * 3 = 6")]);
        assert_eq!(p("**bold").text(), "**bold");
        // Nested and mixed.
        let mixed = p("***both*** and ~~gone~~ and _it_");
        let k = kinds(&mixed);
        assert!(matches!(k[0], Kind::Strong | Kind::Emphasis), "{k:?}");
        assert_eq!(mixed.children[0].text(), "both");
        assert_eq!(mixed.children[2].kind, Kind::Strike);
        assert_eq!(mixed.children[4].kind, Kind::Emphasis);
        // Escapes.
        assert_eq!(kinds(&p(r"\*not\* \`code\`")), [&text("*not* `code`")]);
    }

    #[test]
    fn soft_breaks_keep_the_newline_and_hard_breaks_are_br() {
        let tree = parse("a\nb  \nc\\\nd");
        assert_eq!(
            kinds(&tree.children[0]),
            [
                &text("a\nb"),
                &Kind::Break,
                &text("c"),
                &Kind::Break,
                &text("d")
            ]
        );
    }

    #[test]
    fn code_blocks_keep_their_text_verbatim() {
        let tree = parse("```rust\nfn main() { <b>hi</b> *x* }\n```\n\n    indented\n\n~~~\nopen");
        assert_eq!(
            kinds(&tree),
            [&Kind::CodeBlock, &Kind::CodeBlock, &Kind::CodeBlock]
        );
        assert_eq!(tree.children[0].text(), "fn main() { <b>hi</b> *x* }\n");
        assert_eq!(tree.children[1].text(), "indented\n");
        assert_eq!(tree.children[2].text(), "open\n");
        let inline = parse("``a ` b``");
        assert_eq!(kinds(&inline.children[0]), [&Kind::Code("a ` b".into())]);
    }

    #[test]
    fn lists_ordered_unordered_and_tasks() {
        let tree = parse("3. three\n4. four\n\n- a\n  - nested\n- [x] done\n- [ ] todo\n");
        assert_eq!(
            kinds(&tree),
            [&Kind::List { start: Some(3) }, &Kind::List { start: None }]
        );
        let ordered = &tree.children[0];
        assert_eq!(ordered.children.len(), 2);
        assert_eq!(kinds(&ordered.children[1]), [&text("four")]);
        let bullets = &tree.children[1];
        assert_eq!(bullets.children.len(), 3);
        assert_eq!(
            kinds(&bullets.children[0]),
            [&text("a"), &Kind::List { start: None }]
        );
        assert_eq!(
            kinds(&bullets.children[1]),
            [&Kind::Task(true), &text("done")]
        );
        assert_eq!(bullets.children[2].children[0].kind, Kind::Task(false));
    }

    #[test]
    fn loose_lists_keep_paragraphs_and_lazy_lines_continue() {
        let tree = parse("- one\n\n- two\n  more\nlazy\n");
        let list = &tree.children[0];
        assert_eq!(list.children.len(), 2);
        assert_eq!(kinds(&list.children[0]), [&Kind::Paragraph]);
        assert_eq!(list.children[1].text(), "two\nmore\nlazy");
    }

    #[test]
    fn headings_quotes_and_rules() {
        let tree = parse("# Big #\n###### Small\n\n> quoted\n> > twice\n\n---\nSetext\n===\n#nope");
        assert_eq!(
            kinds(&tree),
            [
                &Kind::Heading(1),
                &Kind::Heading(6),
                &Kind::BlockQuote,
                &Kind::Rule,
                &Kind::Heading(1),
                &Kind::Paragraph,
            ]
        );
        assert_eq!(tree.children[0].text(), "Big");
        let quote = &tree.children[2];
        assert_eq!(kinds(quote), [&Kind::Paragraph, &Kind::BlockQuote]);
        assert_eq!(tree.children[5].text(), "#nope");
    }

    #[test]
    fn tables_with_alignment() {
        let tree = parse("| a | b | c |\n|:--|:-:|--:|\n| 1 | `x|y` | 3 |\n| short |\n\nafter");
        let table = &tree.children[0];
        assert_eq!(table.kind, Kind::Table);
        assert_eq!(
            kinds(table),
            [&Kind::TableHead, &Kind::TableRow, &Kind::TableRow]
        );
        assert_eq!(
            kinds(&table.children[0]),
            [
                &Kind::TableCell(Align::Left),
                &Kind::TableCell(Align::Center),
                &Kind::TableCell(Align::Right)
            ]
        );
        let row = &table.children[1];
        assert_eq!(row.children[1].text(), "x|y");
        assert_eq!(row.children[2].text(), "3");
        // Short rows are padded to the head's width.
        assert_eq!(table.children[2].children.len(), 3);
        assert_eq!(tree.children[1].text(), "after");
        // Not a table without a matching delimiter row.
        assert_eq!(parse("a | b\nc | d").children[0].kind, Kind::Paragraph);
    }

    #[test]
    fn raw_html_is_literal_text() {
        let tree = parse("<script>alert(1)</script>\n\nhi <img src=x onerror=alert(1)> there");
        assert_eq!(kinds(&tree), [&Kind::Paragraph, &Kind::Paragraph]);
        assert_eq!(tree.children[0].text(), "<script>alert(1)</script>");
        assert_eq!(
            tree.children[1].text(),
            "hi <img src=x onerror=alert(1)> there"
        );
    }

    #[test]
    fn entities_decode_to_inert_text() {
        let tree = parse("&lt;script&gt;alert(1)&lt;/script&gt; &amp; &#60;b&#x3E; &bogus; &#0;");
        assert_eq!(
            tree.text(),
            "<script>alert(1)</script> & <b> &bogus; \u{FFFD}"
        );
        assert!(
            walk(&tree)
                .iter()
                .all(|n| matches!(n.kind, Kind::Root | Kind::Paragraph | Kind::Text(_)))
        );
    }

    #[test]
    fn links_show_the_url_and_only_http_opens() {
        let tree = parse(
            "[ok](https://example.com/a?b=1 \"title\") [js](javascript:alert(1)) \
             [JS](JaVaScRiPt:alert(1)) [data](data:text/html,<script>alert(1)</script>) \
             [rel](/etc/passwd) [mail](mailto:a@b.c) <https://auto.example> \
             [tab](java%09script:alert(1)) [http](HTTP://EXAMPLE.COM) \
             see https://bare.example/x_(y). and (http://paren.example)",
        );
        assert_eq!(
            links(&tree),
            [
                ("https://example.com/a?b=1".into(), true),
                ("javascript:alert(1)".into(), false),
                ("JaVaScRiPt:alert(1)".into(), false),
                ("data:text/html,<script>alert(1)</script>".into(), false),
                ("/etc/passwd".into(), false),
                ("mailto:a@b.c".into(), false),
                ("https://auto.example".into(), true),
                ("java%09script:alert(1)".into(), false),
                ("HTTP://EXAMPLE.COM".into(), true),
                ("https://bare.example/x_(y)".into(), true),
                ("http://paren.example".into(), true),
            ]
        );
        assert!(tree.text().contains("(y). and ("));
        // Brackets that aren't links are text.
        assert_eq!(parse("[x] and [y](").text(), "[x] and [y](");
    }

    #[test]
    fn urls_with_hidden_characters_are_shown_marked_and_not_openable() {
        assert!(is_openable("https://example.com/path"));
        assert!(!is_openable("https://exa\u{202E}moc.elpmaxe"));
        assert!(!is_openable("https://example.com/\u{200B}"));
        assert!(!is_openable("https://example.com/ a"));
        assert!(!is_openable("https://"));
        assert!(!is_openable("http:/example.com"));
        assert!(!is_openable("javascript://example.com/%0Aalert(1)"));
        assert!(!is_openable(&format!(
            "https://e.com/{}",
            "a".repeat(MAX_URL_BYTES)
        )));
        assert_eq!(
            shown_url("https://a\u{202E}b\u{7}c"),
            "https://a\u{FFFD}b\u{FFFD}c"
        );
        // A non-ASCII host never opens (homographs); a non-ASCII path does.
        assert!(!is_openable("https://exämple.com"));
        assert!(is_openable("https://example.com/ä"));
        let tree = parse("[x](https://exa\u{202E}moc.elpmaxe)");
        assert_eq!(
            links(&tree),
            [("https://exa\u{FFFD}moc.elpmaxe".into(), false)]
        );
    }

    #[test]
    fn images_are_never_loaded_only_their_alt_text_shown() {
        let tree = parse("![a *cat*](https://tracker.example/pixel.gif) ![](x.png)");
        let images: Vec<&Node> = walk(&tree)
            .into_iter()
            .filter(|n| n.kind == Kind::Image)
            .collect();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].text(), "a cat");
        assert_eq!(images[1].text(), "");
        assert!(!format!("{tree:?}").contains("tracker"));
    }

    #[test]
    fn deep_nesting_is_bounded_and_keeps_the_text() {
        let quotes = format!("{} deep", ">".repeat(5_000));
        let tree = parse(&quotes);
        assert!(depth(&tree) <= DEPTH_BOUND, "{}", depth(&tree));
        assert!(tree.text().contains("deep"));

        let mut list = String::new();
        for i in 0..300 {
            list.push_str(&"  ".repeat(i));
            list.push_str("- item\n");
        }
        let tree = parse(&list);
        assert!(depth(&tree) <= DEPTH_BOUND, "{}", depth(&tree));

        let emphasis = format!("{}x{}", "*_".repeat(2_000), "_*".repeat(2_000));
        let tree = parse(&emphasis);
        assert!(depth(&tree) <= DEPTH_BOUND, "{}", depth(&tree));
        assert!(tree.text().contains('x'));

        let brackets = format!("{}x{}", "[".repeat(20_000), "](u)".repeat(20_000));
        let tree = parse(&brackets);
        assert!(depth(&tree) <= DEPTH_BOUND, "{}", depth(&tree));
    }

    #[test]
    fn pathological_inputs_stay_fast() {
        let start = std::time::Instant::now();
        for hostile in [
            "[".repeat(100_000),
            "`".repeat(50_000) + &"a`".repeat(10_000),
            "*a".repeat(50_000),
            "<".repeat(100_000),
            "&".repeat(100_000),
            "> - ".repeat(30_000),
            "| a ".repeat(30_000) + "\n|" + &"-|".repeat(30_000),
            format!("https://a{} ", ")".repeat(2_000)).repeat(60),
        ] {
            let _ = parse(&hostile);
        }
        assert!(start.elapsed().as_secs() < 5, "{:?}", start.elapsed());
    }

    #[test]
    fn huge_inputs_are_cut_with_a_note() {
        let many = "- a\n".repeat(MAX_NODES);
        let tree = parse(&many);
        assert!(walk(&tree).len() <= MAX_NODES + 20);
        assert_eq!(
            tree.children.last().map(|n| &n.kind),
            Some(&Kind::Truncated)
        );

        let long = "é".repeat(MAX_INPUT_BYTES);
        let tree = parse(&long);
        assert_eq!(
            tree.children.last().map(|n| &n.kind),
            Some(&Kind::Truncated)
        );
        assert!(tree.text().len() <= MAX_INPUT_BYTES);

        let short = parse("fine");
        assert!(!walk(&short).iter().any(|n| n.kind == Kind::Truncated));
    }

    #[test]
    fn hostile_fixtures_render_inert() {
        let fixtures = [
            "<script>alert(1)</script>",
            "<SCRIPT SRC=//evil.example/x.js></SCRIPT>",
            "<iframe src=\"javascript:alert(1)\"></iframe>",
            "<a href=\"javascript:alert(1)\">x</a>",
            "[x](javascript:alert(1))",
            "[x](  javascript:alert(1)  )",
            "[x](<javascript:alert(1)>)",
            "[x](java\\script:alert(1))",
            "[x](vbscript:msgbox(1))",
            "[x](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)",
            "![x](javascript:alert(1))",
            "<svg onload=alert(1)>",
            "<style>body{display:none}</style>",
            "<!-- comment --><b onmouseover=alert(1)>hover</b>",
            "&#x3C;script&#x3E;alert(1)&#x3C;/script&#x3E;",
            "[a]: javascript:alert(1)\n\n[a]",
            "<javascript:alert(1)>",
            "<data:text/html;base64,PHNjcmlwdD4=>",
            "[x](https&#58;//evil.example)",
            "`<script>` and ```\n<script>\n```",
            "| <script> | [x](javascript:1) |\n|---|---|\n| <img src=x> | ok |",
        ];
        for fixture in fixtures {
            let tree = parse(fixture);
            for (url, openable) in links(&tree) {
                assert!(!openable, "{fixture}: {url} must not be openable");
            }
            for node in walk(&tree) {
                if let Kind::Link {
                    shown,
                    openable: true,
                } = &node.kind
                {
                    assert!(is_openable(shown), "{fixture}");
                }
            }
        }
    }

    #[test]
    fn clip_respects_char_boundaries() {
        assert_eq!(clip("héllo", 2), ("h", true));
        assert_eq!(clip("héllo", 3), ("hé", true));
        assert_eq!(clip("hi", 10), ("hi", false));
    }

    #[test]
    fn tabs_and_nul_are_normalized() {
        let tree = parse("-\tone\n\na\0b");
        assert_eq!(tree.children[0].kind, Kind::List { start: None });
        assert_eq!(tree.children[1].text(), "a\u{FFFD}b");
    }

    #[test]
    fn the_host_shown_is_the_host_opened() {
        for bad in [
            "https://good.com\u{2215}evil.com/x",
            "https://good.com\u{2044}evil.com/x",
            "https://good.com@evil.com/",
            "https://user:pw@evil.com",
            "https://evil.com\\@good.com",
            "https://evil.com\\good.com",
            "https://ex\u{0430}mple.com/",
            "https://exa\u{2800}mple.com/",
            "https://exämple.com",
            "https:///path",
            "https://?q",
            "https://example.com/a\\b",
        ] {
            assert!(!is_openable(bad), "{bad}");
            assert_eq!(url_host(bad), None, "{bad}");
        }
        for (good, host) in [
            ("https://example.com", "example.com"),
            ("https://example.com:8443/a?b#c", "example.com"),
            ("http://xn--exmple-cua.com/p", "xn--exmple-cua.com"),
            ("https://[::1]:8080/x", "[::1]"),
            ("https://example.com/ünïcode-path?q=é", "example.com"),
            ("HTTPS://EXAMPLE.COM#top", "EXAMPLE.COM"),
        ] {
            assert!(is_openable(good), "{good}");
            assert_eq!(url_host(good), Some(host), "{good}");
        }
        // Parsed links follow the same rule.
        let tree = parse("[ok](https://good.com@evil.com) <https://good.com\u{2215}evil.com>");
        assert!(links(&tree).iter().all(|(_, openable)| !openable));
    }

    #[test]
    fn hidden_characters_in_text_are_replaced() {
        let tree = parse("abc\u{202E}fed `co\u{2066}de` &#x202E;x &#8203;y\u{200D}z");
        let text = tree.text();
        assert!(!text.contains('\u{202E}') && !text.contains('\u{2066}'));
        assert!(!text.contains('\u{200B}'));
        assert_eq!(
            text,
            "abc\u{FFFD}fed co\u{FFFD}de \u{FFFD}x \u{FFFD}y\u{200D}z"
        );
        let code = parse("```\na\u{202E}b\n```");
        assert_eq!(code.text(), "a\u{FFFD}b\n");
        // Controls stay visible in Markdown, entities included.
        assert_eq!(
            parse("a\rb &#13;c &#9;d").text(),
            "a\u{FFFD}b \u{FFFD}c \u{FFFD}d"
        );
    }

    #[test]
    fn code_text_keeps_tabs_and_line_breaks_only() {
        assert_eq!(
            visible_code("a\tb\r\nc\rd\u{202E}e\u{7}".to_owned()),
            "a\tb\nc\u{FFFD}d\u{FFFD}e\u{FFFD}"
        );
    }

    #[test]
    fn budgets_hold_in_emphasis_and_tables() {
        let emphasis = "*a* ".repeat(MAX_NODES);
        let tree = parse(&emphasis);
        assert!(walk(&tree).len() <= MAX_NODES + 20, "{}", walk(&tree).len());

        let head = format!("|{}\n|{}\n", " a |".repeat(200), "-|".repeat(200));
        let tree = parse(&(head.clone() + &"| x ".repeat(200).add_line().repeat(3)));
        let table = &tree.children[0];
        assert_eq!(table.kind, Kind::Table);
        assert_eq!(table.children[0].children.len(), MAX_TABLE_COLUMNS);

        let rows = format!("| a |\n|---|\n{}", "| r |\n".repeat(MAX_TABLE_ROWS + 50));
        let tree = parse(&rows);
        assert_eq!(tree.children[0].children.len(), MAX_TABLE_ROWS + 1);

        let cells = format!(
            "| a | b |\n|---|---|\n{}",
            "| *x* | *y* |\n".repeat(MAX_NODES)
        );
        let tree = parse(&cells);
        assert!(walk(&tree).len() <= MAX_NODES + 20, "{}", walk(&tree).len());
    }

    trait AddLine {
        fn add_line(self) -> String;
    }

    impl AddLine for String {
        fn add_line(self) -> String {
            self + "\n"
        }
    }

    #[test]
    fn failed_long_destinations_are_not_rescanned() {
        let start = std::time::Instant::now();
        let _ = parse(&"[a](".repeat(30_000));
        let _ = parse(&format!("[a]({}", "([b](x".repeat(20_000)));
        assert!(start.elapsed().as_millis() < 2_000, "{:?}", start.elapsed());
        // Ordinary text around a failed link still links.
        let tree = parse("[a](x y z) then [b](https://ok.example)");
        assert_eq!(links(&tree), [("https://ok.example".into(), true)]);
    }
}
