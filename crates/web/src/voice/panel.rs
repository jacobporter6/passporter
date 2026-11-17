//! The dictation strip, drawn by the dictation WASM into an element the
//! app hands it (design §6.3): a stack of rows, one per note on its way
//! (upload and transcription progress with a Cancel, a card to retry, or
//! with review on the transcript card, whose "Use" fills the composer
//! through the app), then the live meter while recording (its button
//! sends).
//!
//! Plain DOM, not Leptos: this module has no Leptos in it, which is what
//! keeps it small. What to show is decided by the pure [`rows`]; this
//! only builds it, row by row: a row is rebuilt only when what it shows
//! changed, so one note's progress never resets a transcript being edited
//! in another. Each row carries a `data-key` and its buttons a
//! `data-action`, read by one click listener on the root, so redrawing
//! from inside a click never drops the handler that is running.
//!
//! [`rows`]: super::phase::rows

use std::rc::Weak;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Document, Element, Event, HtmlTextAreaElement};

use super::engine::Dictation;
use super::level::METER_BARS;
use super::phase::{Action, RowKey, Strip, bar_step, clock};
use crate::platform;
use crate::ui::icons;

const SVG: &str = "http://www.w3.org/2000/svg";
const SMALL_BUTTON: &str = "button button-secondary button-small";
const PRIMARY_BUTTON: &str = "button button-primary button-small";

/// What a button on the strip does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// Stop (or cancel) the recording.
    Stop,
    /// Stop delivering the note and forget it.
    Cancel,
    /// Retry the stored clip.
    Retry,
    /// Forget the stored clip.
    Discard,
    /// Clear a failure that left nothing stored.
    Dismiss,
    /// Put the transcript in the composer.
    Use,
    /// Forget the transcript and record again.
    Rerecord,
}

impl Act {
    const fn name(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Cancel => "cancel",
            Self::Retry => "retry",
            Self::Discard => "discard",
            Self::Dismiss => "dismiss",
            Self::Use => "use",
            Self::Rerecord => "rerecord",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            Self::Stop,
            Self::Cancel,
            Self::Retry,
            Self::Discard,
            Self::Dismiss,
            Self::Use,
            Self::Rerecord,
        ]
        .into_iter()
        .find(|act| act.name() == name)
    }

    const fn of(action: Action) -> Self {
        match action {
            Action::Retry { .. } => Self::Retry,
            Action::Cancel => Self::Cancel,
            Action::Discard => Self::Discard,
            Action::Dismiss => Self::Dismiss,
        }
    }
}

/// A row's `data-key`.
fn key_name(key: RowKey) -> String {
    match key {
        RowKey::Take => "take".to_owned(),
        RowKey::Note(key) => key.to_string(),
    }
}

/// The row a `data-key` names.
fn parse_key(name: &str) -> Option<RowKey> {
    if name == "take" {
        Some(RowKey::Take)
    } else {
        name.parse().ok().map(RowKey::Note)
    }
}

/// One drawn row.
struct Row {
    key: RowKey,
    strip: Strip,
    element: Element,
}

/// The strip in one mounted element.
pub struct Panel {
    root: Element,
    document: Document,
    /// The rows drawn, top to bottom.
    rows: Vec<Row>,
    /// The meter's bars and clock, while recording.
    bars: Vec<Element>,
    time: Option<Element>,
    /// The root's `click` and `input` listeners.
    listeners: [Closure<dyn FnMut(Event)>; 2],
}

impl core::fmt::Debug for Panel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Panel").finish_non_exhaustive()
    }
}

impl Panel {
    /// Draws into `root` from now on; its buttons act on `dictation`.
    pub fn new(root: Element, dictation: Weak<Dictation>) -> Option<Self> {
        let document = root.owner_document()?;
        let on_click = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            let Some(button) = event
                .target()
                .and_then(|t| t.dyn_into::<Element>().ok())
                .and_then(|t| t.closest("[data-action]").ok().flatten())
            else {
                return;
            };
            let act = button
                .get_attribute("data-action")
                .and_then(|name| Act::parse(&name));
            let key = button
                .closest("[data-key]")
                .ok()
                .flatten()
                .and_then(|row| row.get_attribute("data-key"))
                .and_then(|name| parse_key(&name));
            if let (Some(act), Some(key), Some(dictation)) = (act, key, dictation.upgrade()) {
                dictation.act(act, key);
            }
        });
        // "Use" waits for some text.
        let on_input = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            let Some(text) = event
                .target()
                .and_then(|t| t.dyn_into::<HtmlTextAreaElement>().ok())
            else {
                return;
            };
            let empty = text.value().trim().is_empty();
            if let Some(Ok(Some(button))) = text
                .closest(".dictation-card")
                .ok()
                .flatten()
                .map(|card| card.query_selector("[data-action=use]"))
            {
                let _ = button.toggle_attribute_with_force("disabled", empty);
            }
        });
        for (name, listener) in [("click", &on_click), ("input", &on_input)] {
            root.add_event_listener_with_callback(name, listener.as_ref().unchecked_ref())
                .ok()?;
        }
        Some(Self {
            root,
            document,
            rows: Vec::new(),
            bars: Vec::new(),
            time: None,
            listeners: [on_click, on_input],
        })
    }

    /// Whether this panel draws into `root`.
    #[must_use]
    pub fn is_in(&self, root: &Element) -> bool {
        self.root == *root
    }

    /// The edited transcript on note `key`'s card, while it shows.
    #[must_use]
    pub fn transcript(&self, key: u32) -> Option<String> {
        self.rows
            .iter()
            .find(|row| row.key == RowKey::Note(key))
            .and_then(|row| row.element.query_selector("textarea").ok().flatten())
            .and_then(|t| t.dyn_into::<HtmlTextAreaElement>().ok())
            .map(|t| t.value())
    }

    /// Makes the strip show `rows`, top to bottom. Rows that show the same
    /// as before stay as they are (with any edits and focus in them).
    pub fn draw(&mut self, rows: &[(RowKey, Strip)]) {
        if let Err(error) = self.reconcile(rows) {
            platform::log_error("could not draw the dictation strip", &error);
        }
    }

    fn reconcile(&mut self, rows: &[(RowKey, Strip)]) -> Result<(), JsValue> {
        let mut old = core::mem::take(&mut self.rows);
        if !rows
            .iter()
            .any(|(_, strip)| matches!(strip, Strip::Live { .. }))
        {
            self.bars.clear();
            self.time = None;
        }
        for (key, strip) in rows {
            let kept = old.iter().position(|row| row.key == *key);
            let element = match kept.map(|i| old.remove(i)) {
                Some(row) if row.strip == *strip => row.element,
                Some(row) => {
                    let element = self.build(*key, strip)?;
                    row.element.replace_with_with_node_1(&element)?;
                    element
                }
                None => self.build(*key, strip)?,
            };
            self.rows.push(Row {
                key: *key,
                strip: strip.clone(),
                element,
            });
        }
        for gone in old {
            gone.element.remove();
        }
        // In order, moving only rows that are out of place.
        let children = self.root.children();
        for (i, row) in self.rows.iter().enumerate() {
            let at = children.item(u32::try_from(i).unwrap_or(u32::MAX));
            if at.as_ref() != Some(&row.element) {
                self.root
                    .insert_before(&row.element, at.as_ref().map(AsRef::as_ref))?;
            }
        }
        Ok(())
    }

    /// Moves the meter (while recording).
    pub fn meter(&self, bars: &[u8], elapsed_ms: u64) {
        for (bar, percent) in self.bars.iter().zip(bars) {
            let _ = bar.set_attribute("data-step", &bar_step(*percent).to_string());
        }
        if let Some(time) = &self.time {
            time.set_text_content(Some(&clock(elapsed_ms)));
        }
    }

    /// A detached row for `strip`.
    fn build(&mut self, key: RowKey, strip: &Strip) -> Result<Element, JsValue> {
        let root = self.document.create_element("div")?;
        root.set_class_name("dictation-row");
        root.set_attribute("data-key", &key_name(key))?;
        match strip {
            Strip::Empty => {}
            Strip::Live { opening, send } => {
                let (class, hint, label) = match (*opening, *send) {
                    (true, _) => (
                        "dictation-live dictation-opening",
                        "Waiting for the microphone…",
                        "Cancel",
                    ),
                    (false, true) => ("dictation-live", "Tap the mic to send", "Send"),
                    (false, false) => ("dictation-live", "Stops when you pause", "Stop"),
                };
                let live = self.add(&root, "div", class, None)?;
                live.set_attribute("role", "status")?;
                live.set_attribute("aria-label", "Recording")?;
                self.add(&live, "span", "rec-dot", None)?
                    .set_attribute("aria-hidden", "true")?;
                let meter = self.add(&live, "span", "meter", None)?;
                meter.set_attribute("aria-hidden", "true")?;
                self.bars.clear();
                for _ in 0..METER_BARS {
                    let bar = self.add(&meter, "span", "meter-bar", None)?;
                    self.bars.push(bar);
                }
                self.time = Some(self.add(&live, "span", "rec-time", Some("0:00"))?);
                self.add(&live, "span", "rec-hint", Some(hint))?;
                self.button(&live, SMALL_BUTTON, label, Act::Stop)?;
            }
            Strip::Status { text, busy, cancel } => {
                let class = if *busy {
                    "dictation-status dictation-busy"
                } else {
                    "dictation-status"
                };
                let line = self.add(&root, "div", class, None)?;
                line.set_attribute("role", "status")?;
                self.add(&line, "span", "dictation-status-text", Some(text))?;
                if let Some(cancel) = cancel {
                    self.button(&line, SMALL_BUTTON, cancel.label(), Act::of(*cancel))?;
                }
            }
            Strip::Card {
                alert,
                title,
                message,
                note,
                actions,
            } => {
                let card = self.add(&root, "div", "outcome-card", None)?;
                card.set_attribute("role", if *alert { "alert" } else { "status" })?;
                if let Some(title) = title {
                    self.add(&card, "p", "outcome-title", Some(title))?;
                }
                self.add(&card, "p", "outcome-text", Some(message))?;
                if let Some(note) = note {
                    self.add(&card, "p", "outcome-text", Some(note))?;
                }
                let row = self.add(&card, "div", "outcome-actions", None)?;
                for action in actions {
                    let class = if action.is_primary() {
                        PRIMARY_BUTTON
                    } else {
                        SMALL_BUTTON
                    };
                    self.button(&row, class, action.label(), Act::of(*action))?;
                }
            }
            Strip::Transcript { title, text, note } => {
                self.transcript_card(&root, key, title, text, *note)?;
            }
        }
        Ok(root)
    }

    /// The transcript card: `note` (why the take stopped, if it says),
    /// then the editable `text` and its actions.
    fn transcript_card(
        &self,
        root: &Element,
        key: RowKey,
        heading: &str,
        text: &str,
        note: Option<&str>,
    ) -> Result<(), JsValue> {
        let card = self.add(root, "div", "outcome-card dictation-card", None)?;
        card.set_attribute("role", "region")?;
        card.set_attribute("aria-label", "Transcript")?;
        let title = self.add(&card, "p", "outcome-title", None)?;
        let icon = self.icon(icons::MIC)?;
        title.append_child(&icon)?;
        title.append_with_str_1(heading)?;
        if let Some(note) = note {
            self.add(&card, "p", "outcome-text", Some(note))?;
        }
        let heard = !text.trim().is_empty();
        if heard {
            let label = self.add(&card, "label", "visually-hidden", Some("Transcript"))?;
            let id = format!("dictation-text-{}", key_name(key));
            label.set_attribute("for", &id)?;
            let area: HtmlTextAreaElement = self
                .add(&card, "textarea", "composer-input dictation-text", None)?
                .unchecked_into();
            area.set_id(&id);
            area.set_rows(2);
            area.set_value(text);
        } else {
            self.add(&card, "p", "outcome-text", Some("No speech was heard."))?;
        }
        let row = self.add(&card, "div", "outcome-actions", None)?;
        if heard {
            self.button(&row, PRIMARY_BUTTON, "Use", Act::Use)?;
        }
        self.button(&row, SMALL_BUTTON, "Re-record", Act::Rerecord)?;
        self.button(&row, SMALL_BUTTON, "Discard", Act::Discard)?;
        Ok(())
    }

    /// Appends a `tag` with `class` (and `text`) to `parent`.
    fn add(
        &self,
        parent: &Element,
        tag: &str,
        class: &str,
        text: Option<&str>,
    ) -> Result<Element, JsValue> {
        let element = self.document.create_element(tag)?;
        element.set_class_name(class);
        if text.is_some() {
            element.set_text_content(text);
        }
        parent.append_child(&element)?;
        Ok(element)
    }

    fn button(&self, parent: &Element, class: &str, label: &str, act: Act) -> Result<(), JsValue> {
        let button = self.add(parent, "button", class, Some(label))?;
        button.set_attribute("type", "button")?;
        button.set_attribute("data-action", act.name())
    }

    /// The app's icon markup (`crate::ui::icons::Icon`), decorative.
    fn icon(&self, paths: icons::Paths) -> Result<Element, JsValue> {
        let svg = self.document.create_element_ns(Some(SVG), "svg")?;
        svg.set_attribute("class", "icon")?;
        svg.set_attribute("viewBox", "0 0 24 24")?;
        svg.set_attribute("aria-hidden", "true")?;
        svg.set_attribute("focusable", "false")?;
        for d in paths {
            let path = self.document.create_element_ns(Some(SVG), "path")?;
            path.set_attribute("d", d)?;
            svg.append_child(&path)?;
        }
        Ok(svg)
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        for (name, listener) in [("click", &self.listeners[0]), ("input", &self.listeners[1])] {
            let _ = self
                .root
                .remove_event_listener_with_callback(name, listener.as_ref().unchecked_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_act_reads_back_from_its_button() {
        for act in [
            Act::Stop,
            Act::Cancel,
            Act::Retry,
            Act::Discard,
            Act::Dismiss,
            Act::Use,
            Act::Rerecord,
        ] {
            assert_eq!(Act::parse(act.name()), Some(act));
        }
        assert_eq!(Act::parse("send"), None);
        assert_eq!(Act::of(Action::Retry { now: true }), Act::Retry);
        assert_eq!(Act::of(Action::Cancel), Act::Cancel);
    }

    #[test]
    fn every_row_reads_back_from_its_key() {
        for key in [RowKey::Take, RowKey::Note(0), RowKey::Note(41)] {
            assert_eq!(parse_key(&key_name(key)), Some(key));
        }
        assert_eq!(parse_key("note"), None);
    }
}
