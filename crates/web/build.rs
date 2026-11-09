//! Stamps the build identity (`PASSPORTER_GIT_SHA`, `PASSPORTER_BUILT_UNIX`)
//! into the PWA, so it can tell when the bridge runs a newer build; see
//! `build/version.rs`.

include!("../../build/version.rs");

fn main() {
    stamp_build(&["src"]);
}
