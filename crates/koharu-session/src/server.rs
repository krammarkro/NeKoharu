//! The session host: a local HTTP endpoint serving project state, blobs, and
//! the commit stream. Every guest edit is applied by the owning application
//! before it is broadcast, so only the host ever writes project files.
//!
//! The endpoint and its link outlive any one shared project: sharing attaches
//! a project and stopping detaches it, so a guest that lost the host finds it
//! again at the same link once the project is shared again.

use std::{
    collections::HashMap,
    sync::{
        Arc, PoisonError, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use axum::{
    body::Body,
    extract::{
        Path, RawQuery, State as AxumState,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::protocol::{self, ClientMessage, HOST_PEER, Member, Presence, ServerMessage};

/// The application's side of a hosted session: the project the host owns.
#[async_trait]
pub trait HostSource: Send + Sync + 'static {
    /// The project's display name, shown to guests before they join.
    fn name(&self) -> String;
    /// The current encoded project state.
    async fn state(&self) -> Result<Vec<u8>>;
    /// One blob's bytes.
    async fn blob(&self, id: String) -> Result<Option<Bytes>>;
    /// Applies a guest's patch to the host's project and returns the revision
    /// it earned. An error tells that guest to reload the project state.
    async fn apply(&self, patch: Vec<u8>) -> std::result::Result<u64, String>;
}

struct Shared {
    /// The project being shared. While there is none every request is refused
    /// and guests keep retrying.
    source: RwLock<Option<Arc<dyn HostSource>>>,
    token: String,
    peers: Mutex<HashMap<u64, mpsc::UnboundedSender<ServerMessage>>>,
    /// Peer ids are never reused, so a departing guest cannot remove a newer
    /// one and a returning guest reads as a new arrival.
    next_peer: AtomicU64,
    /// Everyone who announced a presence under the current share, by peer id.
    members: watch::Sender<Vec<Member>>,
    revision: watch::Sender<u64>,
}

impl Shared {
    fn source(&self) -> Option<Arc<dyn HostSource>> {
        self.source
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn present(&self, peer: u64, presence: Presence) {
        self.members.send_if_modified(|members| {
            match members.binary_search_by_key(&peer, |member| member.peer) {
                Ok(index) if members[index].presence == presence => false,
                Ok(index) => {
                    members[index].presence = presence;
                    true
                }
                Err(index) => {
                    members.insert(index, Member { peer, presence });
                    true
                }
            }
        });
    }

    fn depart(&self, peer: u64) {
        self.members.send_if_modified(|members| {
            let before = members.len();
            members.retain(|member| member.peer != peer);
            members.len() != before
        });
    }
}

/// The host's own connectivity, as opposed to a guest's view of it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostStatus {
    Starting,
    Online,
    Offline,
}

/// A running session host.
pub struct Session {
    origin: String,
    token: String,
    shared: Arc<Shared>,
    shutdown: Option<oneshot::Sender<()>>,
    tunnel: Option<crate::tunnel::Tunnel>,
}

impl Session {
    /// Starts serving on a loopback port, then exposes it through a Cloudflare
    /// quick tunnel and returns the session, the link to share, and a receiver
    /// for the host's own connection status. Nothing is shared until
    /// [`Session::share`].
    pub async fn start(
        tunnel: crate::tunnel::Tunnel,
    ) -> Result<(Self, String, watch::Receiver<HostStatus>)> {
        Self::serve(Some(tunnel)).await
    }

    /// Serves on a loopback port, optionally exposing it. Without a tunnel the
    /// link points straight at that port, which is what a test on one machine
    /// needs.
    async fn serve(
        tunnel: Option<crate::tunnel::Tunnel>,
    ) -> Result<(Self, String, watch::Receiver<HostStatus>)> {
        let token = new_token();
        let (revision_tx, _) = watch::channel(0);
        let shared = Arc::new(Shared {
            source: RwLock::new(None),
            token: token.clone(),
            peers: Mutex::new(HashMap::new()),
            next_peer: AtomicU64::new(HOST_PEER + 1),
            members: watch::channel(Vec::new()).0,
            revision: revision_tx,
        });
        forward_members(&shared);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind the session port")?;
        let origin = format!("http://{}", listener.local_addr()?);
        let (shutdown, stopped) = oneshot::channel::<()>();

        let routed = shared.clone();
        let blob_path = format!("{}{{id}}", protocol::BLOB_PATH);
        let serve = tokio::spawn(async move {
            let router = axum::Router::new()
                .route(protocol::STATE_PATH, get(state))
                .route(&blob_path, get(blob))
                .route(protocol::STREAM_PATH, get(stream))
                .with_state(routed);
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await;
        });

        let mut tunnel = tunnel;
        let link = match tunnel.as_mut() {
            Some(tunnel) => {
                let url = tunnel
                    .open(origin.clone())
                    .await
                    .context("failed to open the session tunnel")?;
                crate::protocol::join_link(&url, &token)
            }
            None => crate::protocol::join_link(&origin, &token),
        };
        let (status, status_rx) = watch::channel(HostStatus::Online);

        let session = Self {
            origin,
            token,
            shared,
            shutdown: Some(shutdown),
            tunnel,
        };
        // A tunnel that dies takes the link with it; the host project itself
        // is unaffected.
        let mut tunnel_status = session.tunnel_status();
        tokio::spawn(async move {
            while tunnel_status.changed().await.is_ok() {
                if *tunnel_status.borrow_and_update() == crate::tunnel::TunnelStatus::Stopped {
                    let _ = status.send(HostStatus::Offline);
                    break;
                }
            }
            let _ = serve.await;
        });
        Ok((session, link, status_rx))
    }

    /// Shares `source` at this session's link and returns the handle the host
    /// application publishes its own commits through. Guests still connected
    /// from an earlier share are dropped, so they reload what is shared now.
    pub async fn share(&self, source: Arc<dyn HostSource>) -> Result<Broadcaster> {
        let revision = revision_of(&source.state().await?);
        self.shared.peers.lock().await.clear();
        self.shared.members.send_replace(Vec::new());
        self.shared.revision.send_replace(revision);
        *self
            .shared
            .source
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(source);
        Ok(Broadcaster {
            shared: self.shared.clone(),
        })
    }

    /// Stops sharing while keeping the link: connected guests are dropped and
    /// keep retrying until something is shared again.
    pub async fn unshare(&self) {
        self.shared
            .source
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        // Dropping the peers closes their streams, so every guest learns the
        // host is gone instead of waiting for the tunnel to time out.
        self.shared.peers.lock().await.clear();
        self.shared.members.send_replace(Vec::new());
    }

    /// Everyone in the session, the host first, as it changes.
    pub fn members(&self) -> watch::Receiver<Vec<Member>> {
        self.shared.members.subscribe()
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    fn tunnel_status(&self) -> watch::Receiver<crate::tunnel::TunnelStatus> {
        self.tunnel
            .as_ref()
            .map(crate::tunnel::Tunnel::subscribe)
            .unwrap_or_else(|| {
                let (tx, rx) = watch::channel(crate::tunnel::TunnelStatus::Stopped);
                drop(tx);
                rx
            })
    }

    /// Stops serving. Project state is untouched; only the link stops working.
    pub async fn stop(mut self) {
        self.unshare().await;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(tunnel) = self.tunnel.take() {
            tunnel.close().await;
        }
    }
}

/// Publishes a commit produced by the host itself so guests see it too.
#[derive(Clone)]
pub struct Broadcaster {
    shared: Arc<Shared>,
}

impl Broadcaster {
    pub async fn broadcast(&self, revision: u64, patch: &[u8]) {
        broadcast_commit(&self.shared, ServerMessage::commit(revision, patch)).await;
    }

    /// Replaces the host's own presence.
    pub fn present(&self, presence: Presence) {
        self.shared.present(HOST_PEER, presence);
    }

    /// Everyone in the session, the host first, as it changes.
    pub fn members(&self) -> watch::Receiver<Vec<Member>> {
        self.shared.members.subscribe()
    }
}

/// Sends the member list to every guest whenever it changes. The task only
/// holds the session weakly, so it ends with it.
fn forward_members(shared: &Arc<Shared>) {
    let mut members = shared.members.subscribe();
    let shared = Arc::downgrade(shared);
    tokio::spawn(async move {
        while members.changed().await.is_ok() {
            let message = ServerMessage::Members {
                members: members.borrow_and_update().clone(),
            };
            let Some(shared) = Weak::upgrade(&shared) else {
                return;
            };
            let peers: Vec<_> = shared.peers.lock().await.values().cloned().collect();
            for peer in peers {
                let _ = peer.send(message.clone());
            }
        }
    });
}

async fn state(
    AxumState(shared): AxumState<Arc<Shared>>,
    query: RawQuery,
    headers: header::HeaderMap,
) -> Response {
    if !authorized(&shared, query.0.as_deref(), &headers) {
        return unauthorized();
    }
    let Some(source) = shared.source() else {
        return not_sharing();
    };
    match source.state().await {
        Ok(bytes) => octets(bytes),
        Err(error) => internal(error),
    }
}

async fn blob(
    AxumState(shared): AxumState<Arc<Shared>>,
    Path(id): Path<String>,
    query: RawQuery,
    headers: header::HeaderMap,
) -> Response {
    if !authorized(&shared, query.0.as_deref(), &headers) {
        return unauthorized();
    }
    let Some(source) = shared.source() else {
        return not_sharing();
    };
    match source.blob(id).await {
        Ok(Some(bytes)) => octets(bytes),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal(error),
    }
}

async fn stream(
    AxumState(shared): AxumState<Arc<Shared>>,
    query: RawQuery,
    headers: header::HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !authorized(&shared, query.0.as_deref(), &headers) {
        return unauthorized();
    }
    let Some(source) = shared.source() else {
        return not_sharing();
    };
    upgrade.on_upgrade(move |socket| peer(shared, source, socket))
}

/// One connected guest. It learns its own peer id in `Welcome`, then commits
/// flow both ways; the host broadcasts every commit in the order it landed.
/// The peer belongs to the share it connected under; unsharing drops it.
async fn peer(shared: Arc<Shared>, source: Arc<dyn HostSource>, socket: WebSocket) {
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let peer = {
        let id = shared.next_peer.fetch_add(1, Ordering::Relaxed);
        shared.peers.lock().await.insert(id, outbound_tx);
        id
    };
    tracing::info!(peer, "guest connected");

    let (mut sink, mut frames) = socket.split();
    let welcome = match source.state().await {
        Ok(encoded) => ServerMessage::Welcome {
            peer,
            revision: revision_of(&encoded),
            document: document_of(&encoded),
            name: source.name(),
        },
        Err(error) => {
            tracing::warn!(%error, "guest could not read the project state");
            shared.peers.lock().await.remove(&peer);
            return;
        }
    };
    let members = ServerMessage::Members {
        members: shared.members.borrow().clone(),
    };
    if send(&mut sink, welcome).await.is_err() || send(&mut sink, members).await.is_err() {
        shared.peers.lock().await.remove(&peer);
        return;
    }

    loop {
        tokio::select! {
            frame = frames.next() => {
                let Some(Ok(frame)) = frame else { break };
                let text = match frame {
                    Message::Text(text) => text.to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(message) = serde_json::from_str::<ClientMessage>(&text) else {
                    continue;
                };
                if let ClientMessage::Presence(presence) = message {
                    shared.present(peer, presence);
                    continue;
                }
                let Some((seq, patch)) = message.into_commit() else {
                    continue;
                };
                // The application broadcasts the commit to everybody while it
                // still holds the project lock, so replicas see the host's
                // order; here only the author learns which revision its own
                // edit earned.
                let reply = match source.apply(patch).await {
                    Ok(revision) => ServerMessage::Ack { seq, revision },
                    Err(reason) => ServerMessage::Resync {
                        revision: *shared.revision.borrow(),
                        seq,
                        reason,
                    },
                };
                if let Some(sender) = shared.peers.lock().await.get(&peer) {
                    let _ = sender.send(reply);
                }
            }
            message = outbound_rx.recv() => match message {
                Some(message) => {
                    if send(&mut sink, message).await.is_err() { break; }
                }
                // The session stopped holding this peer: close the socket so
                // the guest notices instead of editing into a dead host.
                None => break,
            },
        }
    }
    let _ = sink.send(Message::Close(None)).await;
    shared.peers.lock().await.remove(&peer);
    shared.depart(peer);
    tracing::info!(peer, "guest disconnected");
}

async fn send(
    sink: &mut futures::stream::SplitSink<WebSocket, Message>,
    message: ServerMessage,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(&message).unwrap_or_default();
    sink.send(Message::Text(text.into())).await
}

async fn broadcast_commit(shared: &Arc<Shared>, message: ServerMessage) {
    if let ServerMessage::Commit { revision, .. } = message {
        shared.revision.send_replace(revision);
    }
    let peers: Vec<_> = shared.peers.lock().await.values().cloned().collect();
    for peer in peers {
        let _ = peer.send(message.clone());
    }
}

fn authorized(shared: &Shared, query: Option<&str>, headers: &header::HeaderMap) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    protocol::authorized(query, bearer, &shared.token)
}

fn octets(bytes: impl Into<Bytes>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(bytes.into()))
        .expect("static response headers")
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "this session link is not valid").into_response()
}

fn not_sharing() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "the host is not sharing a project right now",
    )
        .into_response()
}

fn internal(error: anyhow::Error) -> Response {
    tracing::warn!(%error, "session request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "session unavailable").into_response()
}

fn revision_of(encoded: &[u8]) -> u64 {
    koharu_storage::decode_state(encoded)
        .map(|stored| stored.revision.get())
        .unwrap_or(0)
}

fn document_of(encoded: &[u8]) -> String {
    koharu_storage::decode_state(encoded)
        .map(|stored| stored.document.to_string())
        .unwrap_or_default()
}

fn new_token() -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use rand::RngExt as _;
    let mut bytes = [0u8; 24];
    rand::rng().fill(&mut bytes[..]);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Guest, GuestStatus, Inbound};
    use koharu_storage::{DocumentId, Revision, StoredState, encode_state};
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;

    /// The application side of a session, held in memory instead of on disk.
    struct Fixture {
        encoded: Vec<u8>,
        applied: AsyncMutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl HostSource for Fixture {
        fn name(&self) -> String {
            "Shared project".to_owned()
        }

        async fn state(&self) -> Result<Vec<u8>> {
            Ok(self.encoded.clone())
        }

        async fn blob(&self, _id: String) -> Result<Option<Bytes>> {
            Ok(None)
        }

        async fn apply(&self, patch: Vec<u8>) -> std::result::Result<u64, String> {
            self.applied.lock().await.push(patch);
            Ok(8)
        }
    }

    fn fixture() -> Arc<Fixture> {
        let state = StoredState {
            document: DocumentId::new(),
            revision: Revision::new(7),
            blobs: Vec::new(),
            payload: b"scene payload".to_vec(),
        };
        Arc::new(Fixture {
            encoded: encode_state(&state).expect("a fresh state encodes"),
            applied: AsyncMutex::new(Vec::new()),
        })
    }

    #[tokio::test]
    async fn guests_edit_through_the_host_and_lose_it_when_it_stops() {
        let source = fixture();
        let (session, link, status) = Session::serve(None).await.expect("the session starts");
        assert_eq!(*status.borrow(), HostStatus::Online);
        let broadcaster = session
            .share(source.clone())
            .await
            .expect("the project is shared");

        let guest = Guest::join(&link).await.expect("the guest joins");
        let connected = guest.status();
        wait_until("the welcome", || {
            *connected.borrow() == GuestStatus::Connected && guest.project_name().is_some()
        })
        .await;
        assert_eq!(guest.project_name().as_deref(), Some("Shared project"));
        assert_eq!(
            guest.fetch_state().await.expect("the project state"),
            source.encoded
        );

        // Presence reaches everyone once announced, the host listed first.
        broadcaster.present(Presence {
            name: "Host".to_owned(),
            page: Some("page-1".to_owned()),
        });
        guest.present(Presence {
            name: "Guest".to_owned(),
            page: Some("page-2".to_owned()),
        });
        let listed = guest.members();
        let hosted = broadcaster.members();
        wait_until("both members to be listed", || {
            listed.borrow().len() == 2 && hosted.borrow().len() == 2
        })
        .await;
        assert_eq!(*listed.borrow(), *hosted.borrow());
        let members = hosted.borrow().clone();
        assert_eq!(members[0].peer, crate::protocol::HOST_PEER);
        assert_eq!(
            members[1].peer,
            guest.peer().expect("the guest was welcomed")
        );
        assert_eq!(members[1].presence.page.as_deref(), Some("page-2"));

        // An edit reaches the host and comes back as its own verdict.
        let sequence = guest.send(vec![1, 2, 3]).await.expect("the edit is sent");
        match guest
            .next()
            .await
            .expect("the stream stays open")
            .expect("a verdict")
        {
            Inbound::Ack {
                seq,
                revision: awarded,
            } => {
                assert_eq!(seq, sequence);
                assert_eq!(awarded, 8);
            }
            _ => panic!("the guest's edit was not acknowledged"),
        }
        assert_eq!(*source.applied.lock().await, vec![vec![1, 2, 3]]);

        // The host's own commits reach every guest in host order.
        broadcaster.broadcast(9, b"host change").await;
        match guest
            .next()
            .await
            .expect("the stream stays open")
            .expect("a commit")
        {
            Inbound::Commit { revision, patch } => {
                assert_eq!(revision, 9);
                assert_eq!(patch, b"host change");
            }
            _ => panic!("the guest did not receive the host's commit"),
        }

        // Unsharing keeps the link: the guest loses the host, then finds it
        // again at the same link once the project is shared again.
        session.unshare().await;
        wait_until("the share to end", || {
            *connected.borrow() != GuestStatus::Connected
        })
        .await;
        assert!(hosted.borrow().is_empty());
        assert!(guest.fetch_state().await.is_err());
        session
            .share(source.clone())
            .await
            .expect("the project is shared again");
        wait_until("the guest to reconnect", || {
            *connected.borrow() == GuestStatus::Connected
        })
        .await;
        assert!(guest.take_resync());

        // Stopping the host takes the link down with it, so the guest can no
        // longer edit: this is what puts a joined project into read-only.
        session.stop().await;
        let connected = guest.status();
        wait_until("the host to disappear", || {
            *connected.borrow() != GuestStatus::Connected
        })
        .await;
        assert!(guest.send(vec![4]).await.is_err());
        guest.close().await;
    }

    /// Waits for `condition`, failing the test rather than hanging on it.
    async fn wait_until(what: &str, condition: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
