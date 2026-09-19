//! The HTTP and WebSocket API. Its shape is documented in `ferromesh_model::wire`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferromesh_model::{
    AddChannel, ChannelInfo, DEFAULT_HEALTH_HOURS, DEFAULT_HISTORY_LIMIT, DirectMessageInfo,
    DirectQuery, Event, Filter, Frame, GuessChannels, GuessReport, Health, HealthQuery,
    HistoryQuery, Kind, MAX_DIRECT, MAX_HEALTH_HOURS, MAX_HISTORY_LIMIT, MAX_NODES, MAX_OUTBOX,
    NodeInfo, NodesQuery, ObserverHealth, OutboxQuery, PacketDetail, PinRequest, RadioContact,
    SendRequest, SentMessageInfo, StreamQuery, UnknownChannel,
};
use ferromesh_store::{Micros, Order, Page, Reader, SendTarget};
use jiff::Timestamp;
use meshcore_proto::NodeRole;
use meshcore_proto::companion::Contact;
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{debug, warn};

use crate::companion::contacts::contact_from;
use crate::companion::{Accepted, Request, SendError};
use crate::pipeline::{self, AddOutcome};
use crate::writer::Job;

/// Rows per database round trip while replaying history into a stream.
const REPLAY_PAGE: usize = 500;
/// The most history `last=` may request.
const MAX_LAST: usize = 10_000;
const PING_EVERY: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct AppState {
    db_path: Arc<PathBuf>,
    events: broadcast::Sender<Arc<Event>>,
    shutdown: watch::Receiver<bool>,
    /// Where changes go; without it the API is read-only.
    writer: Option<mpsc::Sender<Job>>,
    /// The bearer token changes require; without it changes are refused.
    token: Option<Arc<str>>,
    /// The companion radio, for sending; without it sending is refused.
    companion: Option<std::sync::mpsc::Sender<Request>>,
}

impl AppState {
    /// `events` carries every newly stored row; flipping `shutdown` to true
    /// ends open streams and stops the server.
    pub fn new(
        db_path: PathBuf,
        events: broadcast::Sender<Arc<Event>>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            db_path: Arc::new(db_path),
            events,
            shutdown,
            writer: None,
            token: None,
            companion: None,
        }
    }

    /// Accepts changes, handed to the writer thread, from clients presenting
    /// `token`. With no token, changes stay refused.
    pub fn with_writer(mut self, writer: mpsc::Sender<Job>, token: Option<String>) -> Self {
        self.writer = Some(writer);
        self.token = token.map(Arc::from);
        self
    }

    /// Sends messages through a companion radio, for clients with the token.
    pub fn with_companion(mut self, requests: std::sync::mpsc::Sender<Request>) -> Self {
        self.companion = Some(requests);
        self
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/stream", get(stream))
        .route("/api/v1/channels", get(list_channels).post(add_channel))
        .route("/api/v1/channels/unknown", get(unknown_channels))
        .route("/api/v1/channels/guess", post(guess_channels))
        .route("/api/v1/nodes", get(list_nodes))
        .route("/api/v1/packets/{hash}", get(packet_detail))
        .route("/api/v1/direct", get(list_direct))
        .route("/api/v1/send", post(send_message))
        .route("/api/v1/outbox", get(list_outbox))
        .route("/api/v1/contacts", get(list_contacts).post(pin_contact))
        .route("/api/v1/observers", get(observer_health))
        .route("/api/v1/{kind}", get(history))
        .with_state(state)
}

/// Serves until the state's shutdown flag is set.
pub async fn serve(listener: TcpListener, state: AppState) -> std::io::Result<()> {
    let mut shutdown = state.shutdown.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move { stopped(&mut shutdown).await })
        .await
}

/// Resolves once shutdown is requested. The watch guard is released before
/// returning, so callers can keep awaiting in a `Send` future.
async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

async fn health() -> Json<Health> {
    Json(Health { version: env!("CARGO_PKG_VERSION").to_owned() })
}

async fn history(
    State(state): State<AppState>,
    Path(kind): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<Event>>, ApiError> {
    let kind: Kind = kind.parse().map_err(|e| ApiError::new(StatusCode::NOT_FOUND, e))?;
    let filter = parse_filter(query.filter.as_deref(), kind)?;
    let page = Page {
        after: query.after,
        before: query.before,
        since: query.since.map(|at| at.as_microsecond()),
        until: query.until.map(|at| at.as_microsecond()),
        limit: query.limit.unwrap_or(DEFAULT_HISTORY_LIMIT).clamp(1, MAX_HISTORY_LIMIT),
        order: Order::Descending,
    };
    let events = read(&state, move |reader| reader.history(kind, &filter, &page)).await?;
    Ok(Json(events))
}

async fn list_nodes(
    State(state): State<AppState>,
    Query(query): Query<NodesQuery>,
) -> Result<Json<Vec<NodeInfo>>, ApiError> {
    let limit = query.limit.unwrap_or(MAX_NODES).clamp(1, MAX_NODES);
    Ok(Json(read(&state, move |reader| reader.nodes(limit)).await?))
}

async fn list_direct(
    State(state): State<AppState>,
    Query(query): Query<DirectQuery>,
) -> Result<Json<Vec<DirectMessageInfo>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_HISTORY_LIMIT).clamp(1, MAX_DIRECT);
    Ok(Json(read(&state, move |reader| reader.direct_messages(limit)).await?))
}

async fn packet_detail(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<PacketDetail>, ApiError> {
    let bytes = hex::decode(&hash).ok().filter(|bytes| bytes.len() == 8).ok_or_else(|| {
        ApiError::new(StatusCode::BAD_REQUEST, format!("{hash:?} is not a 16-digit packet hash"))
    })?;
    read(&state, move |reader| reader.packet_detail(&bytes))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no packet has that hash"))
}

async fn list_channels(State(state): State<AppState>) -> Result<Json<Vec<ChannelInfo>>, ApiError> {
    Ok(Json(read(&state, |reader| reader.channel_infos()).await?))
}

async fn unknown_channels(
    State(state): State<AppState>,
) -> Result<Json<Vec<UnknownChannel>>, ApiError> {
    Ok(Json(read(&state, |reader| reader.unknown_channels()).await?))
}

async fn guess_channels(
    State(state): State<AppState>,
    Json(request): Json<GuessChannels>,
) -> Result<Json<GuessReport>, ApiError> {
    Ok(Json(read(&state, move |reader| reader.guess_channels(&request)).await?))
}

async fn add_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AddChannel>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers)?;
    let writer_gone =
        || ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "the server can't make changes");
    let writer = state.writer.clone().ok_or_else(writer_gone)?;
    let name = request.name.trim().to_owned();
    let (key, kind) = pipeline::channel_key(&name, request.key.as_deref())
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("{e:#}")))?;

    let (reply, outcome) = oneshot::channel();
    writer.send(Job::AddChannel { name, key, kind, reply }).await.map_err(|_| writer_gone())?;
    match outcome.await.map_err(|_| writer_gone())? {
        Ok(AddOutcome::Added(added)) => Ok((StatusCode::CREATED, Json(added)).into_response()),
        Ok(AddOutcome::Exists(existing)) => Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("that key already belongs to channel {}", existing.name),
        )),
        Err(error) => Err(ApiError::internal(error)),
    }
}

/// How long to wait for the radio to take a message. Giving a channel a slot
/// first means reading every slot, which takes a few seconds.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

async fn send_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SendRequest>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers)?;
    let unavailable = |message: &str| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message);
    let companion = state.companion.clone().ok_or_else(|| {
        unavailable("this server has no companion radio: add a [companion] section to its config")
    })?;
    let writer =
        state.writer.clone().ok_or_else(|| unavailable("the server can't make changes"))?;
    let to = request.to.trim().to_owned();
    let lookup = to.clone();
    let target = read(&state, move |reader| reader.send_target(&lookup)).await?;

    let (reply, answer) = oneshot::channel();
    let text = request.text;
    let request = match target {
        SendTarget::Channel { name, secret } => {
            let secret = secret.as_slice().try_into().map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "{name} has a 32-byte key, and companion radios only take 16-byte ones"
                    ),
                )
            })?;
            Request::Channel { name, secret, text, reply }
        }
        SendTarget::Node(node) => Request::Direct { contact: contact_from(&node), text, reply },
        SendTarget::Ambiguous(matches) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("{to:?} matches {}; give more of the key", matches.join(", ")),
            ));
        }
        SendTarget::Unknown => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                format!(
                    "no channel or node is called {to:?}: add channels first, and a node must \
                     have been heard advertising"
                ),
            ));
        }
    };
    companion.send(request).map_err(|_| unavailable("the companion radio has stopped"))?;
    let answer = tokio::time::timeout(SEND_TIMEOUT, answer)
        .await
        .map_err(|_| {
            ApiError::new(StatusCode::GATEWAY_TIMEOUT, "the companion radio didn't answer")
        })?
        .map_err(|_| unavailable("the companion radio has stopped"))?;
    // For anything that reached the radio, sent or refused, the radio thread
    // queued an outbox record before answering. Once the writer has caught
    // up, the outbox shows it.
    if matches!(answer, Ok(_) | Err(SendError::Failed(_))) {
        let (synced, done) = oneshot::channel();
        writer
            .send(Job::Sync(synced))
            .await
            .map_err(|_| unavailable("the server can't make changes"))?;
        done.await.map_err(|_| unavailable("the server can't make changes"))?;
    }
    let sender_timestamp = match answer {
        Ok(Accepted { sender_timestamp }) => sender_timestamp,
        Err(SendError::Invalid(problem)) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, problem));
        }
        Err(SendError::Unavailable(problem)) => return Err(unavailable(&problem)),
        Err(SendError::Failed(problem)) => {
            return Err(ApiError::new(StatusCode::BAD_GATEWAY, problem));
        }
    };
    let now = Timestamp::now().as_microsecond();
    let outbox = read(&state, move |reader| reader.outbox(100, now)).await?;
    let entry = outbox
        .into_iter()
        .find(|entry| entry.sender_timestamp.as_second() == i64::from(sender_timestamp))
        .ok_or_else(|| {
            ApiError::internal(anyhow::anyhow!("the sent message is missing from the outbox"))
        })?;
    Ok((StatusCode::CREATED, Json(entry)).into_response())
}

async fn list_contacts(State(state): State<AppState>) -> Result<Json<Vec<RadioContact>>, ApiError> {
    let mut contacts: Vec<RadioContact> = ask_radio(&state, |reply| Request::Contacts { reply })
        .await?
        .iter()
        .map(radio_contact)
        .collect();
    contacts.sort_by(|a, b| b.favourite.cmp(&a.favourite).then_with(|| a.name.cmp(&b.name)));
    Ok(Json(contacts))
}

async fn pin_contact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PinRequest>,
) -> Result<Json<RadioContact>, ApiError> {
    authorize(&state, &headers)?;
    let to = request.to.trim().to_owned();
    let lookup = to.clone();
    let node = match read(&state, move |reader| reader.send_target(&lookup)).await? {
        SendTarget::Node(node) => node,
        SendTarget::Channel { name, .. } => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("{name} is a channel, not a node"),
            ));
        }
        SendTarget::Ambiguous(matches) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("{to:?} matches {}; give more of the key", matches.join(", ")),
            ));
        }
        SendTarget::Unknown => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                format!("no node called {to:?} has been heard advertising"),
            ));
        }
    };
    let contact = contact_from(&node);
    let pinned = request.pinned;
    let held = ask_radio(&state, |reply| Request::Pin { contact, pinned, reply }).await?;
    Ok(Json(radio_contact(&held)))
}

/// Hands a request to the companion radio's thread and waits for its answer.
async fn ask_radio<T>(
    state: &AppState,
    request: impl FnOnce(oneshot::Sender<Result<T, SendError>>) -> Request,
) -> Result<T, ApiError> {
    let unavailable = |message: &str| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message);
    let companion = state.companion.clone().ok_or_else(|| {
        unavailable("this server has no companion radio: add a [companion] section to its config")
    })?;
    let (reply, answer) = oneshot::channel();
    companion.send(request(reply)).map_err(|_| unavailable("the companion radio has stopped"))?;
    match tokio::time::timeout(SEND_TIMEOUT, answer).await {
        Err(_) => {
            Err(ApiError::new(StatusCode::GATEWAY_TIMEOUT, "the companion radio didn't answer"))
        }
        Ok(Err(_)) => Err(unavailable("the companion radio has stopped")),
        Ok(Ok(Ok(value))) => Ok(value),
        Ok(Ok(Err(SendError::Invalid(problem)))) => {
            Err(ApiError::new(StatusCode::BAD_REQUEST, problem))
        }
        Ok(Ok(Err(SendError::Unavailable(problem)))) => Err(unavailable(&problem)),
        Ok(Ok(Err(SendError::Failed(problem)))) => {
            Err(ApiError::new(StatusCode::BAD_GATEWAY, problem))
        }
    }
}

fn radio_contact(contact: &Contact) -> RadioContact {
    RadioContact {
        pubkey: hex::encode(contact.pubkey),
        name: contact.name.clone(),
        kind: NodeRole::from_flags(contact.kind).name().to_owned(),
        favourite: contact.is_favourite(),
        last_advert: (contact.last_advert > 0)
            .then(|| Timestamp::from_second(i64::from(contact.last_advert)).ok())
            .flatten(),
        // The length byte's low six bits count the hops.
        route_hops: contact.out_path_len.map(|len| len & 0x3F),
    }
}

async fn observer_health(
    State(state): State<AppState>,
    Query(query): Query<HealthQuery>,
) -> Result<Json<Vec<ObserverHealth>>, ApiError> {
    let hours = query.hours.unwrap_or(DEFAULT_HEALTH_HOURS).clamp(1, MAX_HEALTH_HOURS);
    let now = Timestamp::now().as_microsecond();
    Ok(Json(read(&state, move |reader| reader.observer_health(now, hours)).await?))
}

async fn list_outbox(
    State(state): State<AppState>,
    Query(query): Query<OutboxQuery>,
) -> Result<Json<Vec<SentMessageInfo>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_HISTORY_LIMIT).clamp(1, MAX_OUTBOX);
    let now = Timestamp::now().as_microsecond();
    Ok(Json(read(&state, move |reader| reader.outbox(limit, now)).await?))
}

/// Changes need the configured bearer token; with none configured they're off.
fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = &state.token else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "changes are disabled on this server: set api.token in its config",
        ));
    };
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match presented {
        Some(token) if same_secret(token.as_bytes(), expected.as_bytes()) => Ok(()),
        Some(_) => Err(ApiError::new(StatusCode::UNAUTHORIZED, "wrong token")),
        None => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "this change needs a token: pass --token or set FERROMESH_TOKEN",
        )),
    }
}

/// Compares every byte, so response timing doesn't reveal how much of a
/// guessed token was right.
fn same_secret(presented: &[u8], expected: &[u8]) -> bool {
    presented.len() == expected.len()
        && presented.iter().zip(expected).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
}

async fn stream(
    upgrade: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(query): Query<StreamQuery>,
) -> Result<Response, ApiError> {
    let filter = Arc::new(parse_filter(query.filter.as_deref(), query.kind)?);
    let start = match (query.after, query.since, query.last) {
        (Some(after), _, _) => Start::After(after),
        (None, Some(since), _) => Start::Since(since.as_microsecond()),
        (None, None, Some(last)) => Start::Last(last.min(MAX_LAST)),
        (None, None, None) => Start::Live,
    };
    debug!(kind = %query.kind, filter = ?query.filter, ?start, "stream opened");
    Ok(upgrade.on_upgrade(move |socket| async move {
        let mut stream = Stream { socket, state, kind: query.kind, filter, cursor: 0 };
        if let Err(error) = stream.run(start).await {
            debug!("stream ended: {error:#}");
            let _ = stream.send(&Frame::Error { message: format!("{error:#}") }).await;
        }
    }))
}

fn parse_filter(text: Option<&str>, kind: Kind) -> Result<Filter, ApiError> {
    let bad_request = |e: ferromesh_model::FilterError| ApiError::new(StatusCode::BAD_REQUEST, e);
    let filter: Filter = text.unwrap_or_default().parse().map_err(bad_request)?;
    filter.validate(kind).map_err(bad_request)?;
    Ok(filter)
}

/// An HTTP handler's query, on its own read-only connection.
async fn read<T: Send + 'static>(
    state: &AppState,
    query: impl FnOnce(&Reader) -> ferromesh_store::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    blocking_read(Arc::clone(&state.db_path), query).await.map_err(ApiError::internal)
}

/// Runs a query on its own read-only connection, off the async runtime.
async fn blocking_read<T: Send + 'static>(
    db_path: Arc<PathBuf>,
    query: impl FnOnce(&Reader) -> ferromesh_store::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let result = tokio::task::spawn_blocking(move || query(&Reader::open(&*db_path)?)).await?;
    Ok(result?)
}

#[derive(Debug, Clone, Copy)]
enum Start {
    Live,
    Last(usize),
    Since(Micros),
    After(i64),
}

/// One subscriber: history, then live events.
///
/// It subscribes to the broadcast before reading the newest id, so every row
/// is either in the replayed history (id at or below that mark) or arrives
/// live afterwards. `cursor` drops live events the history already covered.
struct Stream {
    socket: WebSocket,
    state: AppState,
    kind: Kind,
    filter: Arc<Filter>,
    /// The newest id already sent or passed over.
    cursor: i64,
}

impl Stream {
    async fn run(&mut self, start: Start) -> anyhow::Result<()> {
        let mut live = self.state.events.subscribe();
        let high = self.max_id().await?;
        match start {
            Start::Live => {}
            Start::Last(count) => {
                let page = Page { before: Some(high + 1), limit: count, ..Page::default() };
                let mut events = self.history(page).await?;
                events.reverse();
                self.send_events(events).await?;
            }
            Start::Since(since) => {
                self.replay(Page { since: Some(since), ..Page::default() }, high).await?;
            }
            Start::After(after) => {
                self.replay(Page { after: Some(after), ..Page::default() }, high).await?;
            }
        }
        self.cursor = self.cursor.max(high);
        self.send(&Frame::CaughtUp { last_id: self.cursor }).await?;

        let mut shutdown = self.state.shutdown.clone();
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            tokio::select! {
                received = live.recv() => match received {
                    Ok(event) => {
                        if event.kind() == self.kind && event.id() > self.cursor {
                            self.cursor = event.id();
                            if self.filter.matches(&event) {
                                self.send(&Frame::Event { event: (*event).clone() }).await?;
                            }
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        // Too slow for the live feed: resubscribe, then fill
                        // the gap from the database.
                        debug!(skipped, "stream lagged; catching up from the database");
                        live = self.state.events.subscribe();
                        let high = self.max_id().await?;
                        let after = self.cursor;
                        self.replay(Page { after: Some(after), ..Page::default() }, high).await?;
                        self.cursor = self.cursor.max(high);
                    }
                    Err(RecvError::Closed) => return Ok(()),
                },
                incoming = self.socket.recv() => match incoming {
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Err(error)) => return Err(error.into()),
                    Some(Ok(_)) => {}
                },
                _ = ping.tick() => self.socket.send(Message::Ping(Default::default())).await?,
                () = stopped(&mut shutdown) => {
                    let _ = self.socket.send(Message::Close(None)).await;
                    return Ok(());
                }
            }
        }
    }

    /// Sends matching history from `page`'s lower bound up to `high`, oldest
    /// first, a page at a time.
    async fn replay(&mut self, mut page: Page, high: i64) -> anyhow::Result<()> {
        page.before = Some(high + 1);
        page.limit = REPLAY_PAGE;
        page.order = Order::Ascending;
        loop {
            let events = self.history(page.clone()).await?;
            let more = events.len() == REPLAY_PAGE;
            if let Some(last) = events.last() {
                page.after = Some(last.id());
            }
            self.send_events(events).await?;
            if !more {
                return Ok(());
            }
        }
    }

    // The queries capture copies rather than `&self`: the socket isn't `Sync`,
    // so a borrowed `Stream` can't be held across an await in a `Send` task.

    fn max_id(&self) -> impl Future<Output = anyhow::Result<i64>> + Send + use<> {
        let (db_path, kind) = (Arc::clone(&self.state.db_path), self.kind);
        blocking_read(db_path, move |reader| reader.max_id(kind))
    }

    fn history(
        &self,
        page: Page,
    ) -> impl Future<Output = anyhow::Result<Vec<Event>>> + Send + use<> {
        let (db_path, kind, filter) =
            (Arc::clone(&self.state.db_path), self.kind, Arc::clone(&self.filter));
        blocking_read(db_path, move |reader| reader.history(kind, &filter, &page))
    }

    async fn send_events(&mut self, events: Vec<Event>) -> anyhow::Result<()> {
        for event in events {
            self.cursor = self.cursor.max(event.id());
            self.send(&Frame::Event { event }).await?;
        }
        Ok(())
    }

    async fn send(&mut self, frame: &Frame) -> anyhow::Result<()> {
        let text = serde_json::to_string(frame)?;
        self.socket.send(Message::Text(text.into())).await?;
        Ok(())
    }
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl ToString) -> Self {
        Self { status, message: message.to_string() }
    }

    fn internal(error: anyhow::Error) -> Self {
        warn!("API request failed: {error:#}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.message }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::same_secret;

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret(b"open-sesame", b"open-sesame"));
        assert!(!same_secret(b"open-sesamE", b"open-sesame"));
        assert!(!same_secret(b"open", b"open-sesame"));
    }
}
