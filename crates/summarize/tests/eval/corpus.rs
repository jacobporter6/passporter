//! Sampling finished turns from the scrubbed corpus (docs/corpus.md).
//!
//! Only main session transcripts are sampled: `projects/<slug>/<session>.jsonl`
//! below the corpus root (the product summarizes the turns a user starts;
//! a subagent's "user" prompt is its parent's task text). This is narrower
//! than the transcripts crate's corpus test, which parses every transcript.
//! Each raw dump is a full snapshot of `~/.claude/projects`, so the same
//! session can appear in several dumps: only the largest copy is used (a
//! transcript only grows, so it is the most complete).
//!
//! A turn is *finished* when a later turn exists in the same transcript;
//! turn 0 (records before the first typed message) and a turn cut by the
//! thread's memory window are never sampled.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use protocol::{ThreadContent, ThreadItem, TurnIndex};
use summarize::TurnStats;
use transcripts::{Scope, TranscriptReader};

/// How to choose turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sampling {
    /// Turns to sample (fewer if the corpus has fewer candidates).
    pub(crate) count: usize,
    /// Seed for the deterministic shuffle: the same corpus and seed give the
    /// same sample.
    pub(crate) seed: u64,
    /// Only turns long enough to get a TL;DR in the product
    /// ([`TurnStats::is_long`]).
    pub(crate) long_only: bool,
}

/// One sampled turn.
#[derive(Debug, Clone)]
pub(crate) struct Sample {
    /// `<transcript file stem>#<turn>`, for the report.
    pub(crate) id: String,
    /// The transcript, relative to the corpus root.
    pub(crate) source: PathBuf,
    /// The turn's items.
    pub(crate) items: Vec<ThreadItem>,
}

/// The sample, plus how many transcripts or directories could not be
/// read (skipped, like the transcripts crate's corpus test does).
#[derive(Debug, Clone)]
pub(crate) struct Drawn {
    pub(crate) samples: Vec<Sample>,
    pub(crate) unreadable: usize,
}

/// Why the corpus could not be sampled.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CorpusError {
    #[error("refusing to read {0}: it is (or is inside) a raw corpus directory")]
    Raw(PathBuf),
    #[error("listing {path}: {source}")]
    List {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("reading transcript {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: transcripts::Error,
    },
}

/// Samples up to `sampling.count` finished turns from the transcripts under
/// `root`. Fails on a path that looks like the raw corpus or a root that
/// cannot be listed; skips (and counts) transcripts and subdirectories
/// that cannot be read.
pub(crate) fn sample(root: &Path, sampling: Sampling) -> Result<Drawn, CorpusError> {
    reject_raw(root)?;
    fs::read_dir(root).map_err(|source| CorpusError::List {
        path: root.to_path_buf(),
        source,
    })?;
    // Paths relative to `root`: layout rules apply below the root only.
    let mut found = Vec::new();
    let mut unreadable = 0;
    collect_transcripts(root, Path::new(""), &mut found, &mut unreadable);
    let files = latest_copies(found);

    // One pass, seeded reservoir sampling (Algorithm R): every candidate
    // turn has the same chance of being kept, each transcript is parsed
    // once, and only kept turns are copied.
    let mut rng = SplitMix64(sampling.seed);
    let mut kept: Vec<Sample> = Vec::with_capacity(sampling.count);
    let mut seen: u64 = 0;
    for relative in &files {
        let Ok(reader) = read(root, relative) else {
            unreadable += 1;
            continue;
        };
        let thread = reader.thread();
        let stem = relative.file_stem().unwrap_or_default().to_string_lossy();
        for turn in finished_turns(&reader) {
            let items = thread.turn(turn);
            if sampling.long_only && !TurnStats::measure(items, None).is_long() {
                continue;
            }
            seen += 1;
            let slot = if kept.len() < sampling.count {
                Some(kept.len())
            } else {
                usize::try_from(rng.below(seen))
                    .ok()
                    .filter(|&j| j < sampling.count)
            };
            let Some(slot) = slot else { continue };
            let sample = Sample {
                id: format!("{stem}#{}", turn.0),
                source: relative.clone(),
                items: items.to_vec(),
            };
            if slot == kept.len() {
                kept.push(sample);
            } else {
                kept[slot] = sample;
            }
        }
    }
    Ok(Drawn {
        samples: kept,
        unreadable,
    })
}

/// The finished, fully held turns of one transcript that have assistant
/// output (nothing to summarize otherwise).
fn finished_turns(reader: &TranscriptReader) -> Vec<TurnIndex> {
    let thread = reader.thread();
    let Some(current) = thread.current_turn() else {
        return Vec::new();
    };
    let items = thread.items();
    // History before the window is gone: the first held turn may be partial.
    let partial = thread
        .truncated_before()
        .and_then(|_| items.first())
        .map(|item| item.turn);
    let mut turns: Vec<TurnIndex> = items.iter().map(|item| item.turn).collect();
    turns.dedup();
    turns
        .into_iter()
        .filter(|&turn| turn.0 > 0 && turn < current && Some(turn) != partial)
        .filter(|&turn| {
            thread.turn(turn).iter().any(|item| {
                matches!(&item.content, ThreadContent::Assistant { text } if !text.trim().is_empty())
            })
        })
        .collect()
}

/// Reads the transcript at `root/relative`.
fn read(root: &Path, relative: &Path) -> Result<TranscriptReader, CorpusError> {
    let path = root.join(relative);
    let mut reader = TranscriptReader::new(&path, Scope::Main);
    reader
        .refresh()
        .map_err(|source| CorpusError::Read { path, source })?;
    reader.settle_pending();
    Ok(reader)
}

/// Swarm agents and this harness never read `corpus/raw` (AGENTS.md).
fn reject_raw(root: &Path) -> Result<(), CorpusError> {
    let resolved = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let is_raw = |path: &Path| {
        path.components()
            .any(|c| matches!(c, Component::Normal(name) if name == "raw"))
    };
    if is_raw(root) || is_raw(&resolved) {
        return Err(CorpusError::Raw(root.to_path_buf()));
    }
    Ok(())
}

/// Collects main session transcripts under `root/relative`, as paths
/// relative to `root` with their sizes; counts directories it cannot list.
fn collect_transcripts(
    root: &Path,
    relative: &Path,
    out: &mut Vec<(PathBuf, u64)>,
    unreadable: &mut usize,
) {
    let Ok(listing) = fs::read_dir(root.join(relative)) else {
        *unreadable += 1;
        return;
    };
    for entry in listing {
        let Ok(entry) = entry else {
            *unreadable += 1;
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            *unreadable += 1;
            continue;
        };
        let child = relative.join(entry.file_name());
        if kind.is_dir() && entry.file_name() != "raw" {
            collect_transcripts(root, &child, out, unreadable);
        } else if kind.is_file() && session_key(&child).is_some() {
            let size = entry.metadata().map_or(0, |m| m.len());
            out.push((child, size));
        }
    }
}

/// For a main session transcript (`.../projects/<slug>/<session>.jsonl`),
/// the part from `projects` on: the same session in every dump.
fn session_key(relative: &Path) -> Option<PathBuf> {
    let parts: Vec<Component<'_>> = relative.components().collect();
    let [.., projects, slug, file] = parts[..] else {
        return None;
    };
    let name = file.as_os_str().to_string_lossy();
    (projects.as_os_str() == "projects" && name.ends_with(".jsonl"))
        .then(|| [projects, slug, file].iter().collect())
}

/// One copy per session: the largest, then the last by path (a later
/// dump) on a tie. Sorted by path, so sampling is deterministic.
fn latest_copies(found: Vec<(PathBuf, u64)>) -> Vec<PathBuf> {
    let mut best: BTreeMap<PathBuf, (u64, PathBuf)> = BTreeMap::new();
    for (relative, size) in found {
        let Some(key) = session_key(&relative) else {
            continue;
        };
        let candidate = (size, relative);
        match best.get(&key) {
            Some(current) if *current >= candidate => {}
            _ => {
                best.insert(key, candidate);
            }
        }
    }
    let mut files: Vec<PathBuf> = best.into_values().map(|(_, path)| path).collect();
    files.sort();
    files
}

/// A tiny seeded PRNG (`SplitMix64`): deterministic per seed, no dependency.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (`bound > 0`; the modulo bias is negligible
    /// for corpus-sized bounds).
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_seeded_and_bounded() {
        let draw = |seed| {
            let mut rng = SplitMix64(seed);
            (0..20).map(|_| rng.below(10)).collect::<Vec<_>>()
        };
        assert_eq!(draw(7), draw(7));
        assert_ne!(draw(7), draw(8));
        assert!(draw(7).iter().all(|&v| v < 10));
    }

    #[test]
    fn only_main_transcripts_have_a_session_key() {
        let key = |p: &str| session_key(Path::new(p));
        assert_eq!(
            key("d1/projects/-src-x/abc.jsonl"),
            Some(PathBuf::from("projects/-src-x/abc.jsonl"))
        );
        assert_eq!(key("d1/projects/-src-x/abc/subagents/agent-1.jsonl"), None);
        assert_eq!(key("d1/history.jsonl"), None);
        assert_eq!(key("d1/projects/-src-x/notes.txt"), None);
    }

    #[test]
    fn the_largest_copy_of_a_session_wins() {
        let files = latest_copies(vec![
            (PathBuf::from("d1/projects/p/s.jsonl"), 10),
            (PathBuf::from("d2/projects/p/s.jsonl"), 30),
            (PathBuf::from("d3/projects/p/s.jsonl"), 20),
            (PathBuf::from("d1/projects/p/t.jsonl"), 5),
            (PathBuf::from("d2/projects/p/t.jsonl"), 5),
        ]);
        assert_eq!(
            files,
            [
                PathBuf::from("d2/projects/p/s.jsonl"),
                PathBuf::from("d2/projects/p/t.jsonl")
            ]
        );
    }

    #[test]
    fn raw_paths_are_refused() {
        let err = sample(
            Path::new("/home/x/.local/share/passporter/corpus/raw"),
            Sampling {
                count: 1,
                seed: 1,
                long_only: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CorpusError::Raw(_)), "{err}");
    }
}
