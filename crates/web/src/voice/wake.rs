//! Keeping the screen on while a take is under way (Screen Wake Lock).
//!
//! A phone's screen timing out hides the page, and a hidden page stops
//! recording (see [`super::browser`]): a long dictation was cut short
//! mid-sentence. So from the tap that starts a take until its transcript
//! is sent, reviewed, cancelled or fails, the page holds a `screen` wake
//! lock. The browser drops the lock whenever the page is hidden (the
//! power button, another app) and may drop it for its own reasons
//! (battery saver); it is asked for again when the page is visible and a
//! take is still under way.
//!
//! [`Awake`] is the pure lifecycle, tested on the host; [`keep_on`]
//! drives it against `navigator.wakeLock`, through `js_sys::Reflect`:
//! `web-sys` only binds the API behind `--cfg=web_sys_unstable_apis`.
//! Where the API is missing (outside a secure context, older browsers)
//! nothing is asked for and dictation works as before.

use core::cell::RefCell;

use js_sys::{Function, Promise, Reflect};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::VisibilityState;

/// What to tell the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Ask for a screen wake lock.
    Request,
    /// Let go of the one held.
    Release,
}

/// Where the lock is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lock {
    /// None held or asked for.
    Free,
    /// Asked for; the browser hasn't answered.
    Asking,
    /// Held (as far as the page knows: the browser may have dropped it
    /// while the page was hidden, which [`Awake::lost`] reports).
    Held,
}

/// The wake lock's lifecycle: wanted while any take is under way, held
/// at most once for all of them.
#[derive(Debug, Clone)]
pub struct Awake {
    /// The browser has the API here.
    supported: bool,
    /// Takes under way (one per agent at most).
    takes: u32,
    lock: Lock,
}

impl Awake {
    /// Nothing under way; `supported` when the browser can keep the
    /// screen on.
    #[must_use]
    pub const fn new(supported: bool) -> Self {
        Self {
            supported,
            takes: 0,
            lock: Lock::Free,
        }
    }

    /// A take started.
    pub const fn begin(&mut self) -> Option<Order> {
        self.takes = self.takes.saturating_add(1);
        self.ask()
    }

    /// A take is over (sent, reviewed, cancelled or failed). The last one
    /// lets go; a lock still being asked for is let go once granted.
    pub const fn end(&mut self) -> Option<Order> {
        self.takes = self.takes.saturating_sub(1);
        if self.takes == 0 && matches!(self.lock, Lock::Held) {
            self.lock = Lock::Free;
            Some(Order::Release)
        } else {
            None
        }
    }

    /// The browser granted the lock asked for: kept if a take still
    /// wants it, else let go at once.
    pub const fn granted(&mut self) -> Option<Order> {
        if self.takes > 0 {
            self.lock = Lock::Held;
            None
        } else {
            self.lock = Lock::Free;
            Some(Order::Release)
        }
    }

    /// The browser refused (a hidden page, battery saver). Asked for again
    /// when the page is next visible.
    pub const fn refused(&mut self) {
        if matches!(self.lock, Lock::Asking) {
            self.lock = Lock::Free;
        }
    }

    /// The browser dropped the lock held (the page was hidden).
    pub const fn lost(&mut self) {
        if matches!(self.lock, Lock::Held) {
            self.lock = Lock::Free;
        }
    }

    /// The page is visible again: ask again if a take still wants it.
    pub const fn visible(&mut self) -> Option<Order> {
        self.ask()
    }

    const fn ask(&mut self) -> Option<Order> {
        if self.supported && self.takes > 0 && matches!(self.lock, Lock::Free) {
            self.lock = Lock::Asking;
            Some(Order::Request)
        } else {
            None
        }
    }
}

/// The document event that reports the page being shown or hidden.
const VISIBILITY_CHANGE: &str = "visibilitychange";

thread_local! {
    /// The lifecycle, made (and the visibility listener added) on first use.
    static AWAKE: RefCell<Option<Awake>> = const { RefCell::new(None) };
    /// The `WakeLockSentinel` held, if any.
    static SENTINEL: RefCell<Option<JsValue>> = const { RefCell::new(None) };
}

/// A take started (`true`) or is over (`false`): calls must pair up.
pub fn keep_on(on: bool) {
    let order = with(|awake| if on { awake.begin() } else { awake.end() });
    run(order);
}

/// Runs `f` on the lifecycle, making it on first use.
fn with<R>(f: impl FnOnce(&mut Awake) -> R) -> R {
    AWAKE.with_borrow_mut(|slot| {
        let awake = slot.get_or_insert_with(|| {
            let supported = wake_lock().is_some();
            if supported {
                listen();
            }
            Awake::new(supported)
        });
        f(awake)
    })
}

/// `navigator.wakeLock`, in a secure context that has it.
fn wake_lock() -> Option<JsValue> {
    let window = web_sys::window()?;
    if !window.is_secure_context() {
        return None;
    }
    Reflect::get(&window.navigator(), &JsValue::from_str("wakeLock"))
        .ok()
        .filter(JsValue::is_object)
}

/// Calls `target[name](...args)`.
fn call(target: &JsValue, name: &str, args: &js_sys::Array) -> Result<JsValue, JsValue> {
    Reflect::get(target, &JsValue::from_str(name))?
        .dyn_into::<Function>()?
        .apply(target, args)
}

/// Tells the browser `order`.
fn run(order: Option<Order>) {
    match order {
        Some(Order::Request) => {
            let asked = wake_lock()
                .ok_or(JsValue::UNDEFINED)
                .and_then(|lock| call(&lock, "request", &js_sys::Array::of1(&"screen".into())))
                .and_then(JsCast::dyn_into::<Promise>);
            let Ok(asked) = asked else {
                with(Awake::refused);
                return;
            };
            spawn_local(async move {
                // A refusal is expected (a hidden page) and harmless: the
                // take goes on, the screen may just sleep.
                match JsFuture::from(asked).await {
                    Ok(sentinel) => {
                        SENTINEL.set(Some(sentinel));
                        run(with(Awake::granted));
                    }
                    Err(_) => with(Awake::refused),
                }
            });
        }
        Some(Order::Release) => {
            let released = SENTINEL
                .take()
                .and_then(|sentinel| call(&sentinel, "release", &js_sys::Array::new()).ok())
                .and_then(|done| done.dyn_into::<Promise>().ok());
            if let Some(released) = released {
                spawn_local(async move {
                    let _ = JsFuture::from(released).await;
                });
            }
        }
        None => {}
    }
}

/// Listens for the page coming back into view, for the page's lifetime.
fn listen() {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let on_change = Closure::<dyn Fn()>::new(on_visibility_change).into_js_value();
    let _ = document.add_event_listener_with_callback(VISIBILITY_CHANGE, on_change.unchecked_ref());
}

/// The page was shown or hidden. Shown: a lock the browser dropped
/// meanwhile is asked for again, if a take still wants one.
fn on_visibility_change() {
    let visible = web_sys::window()
        .and_then(|w| w.document())
        .is_some_and(|d| d.visibility_state() == VisibilityState::Visible);
    if !visible {
        return;
    }
    let dropped = SENTINEL.with_borrow(|sentinel| {
        sentinel.as_ref().is_some_and(|sentinel| {
            Reflect::get(sentinel, &JsValue::from_str("released"))
                .ok()
                .and_then(|released| released.as_bool())
                .unwrap_or(true)
        })
    });
    if dropped {
        SENTINEL.set(None);
        with(Awake::lost);
    }
    run(with(Awake::visible));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_take_holds_the_lock_from_start_to_end() {
        let mut awake = Awake::new(true);
        assert_eq!(awake.begin(), Some(Order::Request), "the tap asks");
        assert_eq!(awake.granted(), None, "kept while the take runs");
        assert_eq!(awake.end(), Some(Order::Release), "sent: let go");
        assert_eq!(awake.end(), None, "nothing more to let go");
    }

    #[test]
    fn a_take_that_ends_before_the_lock_is_granted_lets_it_go() {
        // Cancelled (or failed) while the browser was still answering.
        let mut awake = Awake::new(true);
        assert_eq!(awake.begin(), Some(Order::Request));
        assert_eq!(awake.end(), None, "nothing held yet");
        assert_eq!(awake.granted(), Some(Order::Release));
        assert_eq!(
            awake.begin(),
            Some(Order::Request),
            "the next take asks anew"
        );
    }

    #[test]
    fn the_lock_is_asked_for_again_when_the_page_is_visible_mid_take() {
        let mut awake = Awake::new(true);
        awake.begin();
        awake.granted();
        // The screen went off: the browser dropped the lock.
        awake.lost();
        assert_eq!(
            awake.visible(),
            Some(Order::Request),
            "back on: asked again"
        );
        assert_eq!(awake.visible(), None, "asked once");
        awake.granted();
        assert_eq!(awake.end(), Some(Order::Release));
    }

    #[test]
    fn a_refusal_waits_for_the_page_to_be_visible() {
        let mut awake = Awake::new(true);
        awake.begin();
        awake.refused();
        assert_eq!(awake.end(), None, "nothing to let go");
        assert_eq!(awake.visible(), None, "no take, nothing asked");
        awake.begin();
        awake.refused();
        assert_eq!(awake.visible(), Some(Order::Request));
    }

    #[test]
    fn nothing_is_asked_while_no_take_is_under_way() {
        let mut awake = Awake::new(true);
        assert_eq!(awake.visible(), None);
        assert_eq!(awake.end(), None, "an unpaired end is harmless");
        assert_eq!(awake.begin(), Some(Order::Request));
    }

    #[test]
    fn takes_of_several_agents_share_one_lock() {
        let mut awake = Awake::new(true);
        assert_eq!(awake.begin(), Some(Order::Request));
        awake.granted();
        assert_eq!(awake.begin(), None, "already held");
        assert_eq!(awake.end(), None, "another take is under way");
        assert_eq!(awake.end(), Some(Order::Release));
    }

    #[test]
    fn nothing_is_asked_where_the_browser_cant_keep_the_screen_on() {
        let mut awake = Awake::new(false);
        assert_eq!(awake.begin(), None);
        assert_eq!(awake.visible(), None);
        assert_eq!(awake.end(), None);
    }
}
