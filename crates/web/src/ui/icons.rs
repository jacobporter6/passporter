//! A small stroke icon set (24×24 grid, styled by `.icon` in CSS, so icons
//! follow `currentColor`). Inline SVG elements, not markup strings: nothing
//! here conflicts with the strict CSP.

use leptos::prelude::*;

/// Path data for one icon.
pub type Paths = &'static [&'static str];

/// Microphone.
pub const MIC: Paths = &[
    "M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3z",
    "M19 11a7 7 0 0 1-14 0",
    "M12 18v3",
];
/// Inbox tray: the agent list.
pub const INBOX: Paths = &[
    "M3 13h5l2 3h4l2-3h5",
    "M5.6 5h12.8L21 13v5a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1v-5z",
];
/// Sliders: settings.
pub const SLIDERS: Paths = &[
    "M4 7h9",
    "M19 7h1",
    "M4 17h3",
    "M13 17h7",
    "M13 7a3 3 0 1 0 6 0a3 3 0 1 0-6 0",
    "M7 17a3 3 0 1 0 6 0a3 3 0 1 0-6 0",
];
/// Chevron pointing left: back.
pub const BACK: Paths = &["M15 18l-6-6 6-6"];
/// Chevron pointing right: drill in.
pub const CHEVRON: Paths = &["M9 6l6 6-6 6"];
/// QR code.
pub const QR: Paths = &[
    "M4 4h6v6H4z",
    "M14 4h6v6h-6z",
    "M4 14h6v6H4z",
    "M14 14h2v2h-2z",
    "M18 18h2v2h-2z",
    "M14 19h1",
    "M19 14h1",
];
/// Check mark.
pub const CHECK: Paths = &["M5 12.5l4.5 4.5L19 7"];
/// Crossed-out signal: offline.
pub const OFFLINE: Paths = &[
    "M3 3l18 18",
    "M8.5 16.4a5 5 0 0 1 7 0",
    "M5 12.9a10 10 0 0 1 4.3-2.6",
    "M19 12.9a10 10 0 0 0-2.1-1.5",
    "M12 20h.01",
];
/// Pulse line: work in progress.
pub const PULSE: Paths = &["M3 12h4l3-7 4 14 3-7h4"];
/// Phone: this device.
pub const PHONE: Paths = &[
    "M8 3h8a1 1 0 0 1 1 1v16a1 1 0 0 1-1 1H8a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z",
    "M11 18h2",
];
/// Hand raised: needs a decision.
pub const HAND: Paths = &[
    "M8 13V5.5a1.5 1.5 0 0 1 3 0V11",
    "M11 10V4.5a1.5 1.5 0 0 1 3 0V11",
    "M14 10.5V6a1.5 1.5 0 0 1 3 0v7a7 7 0 0 1-7 7h-.5a6 6 0 0 1-5-2.7L3 14.5a1.5 1.5 0 0 1 2.5-1.7L8 15",
];

/// Key: the bridge token.
pub const KEY: Paths = &[
    "M8 15a4 4 0 1 0 0-8a4 4 0 1 0 0 8z",
    "M12 11h9",
    "M18 11v3",
    "M21 11v2",
];
/// Paper plane: send.
pub const SEND: Paths = &["M4 12l16-8-6 16-2.5-6.5z", "M11.5 13.5L20 4"];
/// Keypad grid.
pub const KEYPAD: Paths = &[
    "M5 5h2v2H5z",
    "M11 5h2v2h-2z",
    "M17 5h2v2h-2z",
    "M5 11h2v2H5z",
    "M11 11h2v2h-2z",
    "M17 11h2v2h-2z",
    "M5 17h2v2H5z",
    "M11 17h2v2h-2z",
    "M17 17h2v2h-2z",
];
/// Keyboard: type a message.
pub const KEYBOARD: Paths = &[
    "M3 7a1 1 0 0 1 1-1h16a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z",
    "M7 10h.01",
    "M11 10h.01",
    "M15 10h.01",
    "M8 14h8",
];
/// Previous agent in the segment (distinct from the back chevron).
pub const ARROW_LEFT: Paths = &["M19 12H5", "M11 18l-6-6 6-6"];

/// Arrow down: jump to the latest thread items.
pub const ARROW_DOWN: Paths = &["M12 5v14", "M6 13l6 6 6-6"];

/// Next agent in the segment.
pub const ARROW_RIGHT: Paths = &["M5 12h14", "M13 6l6 6-6 6"];

/// Skip forward: next blocked agent.
pub const NEXT: Paths = &["M5 6l8 6-8 6z", "M17 6v12"];
/// Padlock: needs the desktop.
pub const LOCK: Paths = &["M6 11h12v9H6z", "M8.5 11V8a3.5 3.5 0 0 1 7 0v3"];
/// Warning triangle: stale.
pub const ALERT: Paths = &["M12 4l9 16H3z", "M12 10v4", "M12 17h.01"];
/// Plus: start something new.
pub const PLUS: Paths = &["M12 5v14", "M5 12h14"];
/// A prompt: runbooks.
pub const TERMINAL: Paths = &["M4 7l5 5-5 5", "M12 17h8"];
/// Close (×).
pub const CLOSE: Paths = &["M6 6l12 12", "M18 6L6 18"];

/// Renders an icon. Decorative: the surrounding control carries the label.
#[component]
pub fn Icon(
    /// The icon's path data, e.g. [`MIC`].
    paths: Paths,
) -> impl IntoView {
    view! {
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            {paths.iter().map(|d| view! { <path d=*d /> }).collect_view()}
        </svg>
    }
}
