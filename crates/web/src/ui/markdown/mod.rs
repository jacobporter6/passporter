//! Safe Markdown for agent and user text in the thread (design §8).
//!
//! Text is parsed by a small purpose-built parser ([`parse`]) into a
//! [`Node`] tree (pure, tested on the host), and the tree becomes DOM nodes
//! created one by one (`createElement`, `createTextNode`) inside a Leptos
//! component. Nothing is ever parsed as HTML by the browser:
//!
//! - there is no HTML in the grammar: `<script>` is text;
//! - entities decode into text nodes, so `&lt;script&gt;` shows as
//!   `<script>` and does nothing;
//! - images are never fetched: their alt text is shown instead;
//! - links are not anchors: they show their text *and* their full URL, and
//!   only `http(s)` URLs without hidden or control characters can be opened,
//!   after an "Open <URL>?" confirmation ([`LinkConfirm`]); the opening
//!   anchor has `target=_blank rel="noopener noreferrer"`;
//! - nesting, node count, input size and every scan are bounded, so hostile
//!   input can't build a tree deep or large enough to hurt the page.
//!
//! No inline styles are emitted (table alignment is a class), so the strict
//! CSP (`style-src 'self'`) holds.
//!
//! Why not a crate: `pulldown-cmark` (the lightest full `CommonMark` parser)
//! added ~75 KiB gzipped to the WASM, ~19% of the whole 400 KiB budget,
//! mostly for HTML-entity and Unicode tables and constructs chat text never
//! uses. This parser covers what agents write in a few kilobytes, and has
//! no HTML concept to get wrong.

pub mod link;
mod parse;

pub use parse::{
    Align, Kind, MAX_DEPTH, MAX_INLINE_DEPTH, MAX_INPUT_BYTES, MAX_NODES, MAX_URL_BYTES, Node,
    is_openable, parse, shown_url, url_host, visible_code,
};

use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::extras::Surface;
use crate::extras::page::Extra;

/// The note shown where text was cut.
pub const TRUNCATED_NOTE: &str = "… (cut: too long to show here)";

/// The link waiting for "Open <URL>?", shared by a thread's messages.
pub type LinkPrompt = RwSignal<Option<String>>;

/// `source` as Markdown, in a `div.md`.
///
/// The elements are created one by one with `createElement` and
/// `createTextNode` (see [`build`]): text is only ever a text node, and the
/// only attributes set are fixed names with values this module chose (or a
/// URL that passed [`is_openable`], as `data-url`). Building the DOM
/// directly keeps the renderer a few kilobytes, where one Leptos view per
/// construct cost tens. Link taps are delegated to the container.
#[component]
pub fn Markdown(
    /// The untrusted text.
    #[prop(into)]
    source: Signal<String>,
    /// Where a tapped link goes for confirmation.
    links: LinkPrompt,
) -> impl IntoView {
    let host = NodeRef::<html::Div>::new();
    Effect::new(move |_| {
        let Some(el) = host.get() else {
            return;
        };
        let tree = source.with(|s| parse(s));
        el.set_text_content(None);
        if let Some(document) = el.owner_document() {
            build(&document, &el, &tree);
        }
    });
    let on_click = move |ev: leptos::ev::MouseEvent| {
        let url = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest("button.md-link").ok().flatten())
            .and_then(|button| button.get_attribute("data-url"))
            .filter(|url| is_openable(url));
        if let Some(url) = url {
            links.set(Some(url));
        }
    };
    view! { <div class="md" node_ref=host on:click=on_click></div> }
}

/// The element (tag, class) for a container kind; `None` for kinds built
/// specially or with no element of their own.
const fn element(kind: &Kind) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        Kind::Paragraph => ("p", "md-p"),
        // Headings are scaled down two levels: the page owns h1/h2.
        Kind::Heading(1) => ("h3", "md-h md-h1"),
        Kind::Heading(2) => ("h4", "md-h md-h2"),
        Kind::Heading(3) => ("h5", "md-h md-h3"),
        Kind::Heading(_) => ("h6", "md-h md-h4"),
        Kind::BlockQuote => ("blockquote", "md-quote"),
        Kind::List { start: Some(_) } => ("ol", "md-list"),
        Kind::List { start: None } => ("ul", "md-list"),
        Kind::Item => ("li", ""),
        Kind::TableHead => ("thead", ""),
        Kind::TableRow => ("tr", ""),
        Kind::TableCell(Align::None) => ("td", ""),
        Kind::TableCell(Align::Left) => ("td", "md-left"),
        Kind::TableCell(Align::Center) => ("td", "md-center"),
        Kind::TableCell(Align::Right) => ("td", "md-right"),
        Kind::Emphasis => ("em", ""),
        Kind::Strong => ("strong", ""),
        Kind::Strike => ("s", ""),
        Kind::Image => ("span", "md-image"),
        Kind::Break => ("br", ""),
        Kind::Rule => ("hr", "md-rule"),
        Kind::Truncated => ("p", "md-p md-cut"),
        Kind::Root
        | Kind::CodeBlock
        | Kind::Task(_)
        | Kind::Table
        | Kind::Link { .. }
        | Kind::Text(_)
        | Kind::Code(_) => return None,
    })
}

fn create(document: &web_sys::Document, tag: &str, class: &str) -> Option<web_sys::Element> {
    let el = document.create_element(tag).ok()?;
    if !class.is_empty() {
        el.set_class_name(class);
    }
    Some(el)
}

fn append_text(document: &web_sys::Document, parent: &web_sys::Node, text: &str) {
    let _ = parent.append_child(&document.create_text_node(text));
}

fn attr(el: &web_sys::Element, name: &str, value: &str) {
    let _ = el.set_attribute(name, value);
}

/// Appends `node`'s children (as elements and text nodes) to `parent`.
/// Recursion depth is bounded by [`MAX_DEPTH`] in the tree.
fn build(document: &web_sys::Document, parent: &web_sys::Node, node: &Node) {
    for child in &node.children {
        build_one(document, parent, child);
    }
}

fn build_one(document: &web_sys::Document, parent: &web_sys::Node, node: &Node) {
    let el = match &node.kind {
        Kind::Root => return build(document, parent, node),
        Kind::Text(text) => return append_text(document, parent, text),
        Kind::Code(code) => {
            let Some(el) = create(document, "code", "md-inline-code") else {
                return;
            };
            append_text(document, &el, code);
            el
        }
        Kind::CodeBlock => {
            let (Some(pre), Some(code)) = (
                create(document, "pre", "code md-code"),
                create(document, "code", ""),
            ) else {
                return;
            };
            // Focusable, so keyboard users can scroll it sideways.
            attr(&pre, "tabindex", "0");
            append_text(document, &code, &node.text());
            let _ = pre.append_child(&code);
            pre
        }
        Kind::Task(done) => {
            let Some(el) = create(document, "span", "md-task") else {
                return;
            };
            attr(&el, "role", "img");
            attr(&el, "aria-label", if *done { "done" } else { "not done" });
            append_text(document, &el, if *done { "☑" } else { "☐" });
            el
        }
        Kind::Table => {
            let (Some(wrap), Some(table)) = (
                create(document, "div", "md-table-wrap"),
                create(document, "table", "md-table"),
            ) else {
                return;
            };
            attr(&wrap, "tabindex", "0");
            attr(&wrap, "role", "region");
            attr(&wrap, "aria-label", "Table");
            build(document, &table, node);
            let _ = wrap.append_child(&table);
            wrap
        }
        Kind::TableHead => {
            // pulldown-cmark puts the head's cells directly in it.
            let (Some(head), Some(row)) =
                (create(document, "thead", ""), create(document, "tr", ""))
            else {
                return;
            };
            for cell in &node.children {
                if let Kind::TableCell(align) = cell.kind
                    && let Some((_, class)) = element(&Kind::TableCell(align))
                    && let Some(th) = create(document, "th", class)
                {
                    attr(&th, "scope", "col");
                    build(document, &th, cell);
                    let _ = row.append_child(&th);
                }
            }
            let _ = head.append_child(&row);
            head
        }
        Kind::Link { shown, openable } => {
            let Some(el) = link(document, node, shown, *openable) else {
                return;
            };
            el
        }
        kind => {
            let Some((tag, class)) = element(kind) else {
                return;
            };
            let Some(el) = create(document, tag, class) else {
                return;
            };
            match kind {
                Kind::List { start: Some(start) } => {
                    attr(&el, "start", &start.to_string());
                    build(document, &el, node);
                }
                Kind::Image => {
                    let alt = node.text();
                    let alt = if alt.trim().is_empty() { "image" } else { &alt };
                    append_text(document, &el, &format!("[image: {alt}]"));
                }
                Kind::Truncated => append_text(document, &el, TRUNCATED_NOTE),
                _ => build(document, &el, node),
            }
            el
        }
    };
    let _ = parent.append_child(&el);
}

/// A link: its text, then its full URL (unless the text is the URL). Only
/// openable links are buttons (carrying the URL as `data-url`); the tap is
/// handled by [`Markdown`], which asks first.
fn link(
    document: &web_sys::Document,
    node: &Node,
    shown: &str,
    openable: bool,
) -> Option<web_sys::Element> {
    let el = if openable {
        let button = create(document, "button", "md-link")?;
        attr(&button, "type", "button");
        attr(&button, "data-url", shown);
        button
    } else {
        create(document, "span", "md-link md-link-inert")?
    };
    let label = node.text();
    if !label.trim().is_empty() && label.trim() != shown {
        let text = create(document, "span", "md-link-text")?;
        build(document, &text, node);
        let _ = el.append_child(&text);
    }
    let url = create(document, "span", "md-url")?;
    append_text(document, &url, shown);
    let _ = el.append_child(&url);
    if !openable {
        let hint = create(document, "span", "visually-hidden")?;
        append_text(document, &hint, " (not opened: unsupported address)");
        let _ = el.append_child(&hint);
    }
    Some(el)
}

/// "Open <URL>?" for the link in `prompt`, drawn by the extras module
/// ([`link`]), which closes it by clearing `prompt`.
#[component]
pub fn LinkConfirm(prompt: LinkPrompt) -> impl IntoView {
    let asking = Memo::new(move |_| prompt.with(Option::is_some));
    move || {
        asking.get().then(|| {
            view! {
                <Extra
                    surface=Surface::Link
                    feed=Signal::derive(move || prompt.get().unwrap_or_default())
                    on_close=Callback::new(move |()| prompt.set(None))
                />
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_elements_are_created() {
        let allowed = [
            "p",
            "h3",
            "h4",
            "h5",
            "h6",
            "blockquote",
            "ol",
            "ul",
            "li",
            "thead",
            "tr",
            "td",
            "em",
            "strong",
            "s",
            "span",
            "br",
            "hr",
        ];
        let kinds = [
            Kind::Paragraph,
            Kind::Heading(1),
            Kind::Heading(2),
            Kind::Heading(3),
            Kind::Heading(6),
            Kind::BlockQuote,
            Kind::List { start: Some(2) },
            Kind::List { start: None },
            Kind::Item,
            Kind::TableHead,
            Kind::TableRow,
            Kind::TableCell(Align::Right),
            Kind::Emphasis,
            Kind::Strong,
            Kind::Strike,
            Kind::Image,
            Kind::Break,
            Kind::Rule,
            Kind::Truncated,
        ];
        for kind in kinds {
            let (tag, _) = element(&kind).unwrap();
            assert!(allowed.contains(&tag), "{kind:?} -> {tag}");
        }
        // Built specially (code, links, tables, tasks) or not elements.
        assert!(element(&Kind::Text("<b>".into())).is_none());
        assert!(
            element(&Kind::Link {
                shown: String::new(),
                openable: false
            })
            .is_none()
        );
    }
}
