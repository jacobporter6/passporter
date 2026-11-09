//! Small async helpers for the notice WASM: running work one at a time
//! (in this page, or across every tab), and awaiting two futures together.

use core::cell::RefCell;
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;
use std::thread::LocalKey;

use js_sys::{Function, Promise, Reflect};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    /// `navigator.locks` (the Web Locks API).
    type LockManager;

    /// `locks.request(name, callback)`: runs `callback` once the lock is
    /// granted, and holds it until the promise `callback` returns settles.
    #[wasm_bindgen(method, catch)]
    fn request(this: &LockManager, name: &str, callback: &JsValue) -> Result<Promise, JsValue>;
}

/// A queue that runs work one piece at a time, in call order (see
/// [`one_at_a_time`]). Holds the promise the last piece settles when done.
#[derive(Debug, Default)]
pub struct Serial(RefCell<Option<Promise>>);

impl Serial {
    /// An empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self(RefCell::new(None))
    }
}

/// Runs `work` once every piece queued on `queue` before it has finished.
/// Work that never finishes holds the queue, so keep it to steps that
/// can't hang for long (no unbounded network waits where avoidable).
pub async fn one_at_a_time<T>(
    queue: &'static LocalKey<Serial>,
    work: impl Future<Output = T>,
) -> T {
    let mut release: Option<Function> = None;
    let done = Promise::new(&mut |resolve: Function, _reject: Function| release = Some(resolve));
    let before = queue.with(|queue| queue.0.replace(Some(done)));
    if let Some(before) = before {
        // Settles only by resolving.
        let _ = JsFuture::from(before).await;
    }
    let output = work.await;
    if let Some(release) = release {
        let _ = release.call0(&JsValue::NULL);
    }
    output
}

/// Runs `work` holding the Web Lock `name`, which every tab (and worker)
/// of this origin shares, so two tabs can't interleave it. Where the Web
/// Locks API is missing or refuses (an old browser, a sandboxed page), it
/// holds `fallback` instead, which orders this page only. Like
/// [`one_at_a_time`], work that never finishes holds the lock.
pub async fn across_tabs<T>(
    name: &str,
    fallback: &'static LocalKey<Serial>,
    work: impl Future<Output = T>,
) -> T {
    let Some((granted, release)) = request_lock(name) else {
        return one_at_a_time(fallback, work).await;
    };
    // Settles only by resolving.
    let _ = JsFuture::from(granted).await;
    let output = work.await;
    let _ = release.call0(&JsValue::NULL);
    output
}

/// Asks for the Web Lock `name`: a promise that resolves once it is held
/// (or the request failed, so the caller goes ahead unlocked rather than
/// never), and the function that lets it go. `None` without Web Locks.
fn request_lock(name: &str) -> Option<(Promise, Function)> {
    let navigator = web_sys::window()?.navigator();
    let locks = Reflect::get(&navigator, &JsValue::from_str("locks")).ok()?;
    if locks.is_undefined() {
        return None;
    }
    let locks: LockManager = locks.unchecked_into();
    let mut release: Option<Function> = None;
    let held = Promise::new(&mut |resolve: Function, _reject: Function| release = Some(resolve));
    let mut grant: Option<Function> = None;
    let granted = Promise::new(&mut |resolve: Function, _reject: Function| grant = Some(resolve));
    let (release, grant) = (release?, grant?);
    let on_grant = grant.clone();
    let callback = Closure::once_into_js(move |_lock: JsValue| -> Promise {
        let _ = on_grant.call0(&JsValue::NULL);
        held
    });
    let request = locks.request(name, &callback).ok()?;
    // The request promise rejects only if the lock was never granted.
    let on_refused = Closure::once_into_js(move |_error: JsValue| {
        let _ = grant.call0(&JsValue::NULL);
    });
    if let Some(catch) = Reflect::get(&request, &JsValue::from_str("catch"))
        .ok()
        .and_then(|f| f.dyn_into::<Function>().ok())
    {
        let _ = catch.call1(&request, &on_refused);
    }
    Some((granted, release))
}

/// Awaits `a` and `b` together and returns both outputs.
pub async fn both<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut out_a, mut out_b) = (None, None);
    poll_fn(|cx| {
        if out_a.is_none()
            && let Poll::Ready(output) = a.as_mut().poll(cx)
        {
            out_a = Some(output);
        }
        if out_b.is_none()
            && let Poll::Ready(output) = b.as_mut().poll(cx)
        {
            out_b = Some(output);
        }
        match (out_a.take(), out_b.take()) {
            (Some(x), Some(y)) => Poll::Ready((x, y)),
            (x, y) => {
                (out_a, out_b) = (x, y);
                Poll::Pending
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use core::task::{Context, Waker};

    use super::*;

    /// Pending on the first poll, ready on the second.
    fn later<T>(value: T) -> impl Future<Output = T> {
        let mut value = Some(value);
        let mut polled = false;
        poll_fn(move |cx| {
            if polled {
                Poll::Ready(value.take().expect("polled after ready"))
            } else {
                polled = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    #[test]
    fn both_waits_for_the_slower_side() {
        let mut joined = pin!(both(later(1), core::future::ready("now")));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(joined.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(joined.as_mut().poll(&mut cx), Poll::Ready((1, "now")));

        let mut joined = pin!(both(core::future::ready('a'), later(2)));
        assert_eq!(joined.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(joined.as_mut().poll(&mut cx), Poll::Ready(('a', 2)));
    }
}
