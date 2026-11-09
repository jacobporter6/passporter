//! `WS /v1/stream`: snapshot, live changes, resync and backpressure.

use std::sync::Arc;
use std::time::Duration;

use herdr::{AgentStatus, ErrorCode, Fault, Herdr, Method};
use protocol::{AgentEvent, Seq};
use tokio::net::{TcpListener, TcpStream};

use crate::agents::start_agent;
use crate::harness::{self, Bridge, CuttableHerdr, PATIENCE, Stream, StubAuth, TOKEN};

fn updated(event: &AgentEvent) -> Option<&protocol::Agent> {
    match event {
        AgentEvent::AgentUpdated { agent } => agent.as_ref(),
        _ => None,
    }
}

fn is_update_of(event: &AgentEvent, pane: &herdr::PaneId) -> bool {
    updated(event).is_some_and(|a| a.id.pane().as_str() == pane.as_str())
}

/// The agent list a snapshot carries.
fn snapshot(event: AgentEvent) -> Vec<protocol::Agent> {
    match event {
        AgentEvent::Snapshot { agents } => agents,
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

#[tokio::test]
async fn status_change_streams_an_update_with_the_new_seq() {
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("worker", "claude", AgentStatus::Idle);
    let bridge = Bridge::start(herdr).await;
    bridge.wait_for_agents("the agent", |a| a.len() == 1).await;

    let mut stream = bridge.stream().await;
    let agents = snapshot(stream.next_event().await);
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].seq, Seq(1));

    bridge.fake().set_status(&pane, AgentStatus::Working);
    let event = stream
        .wait_for("working", |e| {
            updated(e).is_some_and(|a| a.status == protocol::AgentStatus::Working)
        })
        .await;
    let agent = updated(&event).expect("agent");
    assert_eq!(agent.seq, Seq(2));
    assert!(agent.status_since >= agents[0].status_since);

    bridge.fake().set_status(&pane, AgentStatus::Blocked);
    let event = stream
        .wait_for("blocked", |e| {
            updated(e).is_some_and(|a| a.status == protocol::AgentStatus::Blocked)
        })
        .await;
    assert_eq!(updated(&event).expect("agent").seq, Seq(3));
    assert_eq!(bridge.agents().await[0].seq, Seq(3), "the list agrees");

    stream.close().await;
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn agents_appear_and_disappear() {
    let cwd = tempfile::tempdir().expect("tmp");
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    let mut stream = bridge.stream().await;
    assert_eq!(snapshot(stream.next_event().await), Vec::new());

    // Appears through pane.created + pane.agent_detected.
    let first = start_agent(&bridge.herdr, "first", "claude", cwd.path()).await;
    let event = stream
        .wait_for("first appears", |e| is_update_of(e, &first))
        .await;
    assert_eq!(updated(&event).expect("agent").title, "first");

    // Its status is followed from then on.
    bridge.fake().set_status(&first, AgentStatus::Working);
    stream
        .wait_for("first works", |e| {
            is_update_of(e, &first)
                && updated(e).is_some_and(|a| a.status == protocol::AgentStatus::Working)
        })
        .await;

    // The agent exits but the pane stays: released.
    bridge.fake().release_agent(&first);
    let event = stream
        .wait_for("first released", |e| {
            matches!(e, AgentEvent::AgentRemoved { .. })
        })
        .await;
    assert!(
        matches!(&event, AgentEvent::AgentRemoved { agent_id } if agent_id.pane().as_str() == first.as_str())
    );

    // A second agent's pane closes.
    let second = start_agent(&bridge.herdr, "second", "codex", cwd.path()).await;
    stream
        .wait_for("second appears", |e| is_update_of(e, &second))
        .await;
    bridge.fake().close_pane(&second).await.expect("close");
    let event = stream
        .wait_for("second closed", |e| {
            matches!(e, AgentEvent::AgentRemoved { .. })
        })
        .await;
    assert!(
        matches!(&event, AgentEvent::AgentRemoved { agent_id } if agent_id.pane().as_str() == second.as_str())
    );
    assert_eq!(bridge.agents().await, Vec::new());

    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn resyncs_after_the_event_stream_ends() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    let mut stream = bridge.stream().await;
    let _ = stream.next_event().await;

    // herdr reports no event for this agent: only a resync can find it.
    let silent = bridge.fake().add_agent("silent", "pi", AgentStatus::Done);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        bridge.agents().await.is_empty(),
        "not visible without a resync"
    );

    bridge.herdr.cut_streams();
    let event = stream
        .wait_for("resync finds it", |e| is_update_of(e, &silent))
        .await;
    assert_eq!(
        updated(&event).expect("agent").status,
        protocol::AgentStatus::Done
    );

    // Live updates work again after resubscribing.
    bridge.fake().set_status(&silent, AgentStatus::Working);
    stream
        .wait_for("live again", |e| {
            updated(e).is_some_and(|a| a.status == protocol::AgentStatus::Working)
        })
        .await;

    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn resyncs_after_a_herdr_error() {
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("flaky", "claude", AgentStatus::Idle);
    let bridge = Bridge::start(herdr).await;
    bridge.wait_for_agents("the agent", |a| a.len() == 1).await;
    let mut stream = bridge.stream().await;
    let _ = stream.next_event().await;

    // The refresh after this change fails; the resync that follows recovers.
    bridge
        .fake()
        .fail_next(Method::AgentGet, Fault::Reject(ErrorCode::InternalError));
    bridge.fake().set_status(&pane, AgentStatus::Blocked);
    let event = stream
        .wait_for("blocked after recovery", |e| {
            updated(e).is_some_and(|a| a.status == protocol::AgentStatus::Blocked)
        })
        .await;
    assert_eq!(updated(&event).expect("agent").seq, Seq(2));

    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn starts_even_if_herdr_refuses_the_first_subscription() {
    let herdr = CuttableHerdr::new();
    herdr.fake.add_agent("patient", "claude", AgentStatus::Idle);
    herdr.fake.fail_next(
        Method::EventsSubscribe,
        Fault::Reject(ErrorCode::ServerUnavailable),
    );
    let bridge = Bridge::start(herdr).await;
    bridge
        .wait_for_agents("the agent after retry", |a| a.len() == 1)
        .await;
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn periodic_resync_catches_silent_changes() {
    let bridge = Bridge::start_with(CuttableHerdr::new(), |s| {
        s.registry.resync_every = Duration::from_millis(50);
    })
    .await;
    let mut stream = bridge.stream().await;
    let _ = stream.next_event().await;
    let silent = bridge.fake().add_agent("silent", "pi", AgentStatus::Idle);
    stream
        .wait_for("periodic resync", |e| is_update_of(e, &silent))
        .await;
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_slow_consumer_gets_a_fresh_snapshot_instead_of_a_backlog() {
    const FLIPS: u64 = 40;
    let dist = harness::web_dist();
    let mut settings = harness::settings(&dist.path());
    settings.stream_queue = 2;
    let herdr = Arc::new(CuttableHerdr::new());
    let pane = herdr.fake.add_agent("busy", "claude", AgentStatus::Idle);
    // A 256-byte pipe: the server's writes stall as soon as the client stops reading.
    let (listener, connector) = harness::duplex_listener(256);
    let (trigger, shutdown) = bridge::shutdown::channel();
    let task = tokio::spawn(bridge::run(
        Arc::clone(&herdr),
        Arc::new(StubAuth),
        settings,
        listener,
        shutdown,
    ));
    let seq_now = || async {
        let auth = format!("Bearer {TOKEN}");
        let response = harness::http(
            connector.connect(),
            "GET",
            "/v1/agents",
            &[("authorization", &auth)],
        )
        .await;
        let agents: Vec<protocol::Agent> = serde_json::from_str(&response.body).expect("list");
        agents.first().map(|a| a.seq)
    };
    tokio::time::timeout(PATIENCE, async {
        while seq_now().await.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("agent listed");

    let mut stream = Stream::connect(connector.connect(), Some(TOKEN))
        .await
        .expect("handshake");
    assert_eq!(snapshot(stream.next_event().await).len(), 1);

    // The client stops reading while the agent changes many times.
    for i in 0..FLIPS {
        let status = if i % 2 == 0 {
            AgentStatus::Working
        } else {
            AgentStatus::Idle
        };
        herdr.fake.set_status(&pane, status);
        tokio::task::yield_now().await;
    }
    let last = Seq(FLIPS + 1);
    tokio::time::timeout(PATIENCE, async {
        while seq_now().await != Some(last) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("registry caught up");

    // Reading again: a few stale updates, then a snapshot with the final state.
    let mut updates = 0;
    let resync = loop {
        match stream.next_event().await {
            AgentEvent::Snapshot { agents } => break agents,
            AgentEvent::AgentUpdated { .. } => updates += 1,
            other => panic!("unexpected {other:?}"),
        }
    };
    assert!(
        updates < FLIPS,
        "the backlog was dropped ({updates} updates)"
    );
    let final_seq = resync[0].seq;
    if final_seq != last {
        // The snapshot may predate the last few flips; they follow as updates.
        stream
            .wait_for("the final state", |e| {
                updated(e).is_some_and(|a| a.seq == last)
            })
            .await;
    }

    trigger.trigger();
    tokio::time::timeout(PATIENCE, task)
        .await
        .expect("stops")
        .expect("task")
        .expect("clean stop");
}

#[tokio::test]
async fn a_client_that_never_answers_pings_is_dropped() {
    let bridge = Bridge::start_with(CuttableHerdr::new(), |s| {
        s.server.stream.ping_every = Duration::from_millis(50);
    })
    .await;
    let mut stream = bridge.stream().await;
    // Not reading means not answering pings (the client pongs as it reads).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(stream.close_code().await, Some(1008), "policy violation");
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_responsive_client_stays_connected() {
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("steady", "claude", AgentStatus::Idle);
    let bridge = Bridge::start_with(herdr, |s| {
        s.server.stream.ping_every = Duration::from_millis(30);
    })
    .await;
    bridge.wait_for_agents("the agent", |a| a.len() == 1).await;
    let mut stream = bridge.stream().await;
    let _ = stream.next_event().await;
    // Reading for several ping intervals answers every ping.
    let idle = tokio::time::timeout(Duration::from_millis(200), stream.next_event()).await;
    assert!(idle.is_err(), "no events while nothing changes");
    bridge.fake().set_status(&pane, AgentStatus::Working);
    stream
        .wait_for("still live", |e| updated(e).is_some())
        .await;
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn shutdown_closes_streams_and_returns() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    let mut stream = bridge.stream().await;
    let _ = stream.next_event().await;
    let stopped = tokio::spawn(bridge.stop());
    assert_eq!(stream.close_code().await, Some(1001), "going away");
    stopped.await.expect("join").expect("clean stop");
}

#[test]
fn close_frames_are_sent_before_the_runtime_is_dropped() {
    // The bridge runs in its own runtime that is dropped as soon as `run`
    // returns, exactly as the binary does.
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let (trigger, shutdown) = bridge::shutdown::channel();
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let dist = harness::web_dist();
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            addr_tx
                .send(listener.local_addr().expect("addr"))
                .expect("send");
            bridge::run(
                Arc::new(CuttableHerdr::new()),
                Arc::new(StubAuth),
                harness::settings(&dist.path()),
                listener,
                shutdown,
            )
            .await
        })
    });
    let addr = addr_rx.recv().expect("bridge address");
    let client = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("client runtime");
    let mut stream = client.block_on(async {
        let tcp = TcpStream::connect(addr).await.expect("connect");
        let mut stream = Stream::connect(tcp, Some(TOKEN)).await.expect("handshake");
        let _ = stream.next_event().await;
        stream
    });
    trigger.trigger();
    server.join().expect("bridge thread").expect("clean stop");
    // The bridge's runtime is gone; the close frame must already be on the wire.
    assert_eq!(client.block_on(stream.close_code()), Some(1001));
}

#[tokio::test]
async fn a_failing_agent_read_is_retried_without_ending_the_session() {
    for code in [
        ErrorCode::Timeout,
        ErrorCode::AgentLaunchPending,
        ErrorCode::InternalError,
    ] {
        let herdr = CuttableHerdr::new();
        let pane = herdr.fake.add_agent("slow", "claude", AgentStatus::Idle);
        let other = herdr.fake.add_agent("other", "codex", AgentStatus::Idle);
        let bridge = Bridge::start(herdr).await;
        bridge
            .wait_for_agents("both agents", |a| a.len() == 2)
            .await;
        let mut stream = bridge.stream().await;
        let _ = stream.next_event().await;
        let sessions = bridge.herdr.lifecycle_subscriptions();

        // Fail this pane's next two reads; the pane is retried on its own.
        for _ in 0..2 {
            bridge
                .fake()
                .fail_next(Method::AgentGet, Fault::Reject(code.clone()));
        }
        bridge.fake().set_status(&pane, AgentStatus::Blocked);
        let event = stream
            .wait_for("the retried read", |e| is_update_of(e, &pane))
            .await;
        assert_eq!(updated(&event).expect("agent").seq, Seq(2), "{code}");

        // Other agents stayed live throughout, on the same session.
        bridge.fake().set_status(&other, AgentStatus::Working);
        stream
            .wait_for("the other agent", |e| is_update_of(e, &other))
            .await;
        assert_eq!(
            bridge.herdr.lifecycle_subscriptions(),
            sessions,
            "{code}: the lifecycle session was not restarted"
        );
        bridge.stop().await.expect("clean stop");
    }
}

#[tokio::test]
async fn a_failing_periodic_resync_keeps_the_session() {
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("steady", "claude", AgentStatus::Idle);
    let bridge = Bridge::start_with(herdr, |s| {
        s.registry.resync_every = Duration::from_millis(50);
    })
    .await;
    bridge.wait_for_agents("the agent", |a| a.len() == 1).await;
    let sessions = bridge.herdr.lifecycle_subscriptions();
    bridge
        .fake()
        .fail_next(Method::AgentList, Fault::Reject(ErrorCode::Timeout));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut stream = bridge.stream().await;
    let _ = stream.next_event().await;
    bridge.fake().set_status(&pane, AgentStatus::Working);
    stream
        .wait_for("still live", |e| is_update_of(e, &pane))
        .await;
    assert_eq!(bridge.herdr.lifecycle_subscriptions(), sessions);
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_launching_agent_survives_resyncs_that_do_not_list_it() {
    let cwd = tempfile::tempdir().expect("tmp");
    let bridge = Bridge::start_with(CuttableHerdr::new(), |s| {
        s.registry.resync_every = Duration::from_millis(20);
        s.registry.retry_min = Duration::from_millis(300);
        s.registry.retry_max = Duration::from_millis(300);
    })
    .await;
    let mut stream = bridge.stream().await;
    let _ = stream.next_event().await;
    let created = bridge
        .fake()
        .create_workspace("late", Some(cwd.path()))
        .await
        .expect("workspace");
    let pane = created.root_pane.pane_id;
    bridge.herdr.unlist(&pane);
    bridge.fake().fail_next(
        Method::AgentGet,
        Fault::Reject(ErrorCode::AgentLaunchPending),
    );
    bridge
        .fake()
        .detect_agent(&pane, "claude", AgentStatus::Idle);
    // Several resyncs run (and never list it) before the retry is due.
    let event = stream
        .wait_for("the retried agent", |e| is_update_of(e, &pane))
        .await;
    assert_eq!(
        updated(&event).expect("agent").kind,
        protocol::AgentKind::Claude
    );
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn many_repos_keep_their_names_across_resyncs() {
    const AGENTS: usize = 12;
    let repos = tempfile::tempdir().expect("tmp");
    let herdr = CuttableHerdr::new();
    for i in 0..AGENTS {
        let dir = repos.path().join(format!("repo-{i}"));
        std::fs::create_dir_all(dir.join(".git")).expect("git dir");
        start_agent(&herdr, &format!("agent-{i}"), "claude", &dir).await;
    }
    let bridge = Bridge::start_with(herdr, |s| {
        s.registry.resync_every = Duration::from_millis(30);
    })
    .await;
    let agents = bridge
        .wait_for_agents("every agent with its repo", |a| {
            a.len() == AGENTS && a.iter().all(|agent| agent.repo.starts_with("repo-"))
        })
        .await;
    assert_eq!(agents.len(), AGENTS);
    // Resyncs revalidate in the background and never blank a known repo.
    let mut stream = bridge.stream().await;
    let _ = stream.next_event().await;
    let watch = async {
        loop {
            if let Some(agent) = updated(&stream.next_event().await) {
                assert!(agent.repo.starts_with("repo-"), "blanked: {agent:?}");
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(300), watch).await;
    bridge.stop().await.expect("clean stop");
}
