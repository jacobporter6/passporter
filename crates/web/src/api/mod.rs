//! Typed client for the bridge API (design §5), over `crates/protocol`.
//!
//! Three layers, so everything but the actual `fetch` is tested on the host:
//!
//! - [`request`]: endpoint → [`HttpRequest`] (method, path, headers, body).
//! - [`response`]: [`HttpResponse`] → typed value or [`ApiError`], including
//!   409 stale and refusals as [`ActionOutcome`] values.
//! - [`Transport`]: sends one request. [`FetchTransport`] is the browser one;
//!   [`TimedFetch`] gives up after a while.
//!
//! [`ApiClient`] glues them together with a [`TokenSource`].

pub mod auth;
mod fetch;
pub mod request;
pub mod response;

use core::future::Future;

use protocol::{
    Action, Agent, AgentId, AgentScreen, BridgeVersion, CommandList, CommandResult,
    CommandRunRequest, PaneId, PendingInput, PushKey, PushSubscriptions, RunbookList, RunbookName,
    RunbookResult, RunbookRunRequest, SpawnOptions, SpawnRequest, SpawnResult, TerminalKeysRequest,
    TerminalScreen, ThreadCursor, ThreadPage, TurnIndex,
};
use serde::de::DeserializeOwned;

pub use auth::{BearerToken, MemoryToken, TokenSource, Unpaired, parse_pasted_token};
pub use fetch::{FetchTransport, TimedFetch};
pub(crate) use fetch::{RawRequest, fetch_raw};
pub use request::{Endpoint, HttpRequest, Method};
pub use response::{
    ActionOutcome, ApiError, HttpResponse, StatusRefresh, SuggestionsRead, TldrRead,
};

/// Sends one HTTP request. Resolves to `Err` only when there was no HTTP
/// response at all; any status, including errors, is an `Ok`.
pub trait Transport {
    /// Sends `request`.
    fn send(&self, request: HttpRequest) -> impl Future<Output = Result<HttpResponse, ApiError>>;
}

/// The bridge API, typed end to end.
#[derive(Debug)]
pub struct ApiClient<T, A> {
    transport: T,
    auth: A,
}

impl<T: Transport, A: TokenSource> ApiClient<T, A> {
    /// A client that sends through `transport` with tokens from `auth`.
    pub const fn new(transport: T, auth: A) -> Self {
        Self { transport, auth }
    }

    /// `GET /v1/agents`.
    pub async fn agents(&self) -> Result<Vec<Agent>, ApiError> {
        self.read(&Endpoint::Agents).await
    }

    /// `GET /v1/agents/:id/thread?since=&limit=`.
    pub async fn thread(
        &self,
        agent: AgentId,
        since: Option<ThreadCursor>,
        limit: Option<u32>,
    ) -> Result<ThreadPage, ApiError> {
        self.read(&Endpoint::Thread {
            agent,
            since,
            limit,
        })
        .await
    }

    /// `GET /v1/agents/:id/screen`.
    pub async fn screen(&self, agent: AgentId) -> Result<AgentScreen, ApiError> {
        self.read(&Endpoint::Screen(agent)).await
    }

    /// `GET /v1/agents/:id/pending`; `None` when nothing is pending.
    pub async fn pending(&self, agent: AgentId) -> Result<Option<PendingInput>, ApiError> {
        self.read(&Endpoint::Pending(agent)).await
    }

    /// `GET /v1/agents/:id/tldr?turn=`: the TL;DR of that (else the
    /// latest) finished turn, [`TldrRead::Generating`] while the bridge
    /// makes it, or [`TldrRead::None`].
    pub async fn tldr(
        &self,
        agent: AgentId,
        turn: Option<TurnIndex>,
    ) -> Result<TldrRead, ApiError> {
        let request = request::get(&Endpoint::Tldr { agent, turn }, self.auth.current());
        let response = self.send(request).await?;
        response::read_tldr(&response)
    }

    /// `GET /v1/agents/:id/suggestions`: short replies to what the agent
    /// asks, or [`SuggestionsRead::Generating`] until the bridge has them.
    pub async fn suggestions(&self, agent: AgentId) -> Result<SuggestionsRead, ApiError> {
        let request = request::get(&Endpoint::Suggestions(agent), self.auth.current());
        let response = self.send(request).await?;
        response::read_suggestions(&response)
    }

    /// `POST /v1/agents/:id/status`: asks for a fresh status line, which
    /// arrives on the stream.
    pub async fn refresh_status(&self, agent: &AgentId) -> Result<StatusRefresh, ApiError> {
        let request = request::post_status_refresh(agent, self.auth.current());
        let response = self.send(request).await?;
        response::read_status_refresh(&response)
    }

    /// `POST /v1/actions`. Stale (409) and refusals come back as
    /// [`ActionOutcome`] values, not errors; see [`response::action_outcome`].
    pub async fn act(&self, action: &Action) -> Result<ActionOutcome, ApiError> {
        let request = request::post_action(action, self.auth.current())
            .map_err(|e| ApiError::Encode(e.to_string()))?;
        let response = self.send(request).await?;
        response::action_outcome(&response)
    }

    /// `GET /v1/projects`: the projects and kinds this device may start.
    pub async fn spawn_options(&self) -> Result<SpawnOptions, ApiError> {
        self.read(&Endpoint::Projects).await
    }

    /// `POST /v1/agents`. Refusals come back as [`SpawnResult`] values,
    /// not errors; see [`response::spawn_outcome`].
    pub async fn spawn(&self, request: &SpawnRequest) -> Result<SpawnResult, ApiError> {
        let request = request::post_spawn(request, self.auth.current())
            .map_err(|e| ApiError::Encode(e.to_string()))?;
        let response = self.send(request).await?;
        response::spawn_outcome(&response)
    }

    /// `GET /v1/runbooks`: the runbooks this device may run, each with its
    /// exact command and latest run.
    pub async fn runbooks(&self) -> Result<RunbookList, ApiError> {
        self.read(&Endpoint::Runbooks).await
    }

    /// `POST /v1/runbooks/:name/run`. Refusals come back as
    /// [`RunbookResult`] values, not errors; see [`response::runbook_outcome`].
    pub async fn run_runbook(
        &self,
        name: &RunbookName,
        request: &RunbookRunRequest,
    ) -> Result<RunbookResult, ApiError> {
        let request = request::post_runbook_run(name, request, self.auth.current())
            .map_err(|e| ApiError::Encode(e.to_string()))?;
        let response = self.send(request).await?;
        response::runbook_outcome(&response)
    }

    /// `GET /v1/agents/:id/commands`: every command in the agent's latest
    /// message.
    pub async fn commands(&self, agent: AgentId) -> Result<CommandList, ApiError> {
        self.read(&Endpoint::Commands(agent)).await
    }

    /// `POST /v1/agents/:id/commands/run`. Refusals come back as
    /// [`CommandResult`] values, not errors; see
    /// [`response::command_outcome`].
    pub async fn run_command(
        &self,
        agent: &AgentId,
        request: &CommandRunRequest,
    ) -> Result<CommandResult, ApiError> {
        let request = request::post_command_run(agent, request, self.auth.current())
            .map_err(|e| ApiError::Encode(e.to_string()))?;
        let response = self.send(request).await?;
        response::command_outcome(&response)
    }

    /// `GET /v1/terminals/:pane/screen`: a command terminal's last lines.
    pub async fn terminal_screen(&self, pane: PaneId) -> Result<TerminalScreen, ApiError> {
        self.read(&Endpoint::TerminalScreen(pane)).await
    }

    /// `POST /v1/terminals/:pane/keys`, answered like
    /// [`run_command`](Self::run_command).
    pub async fn terminal_keys(
        &self,
        pane: &PaneId,
        request: &TerminalKeysRequest,
    ) -> Result<CommandResult, ApiError> {
        let request = request::post_terminal_keys(pane, request, self.auth.current())
            .map_err(|e| ApiError::Encode(e.to_string()))?;
        let response = self.send(request).await?;
        response::command_outcome(&response)
    }

    /// `GET /v1/push/key`. 404 when push is off on the bridge.
    pub async fn push_key(&self) -> Result<PushKey, ApiError> {
        self.read(&Endpoint::PushKey).await
    }

    /// `GET /v1/push/subscriptions`: the endpoints the bridge holds for
    /// this device.
    pub async fn push_endpoints(&self) -> Result<Vec<String>, ApiError> {
        let list: PushSubscriptions = self.read(&Endpoint::PushSubscriptions).await?;
        Ok(list.subscriptions.into_iter().map(|s| s.endpoint).collect())
    }

    /// `POST /v1/push/subscriptions` with `subscription`, the browser's
    /// `PushSubscription` as JSON. Registering the same endpoint again
    /// replaces it.
    pub async fn register_push(&self, subscription: String) -> Result<(), ApiError> {
        let request = request::post_push_subscription(subscription, self.auth.current());
        response::expect_success(&self.send(request).await?)
    }

    /// `DELETE /v1/push/subscriptions` for `endpoint`. A subscription the
    /// bridge no longer has (404) counts as removed.
    pub async fn unregister_push(&self, endpoint: &str) -> Result<(), ApiError> {
        let request = request::delete_push_subscription(endpoint, self.auth.current());
        match response::expect_success(&self.send(request).await?) {
            Err(ApiError::Status { status: 404 }) => Ok(()),
            other => other,
        }
    }

    /// `GET /v1/version`: the bridge's build. Sent without the token: the
    /// endpoint is public, so it answers a device that can't sign in too.
    pub async fn version(&self) -> Result<BridgeVersion, ApiError> {
        let response = self.send(request::get(&Endpoint::Version, None)).await?;
        response::read_json(&response)
    }

    async fn read<R: DeserializeOwned>(&self, endpoint: &Endpoint) -> Result<R, ApiError> {
        let request = request::get(endpoint, self.auth.current());
        let response = self.send(request).await?;
        response::read_json(&response)
    }

    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, ApiError> {
        let sent_with = request.auth.clone();
        let response = self.transport.send(request).await?;
        if response.status == response::UNAUTHORIZED
            && let Some(token) = sent_with
        {
            self.auth.unauthorized(&token);
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use protocol::{ActionResult, RefusalReason};

    use super::*;

    /// Returns canned responses and records what it was asked to send.
    #[derive(Default)]
    struct FakeTransport {
        responses: RefCell<Vec<Result<HttpResponse, ApiError>>>,
        sent: RefCell<Vec<HttpRequest>>,
    }

    impl FakeTransport {
        fn answering(status: u16, body: &str) -> Self {
            let fake = Self::default();
            fake.responses
                .borrow_mut()
                .push(Ok(HttpResponse::new(status, body)));
            fake
        }
    }

    impl Transport for &FakeTransport {
        fn send(
            &self,
            request: HttpRequest,
        ) -> impl Future<Output = Result<HttpResponse, ApiError>> {
            self.sent.borrow_mut().push(request);
            let next = self.responses.borrow_mut().remove(0);
            core::future::ready(next)
        }
    }

    /// The fakes resolve immediately, so one poll suffices.
    fn now<F: Future>(future: F) -> F::Output {
        match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("fake future was pending"),
        }
    }

    fn agent() -> AgentId {
        "w1:p1".parse().unwrap()
    }

    #[test]
    fn reads_send_the_token_and_parse() {
        let fake = FakeTransport::answering(200, "null");
        let client = ApiClient::new(&fake, MemoryToken::new(BearerToken::new("t").unwrap()));
        assert_eq!(now(client.pending(agent())), Ok(None));
        let sent = fake.sent.borrow();
        assert_eq!(sent[0].path, "/v1/agents/w1:p1/pending");
        assert_eq!(sent[0].auth, Some(BearerToken::new("t").unwrap()));
    }

    #[test]
    fn unauthorized_tells_the_token_source() {
        let fake = FakeTransport::answering(401, "");
        let client = ApiClient::new(&fake, MemoryToken::new(BearerToken::new("t").unwrap()));
        assert_eq!(now(client.agents()), Err(ApiError::Unauthorized));
        assert_eq!(client.auth.current(), None);
    }

    #[test]
    fn transport_errors_pass_through_as_retryable() {
        let fake = FakeTransport::default();
        fake.responses
            .borrow_mut()
            .push(Err(ApiError::Transport("offline".into())));
        let client = ApiClient::new(&fake, Unpaired);
        let err = now(client.agents()).unwrap_err();
        assert!(err.is_retryable());
    }

    #[test]
    fn push_registration_round_trip() {
        let fake = FakeTransport::answering(200, r#"{"vapidPublicKey":"BAAA"}"#);
        let client = ApiClient::new(&fake, Unpaired);
        assert_eq!(now(client.push_key()).unwrap().vapid_public_key, "BAAA");

        fake.responses
            .borrow_mut()
            .push(Ok(HttpResponse::new(201, "")));
        assert_eq!(now(client.register_push("{}".into())), Ok(()));
        fake.responses
            .borrow_mut()
            .push(Ok(HttpResponse::new(409, "full")));
        assert_eq!(
            now(client.register_push("{}".into())),
            Err(ApiError::Status { status: 409 })
        );

        fake.responses
            .borrow_mut()
            .push(Ok(HttpResponse::new(404, "")));
        assert_eq!(now(client.unregister_push("https://e")), Ok(()));

        fake.responses.borrow_mut().push(Ok(HttpResponse::new(
            200,
            r#"{"subscriptions":[{"endpoint":"https://e/1"}]}"#,
        )));
        assert_eq!(
            now(client.push_endpoints()),
            Ok(vec!["https://e/1".to_owned()])
        );
        let sent = fake.sent.borrow();
        assert_eq!(sent[1].path, "/v1/push/subscriptions");
        assert_eq!(sent[3].method, Method::Delete);
        assert_eq!(sent[4].method, Method::Get);
    }

    #[test]
    fn the_version_is_read_without_the_token() {
        let fake = FakeTransport::answering(200, r#"{"bridge":"0123abcd","built":"x"}"#);
        let client = ApiClient::new(&fake, MemoryToken::new(BearerToken::new("t").unwrap()));
        assert_eq!(now(client.version()).unwrap().bridge, "0123abcd");
        let sent = fake.sent.borrow();
        assert_eq!(sent[0].path, "/v1/version");
        assert_eq!(sent[0].auth, None);
    }

    #[test]
    fn act_returns_refusals_as_values() {
        let fake = FakeTransport::answering(429, r#"{"status":"refused","reason":"rate_limited"}"#);
        let client = ApiClient::new(&fake, Unpaired);
        let action: Action = serde_json::from_str(
            r#"{"idempotencyKey":"9ca09978-ef89-46fd-9a85-118b84bee7b4","agentId":"w1:p1",
                "expectedSeq":1,"createdAt":0,"kind":"interrupt","payload":{},"origin":"app"}"#,
        )
        .unwrap();
        let outcome = now(client.act(&action)).unwrap();
        assert_eq!(
            outcome,
            ActionOutcome::Result(ActionResult::Refused {
                reason: RefusalReason::RateLimited,
                message: String::new()
            })
        );
        assert!(outcome.may_retry_same_key());
        assert_eq!(fake.sent.borrow()[0].method, Method::Post);
    }

    #[test]
    fn spawning_reads_options_and_posts_the_request() {
        let fake = FakeTransport::answering(200, r#"{"projects":["web"],"kinds":["claude"]}"#);
        let client = ApiClient::new(&fake, MemoryToken::new(BearerToken::new("t").unwrap()));
        let options = now(client.spawn_options()).unwrap();
        assert_eq!(options.projects[0].as_str(), "web");
        fake.responses.borrow_mut().push(Ok(HttpResponse::new(
            200,
            r#"{"status":"spawned","agentId":"w2:p1"}"#,
        )));
        let request: SpawnRequest = serde_json::from_str(
            r#"{"idempotencyKey":"9ca09978-ef89-46fd-9a85-118b84bee7b4","project":"web",
                "kind":"claude","prompt":"hi","createdAt":0}"#,
        )
        .unwrap();
        assert!(matches!(
            now(client.spawn(&request)),
            Ok(SpawnResult::Spawned { .. })
        ));
        let sent = fake.sent.borrow();
        assert_eq!(sent[0].path, "/v1/projects");
        assert_eq!(sent[1].method, Method::Post);
        assert_eq!(sent[1].path, "/v1/agents");
        assert!(
            sent[1]
                .body
                .as_deref()
                .unwrap()
                .contains(r#""project":"web""#)
        );
    }
}
