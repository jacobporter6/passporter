//! Stamps the build identity (`PASSPORTER_GIT_SHA`, `PASSPORTER_BUILT`) into
//! the bridge and the CLI; see `build/version.rs`.

include!("../../build/version.rs");

fn main() {
    stamp_build(&["src", "hooks"]);
}
