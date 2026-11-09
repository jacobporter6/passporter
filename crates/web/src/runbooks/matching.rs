//! Which runbook, if any, a dictated utterance names.
//!
//! Only an **exact** match counts, after both sides are normalised the
//! same way: lower case, punctuation dropped, filler words ("um", "uh",
//! "please", …) removed, spaces collapsed. "Deploy the bridge." and "um,
//! deploy the bridge please" match an alias "deploy the bridge"; "deploy
//! the bridge now", "can you deploy the bridge" and "deploy bridge" do not.
//! Anything that does not match is dictated text as usual. A match only
//! ever *offers* the runbook: running it takes an explicit confirm. The
//! normalisation is the protocol's [`protocol::normalise_phrase`], the one
//! the bridge checks its config with, so no phrase can offer two runbooks.

use protocol::{Runbook, normalise_phrase as normalise};

/// The runbook whose name or an alias `said` is, exactly, once both are
/// normalised; `None` for anything else (including an utterance that
/// normalises to nothing).
#[must_use]
pub fn find<'a>(said: &str, runbooks: &'a [Runbook]) -> Option<&'a Runbook> {
    let said = normalise(said);
    if said.is_empty() {
        return None;
    }
    runbooks.iter().find(|runbook| {
        core::iter::once(runbook.name.as_str())
            .chain(runbook.aliases.iter().map(String::as_str))
            .any(|phrase| normalise(phrase) == said)
    })
}

#[cfg(test)]
mod tests {
    use protocol::RunbookName;

    use super::*;

    fn runbook(name: &str, aliases: &[&str]) -> Runbook {
        Runbook {
            name: RunbookName::new(name).unwrap(),
            aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
            argv: vec!["deploy/bridge/setup.sh".into()],
            cwd: "/repo".into(),
            confirm: true,
            run: None,
        }
    }

    fn book() -> Vec<Runbook> {
        vec![
            runbook("Deploy bridge", &["deploy the bridge", "ship it"]),
            runbook("Restart whisper", &[]),
        ]
    }

    #[test]
    fn matches_names_and_aliases_exactly_after_normalising() {
        let book = book();
        let named = |said: &str| find(said, &book).map(|r| r.name.as_str());
        assert_eq!(named("Deploy bridge"), Some("Deploy bridge"));
        assert_eq!(named("deploy bridge."), Some("Deploy bridge"));
        assert_eq!(named("Deploy the bridge."), Some("Deploy bridge"));
        assert_eq!(
            named("Um, deploy the bridge, please."),
            Some("Deploy bridge")
        );
        assert_eq!(named("Ship it!"), Some("Deploy bridge"));
        assert_eq!(named("restart Whisper"), Some("Restart whisper"));
    }

    #[test]
    fn anything_else_is_dictated_text() {
        let book = book();
        for said in [
            "deploy the bridge now",
            "can you deploy the bridge",
            "deploy the bridges",
            "deploy",
            "the bridge",
            "restart whisper and deploy the bridge",
            "please",
            "",
            "   ",
            "Deploy bridge; rm -rf /",
        ] {
            assert!(find(said, &book).is_none(), "{said:?}");
        }
        assert!(find("deploy bridge", &[]).is_none());
    }
}
