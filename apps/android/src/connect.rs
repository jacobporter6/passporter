//! Choosing the bridge: what the shell's `connect` command does, minus
//! Tauri, so it is tested on any host.

use std::sync::OnceLock;

use crate::origin::BridgeOrigin;
use crate::store::Store;

/// What [`connect`] did once the bridge was chosen.
#[derive(Debug)]
pub struct Connected<E> {
    /// Where the Connect page goes next: the bridge's start page.
    pub url: String,
    /// How granting the bridge native IPC went. A failure leaves the PWA
    /// working without native plugins, as in Chrome, and the next launch
    /// tries the grant again, so it is the caller's to log, not an error.
    pub grant: Result<(), E>,
}

/// Makes the bridge at `address` (what the person typed) this process's
/// bridge, unless one is chosen already. Call it with the shell's connect
/// lock held, so two never interleave.
///
/// The address is saved first, so a failure there changes nothing. `grant`
/// runs last, after the save: Tauri cannot take a capability back, so no
/// grant is made for an origin that did not become the bridge. Whatever
/// it answers, the origin is the bridge from then on, so a retry says so
/// and the Bridge page can change it.
pub fn connect<E>(
    store: &Store,
    bridge: &OnceLock<BridgeOrigin>,
    address: &str,
    grant: impl FnOnce(&BridgeOrigin) -> Result<(), E>,
) -> Result<Connected<E>, String> {
    let origin = BridgeOrigin::parse(address).map_err(|error| error.to_string())?;
    if let Some(current) = bridge.get() {
        return Err(format!(
            "already connected to {current}; change it from the Bridge page"
        ));
    }
    store
        .save(&origin)
        .map_err(|error| format!("could not save the address: {error}"))?;
    let url = origin.url().to_string();
    let grant = grant(&origin);
    bridge
        .set(origin)
        .map_err(|_| "already connected".to_owned())?;
    Ok(Connected { url, grant })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grant that succeeds.
    fn granted() -> impl FnOnce(&BridgeOrigin) -> Result<(), &'static str> {
        |_| Ok(())
    }

    #[test]
    fn connecting_saves_the_origin_and_chooses_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::in_dir(dir.path());
        let bridge = OnceLock::new();
        let connected =
            connect(&store, &bridge, "bridge.example.org/agents", granted()).expect("connects");
        assert_eq!(connected.url, "https://bridge.example.org/");
        assert_eq!(connected.grant, Ok(()));
        let origin = BridgeOrigin::parse("https://bridge.example.org").expect("origin");
        assert_eq!(bridge.get(), Some(&origin));
        assert_eq!(store.load().expect("load"), Some(origin));
    }

    #[test]
    fn a_failed_grant_still_connects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::in_dir(dir.path());
        let bridge = OnceLock::new();
        let connected = connect(&store, &bridge, "https://bridge.example.org", |origin| {
            let saved = store.load().expect("load");
            assert_eq!(saved.as_ref(), Some(origin), "saved before the grant");
            Err("Tauri refused the capability")
        })
        .expect("the Connect page still opens the bridge");
        assert_eq!(connected.url, "https://bridge.example.org/");
        assert_eq!(connected.grant, Err("Tauri refused the capability"));
        assert!(bridge.get().is_some(), "the Bridge page can change it");
    }

    #[test]
    fn the_bridge_is_chosen_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::in_dir(dir.path());
        let bridge = OnceLock::new();
        connect(&store, &bridge, "https://bridge.example.org", granted()).expect("connects");
        let again = connect(
            &store,
            &bridge,
            "https://other.example.org",
            |_| -> Result<(), &str> { panic!("no second grant") },
        );
        assert_eq!(
            again.expect_err("refused"),
            "already connected to https://bridge.example.org; change it from the Bridge page"
        );
        assert_eq!(
            store.load().expect("load"),
            BridgeOrigin::parse("https://bridge.example.org").ok(),
            "the saved address is unchanged"
        );
    }

    #[test]
    fn an_address_that_is_no_bridge_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::in_dir(dir.path());
        let bridge = OnceLock::new();
        let refused = connect(&store, &bridge, "http://bridge.example.org", granted());
        assert_eq!(
            refused.expect_err("refused"),
            "the bridge must be an https:// address"
        );
        assert!(bridge.get().is_none());
        assert!(store.load().expect("load").is_none());
    }
}
