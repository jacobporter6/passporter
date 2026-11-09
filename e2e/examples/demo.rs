//! Records a short demo video of Passporter (`just demo`).
//!
//! The same rig as the e2e scenarios ([`e2e::app::Rig`]): the real bridge
//! and PWA, the fake herdr, a fake `claude` and a fake `whisper-server`,
//! system Chromium at phone size. Its world is the demo's own: a handful
//! of agents across three repos ([`world`]). A scripted walk through the
//! app is captured frame by frame over CDP, then `ffmpeg` sets the phone
//! on a 1080p stage with captions and, if recorded, narration.
//!
//! ```sh
//! just demo                          # target/demo/passporter-demo.mp4
//! uv run e2e/examples/narrate.py     # speaks target/demo/narration/lines.tsv
//! just demo                          # again, now narrated
//! ```
//!
//! Each run writes the narrator's lines to `target/demo/narration/lines.tsv`;
//! `NN.wav` beside it is scene `NN` spoken, and each scene lasts at least
//! as long as its clip. Without clips the video has captions only.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use e2e::World;
use e2e::app::{
    RECORDING, Rig, RigSpec, connect_secure, expect, js, prompt_reaches_bridge, tap_mic, until,
};
use e2e::cdp::Page;
use e2e::herdr::{FakeAgent, Status};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T, E = Error> = std::result::Result<T, E>;

/// The phone, in CSS pixels, and its pixel ratio in the capture.
const PHONE: (u32, u32) = e2e::app::PHONE;
const SCALE: u32 = 2;
/// Output frame rate.
const FPS: u32 = 30;

/// What the fake `whisper-server` hears, as the dictated message reads.
const DICTATED: &str = "Rerun it fifty times with the seed pinned, then open a PR with the fix.";
/// The new agent's first prompt.
const NEW_PROMPT: &str = "Add rate limiting to the pairing endpoint, with tests.";

/// Folder name of each repo the agents work in.
const PASSPORTER: &str = "passporter";
const WIDGET: &str = "widget";
const ACME: &str = "acme";
/// Where the agents' repos are on the dev box, as their screens and
/// transcripts show them.
const DEV_REPOS: &str = "/home/dev/git";

/// The agents.
const AUTH: &str = "bridge-auth";
const MIGRATION: &str = "orders-migration";
const DOCS: &str = "docs-pass";
const DEPS: &str = "dep-bump";
const RELEASE: &str = "release-notes";
const FLAKY: &str = "flaky-ws-test";
/// The suggested reply the demo taps.
const SUGGESTED: &str = "Yes, open the PR.";

#[expect(clippy::print_stdout, reason = "the video's path is the output")]
fn main() -> Result<()> {
    let mp4 = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(run())?;
    println!("{}", mp4.display());
    Ok(())
}

async fn run() -> Result<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("the e2e crate sits in the workspace")?;
    let out_dir = root.join("target/demo");
    let frames_dir = out_dir.join("frames");
    if frames_dir.exists() {
        std::fs::remove_dir_all(&frames_dir)?;
    }
    std::fs::create_dir_all(&frames_dir)?;
    let clips = narration(&out_dir.join("narration"))?;

    let rig = Rig::start_with(RigSpec::default(), world)
        .await
        .ok_or("no Chromium found")?;
    let page = rig.page().await;
    page.call(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": TAP_SCRIPT}),
    )
    .await?;
    page.call(
        "Emulation.setDeviceMetricsOverride",
        json!({"width": PHONE.0, "height": PHONE.1, "deviceScaleFactor": SCALE, "mobile": true}),
    )
    .await?;
    page.call(
        "Emulation.setEmulatedMedia",
        json!({"features": [{"name": "prefers-color-scheme", "value": "dark"}]}),
    )
    .await?;

    connect_secure(&page, &rig).await;
    warm_up(&rig).await;

    let stop = AtomicBool::new(false);
    let mut demo = Demo {
        page: &page,
        rig: &rig,
        start: Instant::now(),
        captions: Vec::new(),
        clips,
    };
    let (frames, script) = tokio::join!(record(&page, &frames_dir, &stop), async {
        let result = demo.script().await;
        stop.store(true, Ordering::Relaxed);
        result
    });
    let frames = frames?;
    script?;
    let end = demo.start.elapsed();
    let mp4 = out_dir.join("passporter-demo.mp4");
    compose(&out_dir, &frames, &demo.captions, end, &mp4)?;
    Ok(mp4)
}

/// Gets the cards' lines made before the camera rolls, as a bridge that
/// has been up a while has them. Every agent starts mid-turn ([`world`]);
/// each now reaches its state, so the bridge sees its turn finish and
/// headlines it, then settles.
async fn warm_up(rig: &Rig) {
    tokio::time::sleep(Duration::from_secs(2)).await;
    for agent in cast() {
        rig.herdr.set_screen(&agent.pane, &agent.screen);
        rig.herdr.set_status(&agent.pane, agent.status);
    }
    tokio::time::sleep(Duration::from_secs(12)).await;
}

/// One captured frame: its file and when it was taken.
struct Frame {
    path: PathBuf,
    at: Duration,
}

/// Screenshots the page as fast as Chromium allows until `stop`.
async fn record(page: &Page<'_>, dir: &Path, stop: &AtomicBool) -> Result<Vec<Frame>> {
    let start = Instant::now();
    let mut frames = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let at = start.elapsed();
        let shot = page
            .call(
                "Page.captureScreenshot",
                json!({"format": "jpeg", "quality": 90, "optimizeForSpeed": true}),
            )
            .await?;
        let data = shot["data"].as_str().ok_or("no screenshot data")?;
        let path = dir.join(format!("{:05}.jpg", frames.len()));
        std::fs::write(&path, base64::decode(data)?)?;
        frames.push(Frame { path, at });
    }
    Ok(frames)
}

/// One scene: the caption on the stage and the narrator's line.
struct Scene {
    title: &'static str,
    body: &'static str,
    say: &'static str,
}

/// The scenes, in order.
const SCENES: &[Scene] = &[
    Scene {
        title: "Passporter",
        body: "A phone inbox for the coding agents\nrunning on your dev box.",
        say: "This is Passporter: a phone inbox for all the coding agents running on your dev box.",
    },
    Scene {
        title: "Triage at a glance",
        body: "Every agent herdr runs, sorted into\nBlocked, Working and Done.\nBlocked comes first.",
        say: "Every agent herder runs lands in one of three lists: blocked, working, or done. Blocked always comes first.",
    },
    Scene {
        title: "Approve in one tap",
        body: "Permission prompts become buttons.\nRisky options are marked as such.",
        say: "When an agent needs permission, the prompt becomes buttons. One tap, and it's back to work.",
    },
    Scene {
        title: "Long turns, summarised",
        body: "Each turn leads with a one-line headline.\nThe full reply and tool calls\nare one tap away.",
        say: "Long turns lead with a one-line headline. The full reply, and every tool call, is a tap away.",
    },
    Scene {
        title: "Suggested replies",
        body: "When an agent asks a question,\nlikely answers are one tap away.",
        say: "If the agent asks a question, the likely answers are ready to send.",
    },
    Scene {
        title: "Or just say it",
        body: "Dictation runs on whisper.cpp, on your own machine.\nQueued replies survive dead zones,\nand never send stale.",
        say: "Or just say it. Dictation runs on whisper, on your own machine. Replies wait out dead zones, and never send stale.",
    },
    Scene {
        title: "Start new agents",
        body: "Pick a repo, describe the task.\nherdr opens a fresh tab on the dev box.",
        say: "You can start new agents from here too. Pick a repo, describe the task, and herder opens a fresh tab.",
    },
    Scene {
        title: "Built in Rust, end to end",
        body: "axum bridge · Leptos/WASM PWA\nSelf-hosted, on your own dev box\nWorks with Claude Code and Codex",
        say: "It's Rust, end to end. Self-hosted on your own dev box, and it works with Claude Code and Codex.",
    },
];

/// A recorded narration clip and its length.
type Clip = (PathBuf, Duration);

/// Writes the narrator's lines to `dir/lines.tsv`, and returns each
/// scene's clip (`dir/NN.wav`), if recorded.
fn narration(dir: &Path) -> Result<Vec<Option<Clip>>> {
    std::fs::create_dir_all(dir)?;
    let mut lines = String::new();
    for (i, scene) in SCENES.iter().enumerate() {
        let _ = writeln!(lines, "{i:02}\t{}", scene.say);
    }
    std::fs::write(dir.join("lines.tsv"), lines)?;
    (0..SCENES.len())
        .map(|i| {
            let clip = dir.join(format!("{i:02}.wav"));
            if !clip.is_file() {
                return Ok(None);
            }
            let out = std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "format=duration",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&clip)
                .output()?;
            let secs: f64 = String::from_utf8_lossy(&out.stdout).trim().parse()?;
            Ok(Some((clip, Duration::from_secs_f64(secs))))
        })
        .collect()
}

/// A caption on the stage, from `at` until the next one.
struct Caption {
    at: Duration,
    scene: &'static Scene,
    /// The spoken line, if recorded.
    clip: Option<Clip>,
}

/// The walk through the app, and the captions it leaves behind.
struct Demo<'a> {
    page: &'a Page<'a>,
    rig: &'a Rig,
    start: Instant,
    captions: Vec<Caption>,
    clips: Vec<Option<Clip>>,
}

impl Demo<'_> {
    /// Waits for the narrator to finish the current scene's line.
    async fn let_narration_finish(&self) {
        if let Some(Caption {
            at,
            clip: Some((_, len)),
            ..
        }) = self.captions.last()
        {
            let done = *at + *len + Duration::from_millis(450);
            if let Some(left) = done.checked_sub(self.start.elapsed()) {
                tokio::time::sleep(left).await;
            }
        }
    }

    /// Starts the next scene: its caption, and its line if recorded.
    async fn scene(&mut self) {
        self.let_narration_finish().await;
        let i = self.captions.len();
        self.captions.push(Caption {
            at: self.start.elapsed(),
            scene: &SCENES[i],
            clip: self.clips.get(i).cloned().flatten(),
        });
    }

    async fn pause(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    /// Shows a touch ripple on the first element matching `selector` (and
    /// containing `text`, if given), then taps it.
    async fn tap(&self, selector: &str, text: Option<&str>) -> Result<()> {
        let found = self
            .page
            .eval(&format!(
                "window.demoTap({}, {})",
                js(selector),
                text.map_or_else(|| "null".to_owned(), js)
            ))
            .await?;
        if found != Value::Bool(true) {
            return Err(format!("nothing to tap: {selector} {text:?}").into());
        }
        Ok(())
    }

    /// Taps the dock's mic, with a ripple.
    async fn tap_mic(&self) -> Result<()> {
        self.page.eval("window.demoRipple('.mic-fab')").await?;
        self.pause(260).await;
        tap_mic(self.page).await;
        Ok(())
    }

    /// Types into `selector` a character at a time.
    async fn type_text(&self, selector: &str, text: &str) -> Result<()> {
        self.page
            .eval(&format!("document.querySelector({}).focus()", js(selector)))
            .await?;
        for ch in text.chars() {
            self.page.insert_text(&ch.to_string()).await?;
            self.pause(28).await;
        }
        Ok(())
    }

    /// Scrolls the agent's screen down by `px`, smoothly.
    async fn scroll_by(&self, px: i32) -> Result<()> {
        self.page
            .eval(&format!(
                "document.scrollingElement.scrollBy({{ top: {px}, behavior: 'smooth' }})"
            ))
            .await?;
        Ok(())
    }

    async fn until(&self, expression: &str, what: &str) {
        expect(self.page, &self.rig.bridge, expression, what).await;
    }

    async fn segment(&self, name: &str) -> Result<()> {
        self.tap(&format!("#tab-{name}"), None).await?;
        self.until(
            &format!(
                "document.querySelector('#tab-{name}')?.getAttribute('aria-selected') === 'true'"
            ),
            name,
        )
        .await;
        Ok(())
    }

    async fn open(&self, title: &str) -> Result<()> {
        self.tap(".agent-card", Some(title)).await?;
        self.until("location.pathname.startsWith('/agent/')", title)
            .await;
        Ok(())
    }

    async fn back_to_triage(&self) -> Result<()> {
        self.tap(".dock-link[href='/']", None).await?;
        self.until("location.pathname === '/'", "triage").await;
        Ok(())
    }

    /// The walk through the app, a scene at a time.
    async fn script(&mut self) -> Result<()> {
        self.scene().await;
        self.pause(3_500).await;
        self.triage().await?;
        self.approve().await?;
        self.summary().await?;
        self.suggested_reply().await?;
        self.dictate().await?;
        self.new_agent().await?;
        self.scene().await;
        self.back_to_triage().await?;
        self.pause(4_000).await;
        self.let_narration_finish().await;
        self.pause(800).await;
        Ok(())
    }

    /// Each of triage's segments, ending on Blocked.
    async fn triage(&mut self) -> Result<()> {
        self.scene().await;
        self.pause(1_800).await;
        self.segment("working").await?;
        self.pause(2_200).await;
        self.segment("done").await?;
        self.pause(2_200).await;
        self.segment("blocked").await?;
        self.pause(900).await;
        Ok(())
    }

    /// Yes on the auth agent's permission dialog; it runs the tests and
    /// finishes.
    async fn approve(&mut self) -> Result<()> {
        let (herdr, bridge) = (&self.rig.herdr, &self.rig.bridge);
        self.scene().await;
        self.open(AUTH).await?;
        self.until(
            "document.querySelectorAll('.chips .chip').length >= 2",
            "permission chips",
        )
        .await;
        self.pause(2_800).await;
        self.tap(".chips .chip", Some("Yes")).await?;
        until(bridge, "the Yes key reaches herdr", || {
            !herdr.keyed().is_empty()
        })
        .await;
        // The dialog is answered: the agent runs the tests it asked to.
        let auth = pane(AUTH);
        herdr.set_screen(&auth, "⏺ Running the bridge tests…\n");
        herdr.set_status(&auth, Status::Working);
        self.until(
            "!document.querySelector('.chips .chip')",
            "the dialog answered",
        )
        .await;
        self.pause(1_800).await;
        // They pass, and it finishes.
        finish_auth(&self.rig.world)?;
        herdr.set_screen(&auth, "⏺ Done.\n");
        herdr.set_status(&auth, Status::Done);
        self.pause(2_200).await;
        Ok(())
    }

    /// The release agent's long turn: its headline, then the full turn.
    async fn summary(&mut self) -> Result<()> {
        self.scene().await;
        self.back_to_triage().await?;
        self.segment("done").await?;
        self.pause(700).await;
        self.open(RELEASE).await?;
        self.until(
            "!!document.querySelector('.thread .turn-headline')",
            "headline",
        )
        .await;
        self.pause(2_600).await;
        self.tap(".thread .turn-toggle", None).await?;
        self.pause(1_400).await;
        self.scroll_by(320).await?;
        self.pause(2_000).await;
        Ok(())
    }

    /// The release agent asked a question: one tap sends a likely answer.
    async fn suggested_reply(&mut self) -> Result<()> {
        self.scene().await;
        self.until(
            &format!(
                "[...document.querySelectorAll('.suggestions .chip-suggestion')].some(c => c.textContent.includes({}))",
                js(SUGGESTED)
            ),
            "a suggested reply",
        )
        .await;
        self.pause(1_600).await;
        self.tap(".suggestions .chip-suggestion", Some(SUGGESTED))
            .await?;
        prompt_reaches_bridge(&self.rig.bridge, SUGGESTED, "the suggested reply").await;
        prompt(&self.rig.world, RELEASE, SUGGESTED)?;
        self.pause(2_200).await;
        Ok(())
    }

    /// A reply to the flaky-test agent, dictated through the dock mic.
    async fn dictate(&mut self) -> Result<()> {
        self.scene().await;
        self.back_to_triage().await?;
        self.segment("done").await?;
        self.pause(600).await;
        self.open(FLAKY).await?;
        self.until(
            "!!document.querySelector('.mic-fab:not([disabled])')",
            "the mic",
        )
        .await;
        self.pause(1_600).await;
        self.tap_mic().await?;
        self.until(RECORDING, "recording").await;
        self.pause(3_000).await;
        self.tap_mic().await?;
        prompt_reaches_bridge(&self.rig.bridge, DICTATED, "the dictated prompt").await;
        prompt(&self.rig.world, FLAKY, DICTATED)?;
        self.pause(2_600).await;
        Ok(())
    }

    /// A new agent in a picked repo, with its first prompt.
    async fn new_agent(&mut self) -> Result<()> {
        self.scene().await;
        self.back_to_triage().await?;
        self.pause(600).await;
        self.tap(".new-agent-button", None).await?;
        self.until("!!document.querySelector('.new-agent-sheet')", "the sheet")
            .await;
        self.pause(900).await;
        self.tap(".new-agent-sheet .option-chip", Some(PASSPORTER))
            .await?;
        self.pause(500).await;
        self.type_text("#new-agent-prompt", NEW_PROMPT).await?;
        self.pause(700).await;
        self.tap(".new-agent-start", None).await?;
        self.until(
            "location.pathname.startsWith('/agent/w') && !document.querySelector('.new-agent-sheet')",
            "the new agent",
        )
        .await;
        self.pause(2_400).await;
        Ok(())
    }
}

/// Touch ripples, drawn over the app (and ignoring pointer events), so a
/// viewer can see where each tap lands. `demoTap` ripples and clicks the
/// first match; `demoRipple` only ripples.
const TAP_SCRIPT: &str = r"
(() => {
  const css = `
    .demo-tap { position: fixed; z-index: 2147483647; pointer-events: none;
      width: 44px; height: 44px; margin: -22px 0 0 -22px; border-radius: 50%;
      background: rgba(255,255,255,.45); box-shadow: 0 0 0 2px rgba(255,255,255,.7);
      animation: demo-tap .55s ease-out forwards; }
    @keyframes demo-tap { 0% { transform: scale(.4); opacity: 1 }
      100% { transform: scale(1.5); opacity: 0 } }`;
  const find = (sel, text) => [...document.querySelectorAll(sel)]
    .find(e => text === null || e.textContent.includes(text));
  const ripple = (el) => {
    if (!document.querySelector('#demo-tap-style')) {
      const s = document.createElement('style'); s.id = 'demo-tap-style';
      s.textContent = css; document.head.append(s);
    }
    el.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    const r = el.getBoundingClientRect();
    const d = document.createElement('div'); d.className = 'demo-tap';
    d.style.left = (r.left + r.width / 2) + 'px'; d.style.top = (r.top + r.height / 2) + 'px';
    document.body.append(d); setTimeout(() => d.remove(), 700);
  };
  window.demoRipple = (sel) => { const el = find(sel, null); if (el) ripple(el); return !!el; };
  window.demoTap = (sel, text) => new Promise(ok => {
    const el = find(sel, text); if (!el) return ok(false);
    ripple(el); setTimeout(() => { el.click(); ok(true); }, 260);
  });
})();
";

// ---------------------------------------------------------------------------
// The demo world: agents, transcripts and a fake `claude`.

/// The pane of the agent called `name`.
fn pane(name: &str) -> String {
    cast()
        .into_iter()
        .find(|a| a.name.as_deref() == Some(name))
        .map(|a| a.pane)
        .unwrap_or_default()
}

/// A repo's folder on the dev box.
fn repo(name: &str) -> PathBuf {
    Path::new(DEV_REPOS).join(name)
}

/// Claude Code's Bash permission dialog, as herdr's `agent.read` shows it.
fn permission_screen(intro: &str, command: &str, why: &str, rule: &str, repo_name: &str) -> String {
    format!(
        "⏺ {intro}\n\n⏺ Bash({command})\n  ⎿  Running… ({{tick}}s)\n\n\
         ────────────────────────────────────────────────────────────────────────────────\n \
         Bash command\n\n   {command}\n   {why}\n\n Do you want to proceed?\n \
         ❯ 1. Yes\n   2. Yes, and don't ask again for {rule} commands in\n      {}\n   \
         3. No, and tell Claude what to do differently (esc)\n",
        repo(repo_name).display()
    )
}

/// The agents as the camera finds them, in herdr's order: two blocked,
/// two working, two done.
fn cast() -> Vec<FakeAgent> {
    let agent = |n: u32, name: &str, status: Status, repo_name: &str, screen: String| FakeAgent {
        screen,
        ..FakeAgent::claude(
            &format!("w{}:p{n}", workspace(repo_name).0),
            name,
            status,
            &format!("de30de30-0000-4000-8000-{n:012}"),
            repo(repo_name),
        )
    };
    vec![
        agent(
            1,
            AUTH,
            Status::Blocked,
            PASSPORTER,
            permission_screen(
                "Device sessions now rotate on every refresh. I'll run the bridge tests.",
                "cargo nextest run -p bridge",
                "Run the bridge tests",
                "cargo nextest run",
                PASSPORTER,
            ),
        ),
        agent(
            2,
            MIGRATION,
            Status::Blocked,
            ACME,
            permission_screen(
                "The migration and backfill are written. Next, apply them to the dev database.",
                "sqlx migrate run",
                "Apply the pending migrations",
                "sqlx migrate",
                ACME,
            ),
        ),
        agent(
            3,
            DOCS,
            Status::Working,
            PASSPORTER,
            "⏺ Editing docs/design.md…\n".to_owned(),
        ),
        agent(
            4,
            DEPS,
            Status::Working,
            WIDGET,
            "⏺ Fixing the tokio deprecations…\n".to_owned(),
        ),
        agent(5, RELEASE, Status::Done, PASSPORTER, "⏺ Done.\n".to_owned()),
        agent(6, FLAKY, Status::Done, WIDGET, "⏺ Done.\n".to_owned()),
    ]
}

/// herdr's workspace for each repo: id number and label.
fn workspace(repo_name: &str) -> (u32, &'static str) {
    match repo_name {
        PASSPORTER => (1, PASSPORTER),
        ACME => (2, ACME),
        _ => (3, WIDGET),
    }
}

/// An agent's name, how long ago its turn began, and the turn.
type Turn = (&'static str, Duration, fn(&mut Transcript));

/// The demo's world under `root`: the [`cast`], each mid-turn (the warm-up
/// moves them on), their transcripts, the repos new agents may start in,
/// and a fake `claude` that answers by what each transcript mentions.
fn world(root: &Path) -> std::io::Result<World> {
    let min = |m: u64| Duration::from_secs(m * 60);
    let mut world = World {
        agents: Vec::new(),
        workspaces: [PASSPORTER, ACME, WIDGET]
            .into_iter()
            .map(|r| {
                let (n, label) = workspace(r);
                (format!("w{n}"), label.to_owned())
            })
            .collect(),
        projects_dir: root.join("claude-projects"),
        codex_dir: root.join("codex-sessions"),
        spawn_projects: [PASSPORTER, WIDGET, ACME]
            .into_iter()
            .map(|r| (r.to_owned(), root.join("repos").join(r)))
            .collect(),
        claude: root.join("fake-claude"),
        heard: format!(" {DICTATED}\n"),
    };
    for (_, folder) in &world.spawn_projects {
        std::fs::create_dir_all(folder)?;
    }
    std::fs::create_dir_all(&world.codex_dir)?;

    let turns: [Turn; 6] = [
        (AUTH, min(4), |t| {
            t.user("Rotate device session tokens on refresh, and reject a reused refresh token.")
                .say("I'll look at how sessions are stored first.")
                .bash(
                    "rg -n 'fn refresh' crates/bridge/src/auth",
                    "crates/bridge/src/auth/session.rs:88:    pub fn refresh(&self, token: &RefreshToken) -> Result<Session> {",
                )
                .edit(
                    "crates/bridge/src/auth/session.rs",
                    "let session = self.find(token)?;",
                    "let session = self.take(token)?; // one use only\nlet session = session.rotate(&mut self.rng);",
                )
                .say("Device sessions now rotate on every refresh. I'll run the bridge tests.");
        }),
        (MIGRATION, min(11), |t| {
            t.user("Add an archived_at column to orders, backfilled from the status history.")
                .edit(
                    "migrations/0042_orders_archived_at.sql",
                    "-- empty",
                    "ALTER TABLE orders ADD COLUMN archived_at timestamptz;\nUPDATE orders o SET archived_at = h.at\n  FROM order_history h WHERE h.order_id = o.id AND h.status = 'archived';",
                )
                .say("The migration and backfill are written. Next, apply them to the dev database.");
        }),
        (DOCS, min(2), |t| {
            t.user("Bring docs/design.md up to date with the summary-first thread view.")
                .say("Reading the thread renderer to describe it accurately.");
        }),
        (DEPS, min(6), |t| {
            t.user("Update the dependencies and fix anything that breaks.")
                .bash(
                    "cargo update",
                    "Locking 23 packages to latest compatible versions",
                );
        }),
        (RELEASE, min(18), |t| {
            t.user("Draft release notes for 0.4 from the merged PRs since 0.3.")
                .bash(
                    "gh pr list --state merged --search 'merged:>2026-10-25' --json number,title",
                    "[{\"number\":41,\"title\":\"feat(web): send a suggested reply on one tap\"},\n {\"number\":38,\"title\":\"feat: summary-first agent detail\"},\n {\"number\":36,\"title\":\"perf(web): on-demand extras WASM\"},\n {\"number\":33,\"title\":\"Codex agents: thread, TL;DR and approval chips\"}]",
                )
                .edit(
                    "CHANGELOG.md",
                    "## Unreleased",
                    "## 0.4.0\n\n### Added\n- Codex agents: threads, TL;DRs and approval chips (#33)\n- Summary-first agent detail with turn headlines (#38)\n- One-tap suggested replies (#41)\n\n### Performance\n- Rarely used screens load from an on-demand WASM (#36)",
                )
                .say("Release notes for 0.4 are drafted in CHANGELOG.md: three features (Codex support, summary-first detail, one-tap replies) and one performance change (the on-demand extras WASM, which cuts the first load by about a third).\n\nI left out the internal backlog and CI chores, since they don't change anything users see.\n\nShall I open the PR?");
        }),
        (FLAKY, min(25), |t| {
            t.user("Why does stream_resumes_after_drop fail about one run in twenty?")
                .bash(
                    "cargo nextest run -p server stream_resumes --retries 0 --stress-count 40",
                    "40 runs: 38 passed, 2 FAILED (seeds 0x5e11, 0x9a02)",
                )
                .say("It's a race: the test drops the socket before the hub has registered the subscriber, so the resume cursor is sometimes empty. Pinning the seed reproduces it every time. The fix is to wait for the subscription ack before dropping.");
        }),
    ];
    for agent in cast() {
        let (_, ago, turn) = turns
            .iter()
            .find(|(name, ..)| agent.name.as_deref() == Some(*name))
            .ok_or_else(|| std::io::Error::other("an agent without a turn"))?;
        let mut transcript = Transcript::new(&agent, *ago, 0);
        turn(&mut transcript);
        transcript.write(&world.transcript_file(&agent))?;
        world.agents.push(FakeAgent {
            status: Status::Working,
            screen: "⏺ Working on it…\n".to_owned(),
            ..agent
        });
    }

    std::fs::write(&world.claude, FAKE_CLAUDE)?;
    std::fs::set_permissions(
        &world.claude,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )?;
    Ok(world)
}

/// Appends records to the transcript of the agent called `name`.
fn append(world: &World, name: &str, more: impl FnOnce(&mut Transcript)) -> Result<()> {
    let agent = cast()
        .into_iter()
        .find(|a| a.name.as_deref() == Some(name))
        .ok_or("no such agent")?;
    let file = world.transcript_file(&agent);
    let existing = std::fs::read_to_string(&file)?;
    let last: Value = serde_json::from_str(existing.lines().last().unwrap_or("{}"))?;
    let mut transcript = Transcript::new(&agent, Duration::ZERO, existing.lines().count());
    transcript.parent = last["uuid"].as_str().map(str::to_owned);
    more(&mut transcript);
    let mut text = existing;
    text.push_str(&transcript.jsonl());
    std::fs::write(file, text)?;
    Ok(())
}

/// The approved test run passes and the auth agent wraps up.
fn finish_auth(world: &World) -> Result<()> {
    append(world, AUTH, |t| {
        t.bash(
            "cargo nextest run -p bridge",
            "Summary [   4.112s] 212 tests run: 212 passed, 0 skipped",
        )
        .say("All 212 bridge tests pass. Refresh tokens are now single-use: a reused one revokes the whole device session, and the audit log records it.");
    })
}

/// A prompt sent to `name` shows in its transcript, as Claude Code
/// records it.
fn prompt(world: &World, name: &str, text: &str) -> Result<()> {
    append(world, name, |t| {
        t.user(text);
    })
}

/// A transcript under construction, in Claude Code's JSONL shape.
struct Transcript {
    cwd: PathBuf,
    session: String,
    lines: Vec<Value>,
    parent: Option<String>,
    at: SystemTime,
    /// Records so far, numbering the next one's ids.
    n: usize,
}

impl Transcript {
    /// A transcript for `agent` starting `ago`, after `n` records.
    fn new(agent: &FakeAgent, ago: Duration, n: usize) -> Self {
        Self {
            cwd: agent.cwd.clone(),
            session: agent.session.clone().unwrap_or_default(),
            lines: Vec::new(),
            parent: None,
            at: SystemTime::now() - ago,
            n,
        }
    }

    fn record(&mut self, kind: &str, message: &Value, extra: &Value) -> &mut Self {
        self.n += 1;
        self.at += Duration::from_secs(7);
        let uuid = format!("00000000-0000-4000-8000-{:012}", self.n);
        let mut line = json!({
            "parentUuid": self.parent,
            "isSidechain": false,
            "userType": "external",
            "cwd": self.cwd,
            "sessionId": self.session,
            "version": "2.1.317",
            "gitBranch": "main",
            "type": kind,
            "uuid": uuid,
            "timestamp": rfc3339(self.at),
            "message": message,
        });
        if let (Some(line), Some(extra)) = (line.as_object_mut(), extra.as_object()) {
            line.extend(extra.clone());
        }
        self.parent = Some(uuid);
        self.lines.push(line);
        self
    }

    fn user(&mut self, text: &str) -> &mut Self {
        self.record(
            "user",
            &json!({"role": "user", "content": text}),
            &json!({"origin": {"kind": "human"}}),
        )
    }

    fn assistant(&mut self, content: &Value) -> &mut Self {
        let n = self.n;
        self.record(
            "assistant",
            &json!({"id": format!("msg_demo{n}"), "type": "message", "role": "assistant",
                   "model": "claude-opus-5-5", "content": content,
                   "stop_reason": "end_turn", "stop_sequence": null}),
            &json!({"requestId": format!("req_demo{n}")}),
        )
    }

    fn say(&mut self, text: &str) -> &mut Self {
        self.assistant(&json!([{"type": "text", "text": text}]))
    }

    fn bash(&mut self, command: &str, output: &str) -> &mut Self {
        let id = format!("toolu_demo{}", self.n);
        self.assistant(&json!([{"type": "tool_use", "id": id, "name": "Bash",
            "input": {"command": command, "description": "Run it"}}]));
        self.record(
            "user",
            &json!({"role": "user", "content": [{"tool_use_id": id, "type": "tool_result", "content": output}]}),
            &json!({"toolUseResult": {"stdout": output, "stderr": "", "interrupted": false, "isImage": false}}),
        )
    }

    fn edit(&mut self, file: &str, old: &str, new: &str) -> &mut Self {
        let id = format!("toolu_demo{}", self.n);
        let path = self.cwd.join(file);
        self.assistant(&json!([{"type": "tool_use", "id": id, "name": "Edit",
            "input": {"file_path": path, "old_string": old, "new_string": new}}]));
        let old_lines: Vec<String> = old.lines().map(|l| format!("-{l}")).collect();
        let new_lines: Vec<String> = new.lines().map(|l| format!("+{l}")).collect();
        let (old_count, new_count) = (old_lines.len(), new_lines.len());
        let lines = [old_lines, new_lines].concat();
        self.record(
            "user",
            &json!({"role": "user", "content": [{"tool_use_id": id, "type": "tool_result",
                "content": format!("The file {} has been updated.", path.display())}]}),
            &json!({"toolUseResult": {"filePath": path, "oldString": old, "newString": new,
                "originalFile": old,
                "structuredPatch": [{"oldStart": 1, "oldLines": old_count, "newStart": 1,
                    "newLines": new_count, "lines": lines}]}}),
        )
    }

    /// The records, a line each.
    fn jsonl(&self) -> String {
        self.lines.iter().fold(String::new(), |mut text, line| {
            let _ = writeln!(text, "{line}");
            text
        })
    }

    /// Writes the transcript to `file`, creating its folder.
    fn write(&self, file: &Path) -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, self.jsonl())
    }
}

/// `2026-11-08T09:00:01.000Z` for `at`.
fn rfc3339(at: SystemTime) -> String {
    let secs = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// A stand-in for `claude -p`: answers the bridge's structured requests
/// (by schema) with a headline, status line, TL;DR or reply suggestion
/// chosen by what the transcript in the request mentions.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
req=$(cat)
pick() { case "$req" in *"$1"*) return 0 ;; esac; return 1; }
case "$*" in
  *'"status_line"'*)
    if pick 'design.md'; then s='Rewriting the thread section of design.md'
    elif pick 'cargo update'; then s='Fixing two deprecations after the tokio bump'
    elif pick 'fifty times'; then s='Stress-running the test with the seed pinned'
    elif pick 'open the PR'; then s='Opening the release notes PR'
    elif pick 'rate limiting'; then s='Reading the pairing handler'
    else s='Running the bridge tests'; fi
    out="{\"status_line\":\"$s\"}" ;;
  *'"headline"'*)
    if pick '212 tests'; then h='Refresh tokens rotate and are single-use; 212 tests pass.'
    elif pick 'CHANGELOG'; then h='Drafted 0.4 release notes: three features, one perf win.'
    elif pick 'subscription ack'; then h='Found the race: socket drops before the hub subscribes.'
    elif pick 'archived_at'; then h='Wrote the archived_at migration; wants to apply it.'
    elif pick 'session.rs'; then h='Rotating session tokens; wants to run the bridge tests.'
    else h='Working through the task.'; fi
    out="{\"headline\":\"$h\"}" ;;
  *'"outcome"'*)
    out='{"outcome":"Done; nothing left to decide.","changed":[],"decide":null,"risks":[]}' ;;
  *)
    if pick 'open the PR'; then out='{"suggestions":["Yes, open the PR.","Mention the size win in the title."]}'
    else out='{"suggestions":["Looks good, carry on."]}'; fi ;;
esac
printf '{"type":"result","subtype":"success","is_error":false,"structured_output":%s}\n' "$out"
"#;

// ---------------------------------------------------------------------------
// Video.

/// Sets the captured frames on a 1080p stage with captions and narration.
fn compose(
    out: &Path,
    frames: &[Frame],
    captions: &[Caption],
    end: Duration,
    mp4: &Path,
) -> Result<()> {
    // Frames, each shown until the next one was taken.
    let mut list = String::from("ffconcat version 1.0\n");
    for (i, frame) in frames.iter().enumerate() {
        let next = frames.get(i + 1).map_or(end, |f| f.at);
        let _ = writeln!(
            list,
            "file '{}'\nduration {:.4}",
            frame.path.display(),
            (next.saturating_sub(frame.at)).as_secs_f64()
        );
    }
    if let Some(last) = frames.last() {
        let _ = writeln!(list, "file '{}'", last.path.display());
    }
    let list_file = out.join("frames.ffconcat");
    std::fs::write(&list_file, list)?;

    let ass_file = out.join("captions.ass");
    std::fs::write(&ass_file, ass(captions, end))?;

    // The phone: 900 px tall with rounded corners, in a thin bezel.
    let h = 900;
    let w = h * PHONE.0 / PHONE.1;
    let radius = 46;
    let bezel = 12;
    let mut filter = format!(
        "color=c=0x0b0f17:s=1920x1080:r={FPS},format=rgba[bgc];\
         [bgc]geq=r='11+18*(1-Y/H)':g='15+22*(1-Y/H)':b='23+40*(1-Y/H)':a=255,trim=end_frame=1,loop=-1:1,setpts=N/{FPS}/TB[bg];\
         [0:v]fps={FPS},scale={w}:{h}:flags=lanczos,format=rgba,\
           geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='{mask}'[phone];\
         color=c=0x1c2230:s={bw}x{bh}:r={FPS},format=rgba,\
           geq=r='38':g='38':b='52':a='{bmask}',trim=end_frame=1,loop=-1:1,setpts=N/{FPS}/TB[frame];\
         [bg][frame]overlay=x={fx}:y={fy}:shortest=1[s1];\
         [s1][phone]overlay=x={px}:y={py}:shortest=1,\
           ass='{ass}',format=yuv420p[v]",
        mask = rounded_mask(w, h, radius),
        bw = w + 2 * bezel,
        bh = h + 2 * bezel,
        bmask = rounded_mask(w + 2 * bezel, h + 2 * bezel, radius + bezel),
        fx = 1920 - 220 - w - 2 * bezel,
        fy = (1080 - h) / 2 - bezel,
        px = 1920 - 220 - w - bezel,
        py = (1080 - h) / 2,
        ass = ass_file.display(),
    );
    let mut command = std::process::Command::new("ffmpeg");
    command
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
        ])
        .arg(&list_file);
    // The narration: each scene's line at the scene's start.
    let mut voices = String::new();
    let mut n = 0;
    for caption in captions {
        if let Some((clip, _)) = &caption.clip {
            n += 1;
            command.arg("-i").arg(clip);
            let ms = caption.at.as_millis();
            let _ = write!(filter, ";[{n}:a]aresample=48000,adelay={ms}:all=1[a{n}]");
            let _ = write!(voices, "[a{n}]");
        }
    }
    if n > 0 {
        let _ = write!(
            filter,
            ";{voices}amix=inputs={n}:normalize=0,apad,loudnorm=I=-16:TP=-1.5[a]"
        );
    }
    command.args(["-filter_complex", &filter, "-map", "[v]"]);
    if n > 0 {
        command.args(["-map", "[a]", "-c:a", "aac", "-b:a", "128k"]);
    }
    let status = command
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "slow",
            "-crf",
            "20",
            "-movflags",
            "+faststart",
        ])
        .args(["-t", &format!("{:.2}", end.as_secs_f64())])
        .arg(mp4)
        .status()?;
    if !status.success() {
        return Err(format!("ffmpeg failed: {status}").into());
    }
    Ok(())
}

/// A `geq` alpha expression: 255 inside a `w`×`h` rounded rectangle of
/// corner radius `r`, 0 outside, antialiased over a pixel.
fn rounded_mask(w: u32, h: u32, r: u32) -> String {
    let (w, h, r) = (f64::from(w), f64::from(h), f64::from(r));
    format!(
        "255*clip({r}+0.5-hypot(max(0,max({r}-X,X-({w}-1-{r}))),max(0,max({r}-Y,Y-({h}-1-{r})))),0,1)"
    )
}

/// An ASS subtitle script: the brand, and each caption's title and body
/// on the left of the stage, fading in, until the next caption.
fn ass(captions: &[Caption], end: Duration) -> String {
    let mut out = String::from(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: 1920\nPlayResY: 1080\nWrapStyle: 2\n\n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
         Style: Title,Liberation Sans,78,&H00FFFFFF,&H00FFFFFF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,0,0,4,200,0,0,1\n\
         Style: Body,Liberation Sans,40,&H00D8CCBF,&H00D8CCBF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,0,0,7,200,0,0,1\n\
         Style: Brand,Liberation Sans,30,&H00F0B47A,&H00F0B47A,&H00000000,&H00000000,-1,0,0,0,100,100,2,0,1,0,0,7,200,0,0,1\n\n\
         [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
    );
    let _ = writeln!(
        out,
        "Dialogue: 0,{},{},Brand,,0,0,0,,{{\\pos(200,110)}}PASSPORTER",
        ass_time(Duration::ZERO),
        ass_time(end)
    );
    for (i, caption) in captions.iter().enumerate() {
        let (from, until) = (
            ass_time(caption.at),
            ass_time(captions.get(i + 1).map_or(end, |c| c.at)),
        );
        let _ = writeln!(
            out,
            "Dialogue: 0,{from},{until},Title,,0,0,0,,{{\\fad(250,150)\\pos(200,470)}}{}",
            caption.scene.title
        );
        let _ = writeln!(
            out,
            "Dialogue: 0,{from},{until},Body,,0,0,0,,{{\\fad(350,150)\\pos(200,530)}}{}",
            caption.scene.body.replace('\n', "\\N")
        );
    }
    out
}

/// `H:MM:SS.cc`, as ASS writes times.
fn ass_time(at: Duration) -> String {
    let cs = at.as_millis() / 10;
    format!(
        "{}:{:02}:{:02}.{:02}",
        cs / 360_000,
        cs / 6_000 % 60,
        cs / 100 % 60,
        cs % 100
    )
}

/// Standard base64 decoding, enough for CDP screenshots.
mod base64 {
    pub(super) fn decode(text: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(text.len() * 3 / 4);
        let mut acc = 0u32;
        let mut bits = 0;
        for byte in text.bytes() {
            let v = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' | b'\n' | b'\r' => continue,
                _ => return Err(format!("bad base64 byte {byte}")),
            };
            acc = (acc << 6) | u32::from(v);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(u8::try_from((acc >> bits) & 0xff).map_err(|e| e.to_string())?);
            }
        }
        Ok(out)
    }
}
