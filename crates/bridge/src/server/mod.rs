//! The HTTP side: the `/v1` API, the event stream and the PWA's files.
//!
//! Request flow, outermost first: request logging → [`ClientAddr`]
//! resolution → security headers → body cap → either the `/v1` router
//! (`Cache-Control: no-store`, timeout, auth, the scope check, then the
//! handler) or the static files (timeout, then the handler).
//!
//! Scope: a car-scoped session ([`protocol::Scope::Car`], Android Auto)
//! reaches only [`auth::CAR_ROUTES`]: `GET /v1/agents`,
//! `GET /v1/agents/:id/pending`, `GET /v1/agents/:id/tldr`,
//! `POST /v1/agents/:id/car-reply` and `POST /v1/actions` (judged as
//! `origin: "car"`). The auth middleware answers it 403 everywhere else,
//! deny by default, and the stream closes on it as on a bad token.
//!
//! Timeouts: [`ServerSettings::request_timeout`] for every route but
//! `PATCH /v1/audio/:id`, which gets
//! [`AudioSettings::chunk_timeout`](crate::audio::AudioSettings::chunk_timeout)
//! so a full chunk fits through a slow uplink, and `POST /v1/attachments`,
//! which gets
//! [`AttachmentSettings::upload_timeout`](crate::attachments::AttachmentSettings::upload_timeout).
//!
//! Body limits: the outer cap refuses anything over the largest body any
//! route takes (a photo, or an audio chunk) before auth. Each route's own limit is
//! axum's [`DefaultBodyLimit`], applied by its body extractor:
//! [`ServerSettings::body_limit`] everywhere, raised to
//! [`AudioSettings::max_chunk_bytes`](crate::audio::AudioSettings::max_chunk_bytes)
//! on `PATCH /v1/audio/:id` and to
//! [`AttachmentSettings::max_upload_bytes`](crate::attachments::AttachmentSettings::max_upload_bytes)
//! on `POST /v1/attachments`.

mod agents;
mod audio;
pub mod auth;
mod client_addr;
mod detach;
mod headers;
mod log;
mod push;
pub(crate) mod query;
mod static_files;
mod status;
mod stream;
mod suggestions;
mod tldr;
mod token;
mod version;
mod views;

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::connect_info::Connected;
use axum::extract::{DefaultBodyLimit, FromRef};
use axum::handler::Handler as _;
use axum::http::StatusCode;
use axum::middleware::{from_fn, from_fn_with_state, map_response};
use axum::routing::{get, patch, post};
use axum::serve::{IncomingStream, Listener};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

pub use auth::{AuthFuture, AuthRejection, AuthRequest, Authenticator, Principal, Revocation};
pub use client_addr::ClientAddr;
pub(crate) use detach::detached;
pub use headers::CONTENT_SECURITY_POLICY;
pub use stream::StreamSettings;
pub use token::{
    ApiToken, InvalidToken, MAX_TOKEN_CHARS, MIN_TOKEN_CHARS, STATIC_DEVICE, StaticToken, bearer,
};
pub(crate) use views::{current_agent, screen_tail};

use self::static_files::StaticFiles;
use self::stream::OpenStreams;
use crate::actions::Actions;
use crate::attachments::Attachments;
use crate::audio::Audio;
use crate::commands::Commands;
use crate::control::AgentControl;
use crate::pending::Pendings;
use crate::push::Push;
use crate::registry::Hub;
use crate::runbooks::Runbooks;
use crate::shutdown::Shutdown;
use crate::spawn::Spawner;
use crate::threads::Threads;
use crate::tldr::Tldrs;

/// HTTP-level settings.
#[derive(Debug, Clone)]
pub struct ServerSettings {
    /// The PWA's build output.
    pub static_dir: PathBuf,
    /// The edge proxy, the only peer allowed to set `X-Forwarded-For`.
    pub trusted_proxy: Option<IpAddr>,
    /// Longest time to produce a response (WebSocket sessions excepted).
    pub request_timeout: Duration,
    /// Largest request body accepted, in bytes (audio chunks have their
    /// own limit, [`AudioSettings::max_chunk_bytes`](crate::audio::AudioSettings::max_chunk_bytes)).
    pub body_limit: usize,
    /// Stream keepalive and backpressure.
    pub stream: StreamSettings,
}

impl ServerSettings {
    /// Defaults for everything but the static dir: no trusted proxy, a 30 s
    /// request timeout and a 64 KiB body limit (prompts are at most 16 KiB).
    #[must_use]
    pub fn new(static_dir: impl Into<PathBuf>) -> Self {
        Self {
            static_dir: static_dir.into(),
            trusted_proxy: None,
            request_timeout: Duration::from_secs(30),
            body_limit: 64 * 1024,
            stream: StreamSettings::default(),
        }
    }
}

/// Everything a handler can reach.
#[derive(Clone)]
pub struct AppState {
    pub(crate) hub: Arc<Hub>,
    auth: Arc<dyn Authenticator>,
    pub(crate) control: Arc<dyn AgentControl>,
    pub(crate) threads: Arc<Threads>,
    pub(crate) actions: Arc<Actions>,
    pub(crate) spawner: Arc<Spawner>,
    pub(crate) runbooks: Arc<Runbooks>,
    pub(crate) commands: Arc<Commands>,
    pub(crate) pending: Arc<Pendings>,
    pub(crate) push: Option<Arc<Push>>,
    pub(crate) audio: Arc<Audio>,
    pub(crate) attachments: Arc<Attachments>,
    pub(crate) tldrs: Arc<Tldrs>,
    settings: Arc<ServerSettings>,
    pub(crate) shutdown: Shutdown,
    files: StaticFiles,
    streams: OpenStreams,
}

impl fmt::Debug for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppState")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// The services behind the per-agent endpoints.
#[derive(Clone)]
pub struct Services {
    /// herdr, per pane.
    pub control: Arc<dyn AgentControl>,
    /// Transcript readers.
    pub threads: Arc<Threads>,
    /// The idempotency ledger and audit log.
    pub actions: Arc<Actions>,
    /// New agents from the phone.
    pub spawner: Arc<Spawner>,
    /// Preconfigured operations from the phone.
    pub runbooks: Arc<Runbooks>,
    /// Terminal mode: commands from the agent's latest message.
    pub commands: Arc<Commands>,
    /// Screen-parsed pending inputs.
    pub pending: Arc<Pendings>,
    /// Web Push subscriptions; `None` when push is off.
    pub push: Option<Arc<Push>>,
    /// Dictation uploads and transcripts.
    pub audio: Arc<Audio>,
    /// Photo attachments.
    pub attachments: Arc<Attachments>,
    /// TL;DRs of long turns.
    pub tldrs: Arc<Tldrs>,
}

impl fmt::Debug for Services {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Services").finish_non_exhaustive()
    }
}

impl AppState {
    /// Bundles the shared pieces. `shutdown` closes open streams.
    #[must_use]
    pub fn new(
        hub: Arc<Hub>,
        auth: Arc<dyn Authenticator>,
        services: Services,
        settings: ServerSettings,
        shutdown: Shutdown,
    ) -> Self {
        Self {
            hub,
            auth,
            control: services.control,
            threads: services.threads,
            actions: services.actions,
            spawner: services.spawner,
            runbooks: services.runbooks,
            commands: services.commands,
            pending: services.pending,
            push: services.push,
            audio: services.audio,
            attachments: services.attachments,
            tldrs: services.tldrs,
            files: StaticFiles::new(&settings.static_dir),
            settings: Arc::new(settings),
            shutdown,
            streams: OpenStreams::default(),
        }
    }
}

impl FromRef<AppState> for Arc<Hub> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.hub)
    }
}

impl FromRef<AppState> for Arc<dyn Authenticator> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.auth)
    }
}

/// The bridge's routes and middleware. [`serve`] serves them (they need the
/// peer address as `ConnectInfo<SocketAddr>`).
fn router(state: AppState) -> Router {
    let settings = Arc::clone(&state.settings);
    let audio = state.audio.settings();
    let chunk_limit = audio.max_chunk_bytes;
    let photo_settings = state.attachments.settings();
    let photo_limit = photo_settings.max_upload_bytes;
    let photo_timeout = photo_settings.upload_timeout;
    let timeout = |after| TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, after);
    let require_auth = from_fn_with_state(state.clone(), auth::require_auth);
    // `require_auth` also refuses car-scoped sessions (Android Auto) on
    // every route but `auth::CAR_ROUTES`: deny by default, so a route added
    // here is the app's until it is listed there.
    // The stream authenticates in its first message (browsers cannot set
    // headers on a WebSocket), so it sits outside the header check, as do
    // the authenticator's own (pairing) routes and the build version.
    let public_auth = Arc::clone(&state.auth).public_routes();
    let api = Router::new()
        .route("/agents", get(agents::list))
        .route("/agents/{id}/pending", get(views::pending))
        .route("/agents/{id}/tldr", get(tldr::tldr))
        .route("/agents/{id}/car-reply", post(crate::actions::car::post))
        .route("/actions", post(crate::actions::post))
        .route("/agents/{id}/thread", get(views::thread))
        .route("/agents/{id}/screen", get(views::screen))
        .route("/agents/{id}/suggestions", get(suggestions::suggestions))
        .route("/agents/{id}/status", post(status::refresh))
        .route("/agents/{id}/commands", get(crate::commands::list))
        .route("/agents/{id}/commands/run", post(crate::commands::run))
        .route("/terminals/{pane}/screen", get(crate::commands::screen))
        .route("/terminals/{pane}/keys", post(crate::commands::keys))
        .route("/projects", get(crate::spawn::options))
        .route("/runbooks", get(crate::runbooks::list))
        .route("/runbooks/{name}/run", post(crate::runbooks::run))
        .route("/push/key", get(push::key))
        .route("/push/native", post(crate::push::native::register))
        .route(
            "/push/subscriptions",
            get(push::list)
                .post(push::subscribe)
                .delete(push::unsubscribe),
        )
        .route("/audio", post(audio::create))
        .route("/audio/{id}", get(audio::status))
        .route("/audio/{id}/transcript", get(audio::transcript))
        .fallback(api_not_found)
        .layer(require_auth.clone())
        .route("/stream", get(stream::upgrade))
        .route("/version", get(version::version))
        .nest_service("/auth", public_auth)
        .layer(timeout(settings.request_timeout));
    // Chunks: their own body limit and timeout (merged into the same path).
    let chunks = Router::new()
        .route(
            "/audio/{id}",
            patch(audio::append).layer(DefaultBodyLimit::max(chunk_limit)),
        )
        .layer(require_auth.clone())
        .layer(timeout(audio.chunk_timeout));
    // Photos: their own body limit and timeout.
    let photos = Router::new()
        .route(
            "/attachments",
            post(crate::attachments::post).layer(DefaultBodyLimit::max(photo_limit)),
        )
        .layer(require_auth.clone())
        .layer(timeout(photo_timeout));
    // Starting an agent waits on herdr (shell, startup, first prompt): its
    // own timeout, past the handler's own answer deadline (merged into
    // `/agents`).
    // The built-in notification stream stays open for a session's
    // lifetime: no request timeout (it ends itself, see `push::native`).
    let native = Router::new()
        .route("/push/native/{id}/stream", get(crate::push::native::stream))
        .layer(require_auth.clone());
    let spawns = Router::new()
        .route("/agents", post(crate::spawn::post))
        .layer(require_auth)
        .layer(timeout(state.spawner.settings().route_timeout()));
    Router::new()
        .nest(
            "/v1",
            api.merge(native)
                .merge(chunks)
                .merge(photos)
                .merge(spawns)
                .layer(map_response(headers::no_store)),
        )
        .fallback(static_files::serve.layer(timeout(settings.request_timeout)))
        .layer(DefaultBodyLimit::max(settings.body_limit))
        .layer(RequestBodyLimitLayer::new(
            settings.body_limit.max(chunk_limit).max(photo_limit),
        ))
        .layer(map_response(headers::security))
        .layer(from_fn_with_state(
            settings.trusted_proxy,
            client_addr::attach,
        ))
        .layer(from_fn(log::requests))
        .with_state(state)
}

async fn api_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// Extra time, beyond one stream write timeout, that [`serve`] waits for
/// open streams to send their close frames.
const STREAM_DRAIN_SLACK: Duration = Duration::from_secs(1);

/// Serves the bridge on `listener` until `shutdown`, then lets in-flight
/// requests finish and waits (boundedly) for every event stream to send its
/// close frame. axum does not track upgraded connections, so without this
/// the runtime could be torn down mid-close.
pub async fn serve<L>(listener: L, state: AppState, shutdown: Shutdown) -> io::Result<()>
where
    L: Listener,
    L::Addr: fmt::Debug,
    for<'a> SocketAddr: Connected<IncomingStream<'a, L>>,
{
    let streams = state.streams.clone();
    let drain = state.settings.stream.write_timeout + STREAM_DRAIN_SLACK;
    let served = axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown.wait_owned())
    .await;
    if tokio::time::timeout(drain, streams.all_closed())
        .await
        .is_err()
    {
        tracing::warn!("event streams did not close in time");
    }
    served
}
