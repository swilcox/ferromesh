//! The HTTP and WebSocket API. Its shape is documented in `ferromesh_model::wire`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ferromesh_model::{
    DEFAULT_HISTORY_LIMIT, Event, Filter, Frame, Health, HistoryQuery, Kind, MAX_HISTORY_LIMIT,
    StreamQuery,
};
use ferromesh_store::{Micros, Order, Page, Reader};
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use tracing::{debug, warn};

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
}

impl AppState {
    /// `events` carries every newly stored row; flipping `shutdown` to true
    /// ends open streams and stops the server.
    pub fn new(
        db_path: PathBuf,
        events: broadcast::Sender<Arc<Event>>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self { db_path: Arc::new(db_path), events, shutdown }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/stream", get(stream))
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
    let events =
        read(Arc::clone(&state.db_path), move |reader| reader.history(kind, &filter, &page))
            .await
            .map_err(ApiError::internal)?;
    Ok(Json(events))
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

/// Runs a query on its own read-only connection, off the async runtime.
async fn read<T: Send + 'static>(
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
        read(db_path, move |reader| reader.max_id(kind))
    }

    fn history(
        &self,
        page: Page,
    ) -> impl Future<Output = anyhow::Result<Vec<Event>>> + Send + use<> {
        let (db_path, kind, filter) =
            (Arc::clone(&self.state.db_path), self.kind, Arc::clone(&self.filter));
        read(db_path, move |reader| reader.history(kind, &filter, &page))
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
