//! The guest side of a session: it connects to a host link, keeps a read-only
//! replica of the host's project, and sends edits. Every message from the host
//! is handed over in host order, so the replica applies commits exactly as the
//! host produced them, and losing the connection makes the replica read-only.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result};
use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use tokio::{
    sync::{Mutex, Notify, mpsc, watch},
    time::Duration,
};

use crate::protocol::{ClientMessage, Member, Presence, ServerMessage, token_from_link};

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
/// A stalled tunnel fails a download instead of hanging its reader. The read
/// timeout restarts on every chunk, so large blobs on slow links still finish.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(30);

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// One message from the host, in the order the host produced it.
#[derive(Clone, Debug)]
pub enum Inbound {
    /// A committed edit, waiting to be applied to the local replica.
    Commit { revision: u64, patch: Vec<u8> },
    /// The host applied this guest's edit and gave it a revision.
    Ack { seq: u64, revision: u64 },
    /// The host refused this guest's edit, so the replica must reload.
    Rejected { seq: u64, reason: String },
}

/// What this guest currently knows about the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestStatus {
    Connected,
    Reconnecting,
    Closed,
}

struct Inner {
    http: reqwest::Client,
    origin: String,
    token: String,
    status: watch::Sender<GuestStatus>,
    peer: watch::Sender<Option<u64>>,
    name: watch::Sender<Option<String>>,
    /// What this guest tells the host about itself; resent on every connect.
    presence: watch::Sender<Presence>,
    members: watch::Sender<Vec<Member>>,
    generation: watch::Sender<u64>,
    inbox: Mutex<VecDeque<Inbound>>,
    inbox_notify: Notify,
    writer: Mutex<Option<mpsc::Sender<ClientMessage>>>,
    closed: AtomicBool,
    resync: AtomicBool,
    sequence: AtomicU64,
}

/// A connection to one session host. Cheap to clone; the connection itself is
/// owned by a background task that reconnects on its own.
#[derive(Clone)]
pub struct Guest {
    inner: Arc<Inner>,
}

impl Guest {
    /// Connects to a host's link. A link that is unreachable or not a session
    /// link fails here rather than in a background task.
    pub async fn join(link: &str) -> Result<Self> {
        let token = token_from_link(link).context("that link is not a Koharu session link")?;
        let origin = origin_of(link)?;
        let ws = websocket_url(&origin, &token)?;
        drop(connect(&ws).await.context("could not reach that session")?);

        let inner = Arc::new(Inner {
            http: reqwest::Client::builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .read_timeout(HTTP_READ_TIMEOUT)
                .build()
                .context("failed to build the session client")?,
            origin,
            token,
            status: watch::channel(GuestStatus::Reconnecting).0,
            peer: watch::channel(None).0,
            name: watch::channel(None).0,
            presence: watch::channel(Presence::default()).0,
            members: watch::channel(Vec::new()).0,
            generation: watch::channel(0).0,
            inbox: Mutex::new(VecDeque::new()),
            inbox_notify: Notify::new(),
            writer: Mutex::new(None),
            closed: AtomicBool::new(false),
            resync: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
        });
        let supervisor = inner.clone();
        tokio::spawn(async move { supervisor.supervise(ws).await });
        Ok(Self { inner })
    }

    pub fn status(&self) -> watch::Receiver<GuestStatus> {
        self.inner.status.subscribe()
    }

    /// This guest's id inside the host's session, once welcomed.
    pub fn peer(&self) -> Option<u64> {
        *self.inner.peer.borrow()
    }

    /// The name the host gives its project, once welcomed.
    pub fn project_name(&self) -> Option<String> {
        self.inner.name.borrow().clone()
    }

    /// Replaces what the host and the other guests see of this guest.
    pub fn present(&self, presence: Presence) {
        self.inner.presence.send_if_modified(|current| {
            let changed = *current != presence;
            *current = presence;
            changed
        });
    }

    /// Everyone in the session, as the host last listed them. Empty while the
    /// host is unreachable.
    pub fn members(&self) -> watch::Receiver<Vec<Member>> {
        self.inner.members.subscribe()
    }

    /// True while the replica is behind the host and must be reloaded.
    pub fn take_resync(&self) -> bool {
        self.inner.resync.swap(false, Ordering::SeqCst)
    }

    /// Asks for the replica to be reloaded from the host, after an edit the
    /// host refused.
    pub fn request_resync(&self) {
        self.inner.resync.store(true, Ordering::SeqCst);
        self.inner.inbox_notify.notify_waiters();
    }

    /// Sends one edit to the host and returns the id that identifies it in the
    /// stream. Fails immediately when the host is not reachable.
    pub async fn send(&self, patch: Vec<u8>) -> std::result::Result<u64, String> {
        if *self.inner.status.borrow() != GuestStatus::Connected {
            return Err("the session host is offline".to_owned());
        }
        if self.inner.resync.load(Ordering::SeqCst) {
            return Err("reconnecting to the session host".to_owned());
        }
        let sequence = self.inner.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let writer = self
            .inner
            .writer
            .lock()
            .await
            .clone()
            .ok_or_else(|| "the session host is offline".to_owned())?;
        writer
            .send(ClientMessage::commit(sequence, patch))
            .await
            .map_err(|_| "the session host is offline".to_owned())?;
        Ok(sequence)
    }

    /// Waits for the next message the host produced. Callers must hold the
    /// project lock, so commits are applied in exactly the order they arrive.
    pub async fn next(&self) -> std::result::Result<Option<Inbound>, String> {
        let generation = *self.inner.generation.borrow();
        loop {
            let notified = self.inner.inbox_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.closed.load(Ordering::SeqCst) {
                return Err("the session ended".to_owned());
            }
            if let Some(inbound) = self.inner.inbox.lock().await.pop_front() {
                return Ok(Some(inbound));
            }
            if *self.inner.status.borrow() != GuestStatus::Connected {
                return Err("the session host is offline".to_owned());
            }
            if *self.inner.generation.borrow() != generation {
                return Err("reconnected to the session host".to_owned());
            }
            notified.await;
        }
    }

    /// Takes a message that is already waiting, without blocking. Only call
    /// this while holding the project lock, so entries are applied in order.
    pub fn poll(&self) -> Option<Inbound> {
        self.inner
            .inbox
            .try_lock()
            .ok()
            .and_then(|mut inbox| inbox.pop_front())
    }

    /// Waits for at least one message to be waiting, without taking it. Used
    /// without the project lock, so an idle reader never blocks edits.
    pub async fn wait(&self) -> std::result::Result<(), String> {
        let generation = *self.inner.generation.borrow();
        loop {
            let notified = self.inner.inbox_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.closed.load(Ordering::SeqCst) {
                return Err("the session ended".to_owned());
            }
            if !self.inner.inbox.lock().await.is_empty() {
                return Ok(());
            }
            if *self.inner.status.borrow() != GuestStatus::Connected {
                return Err("the session host is offline".to_owned());
            }
            if *self.inner.generation.borrow() != generation {
                return Err("reconnected to the session host".to_owned());
            }
            notified.await;
        }
    }

    /// Reads the host's encoded project state, for joining and for reloading
    /// after a reconnect.
    pub async fn fetch_state(&self) -> Result<Vec<u8>> {
        let response = self
            .inner
            .http
            .get(format!("{}/v1/state", self.inner.origin))
            .header("authorization", format!("Bearer {}", self.inner.token))
            .send()
            .await
            .context("failed to read the project state")?
            .error_for_status()
            .context("the session host refused the project state")?;
        Ok(response.bytes().await?.to_vec())
    }

    /// Reads one blob the host's project references.
    pub async fn fetch_blob(&self, id: koharu_storage::BlobId) -> Result<Bytes> {
        let response = self
            .inner
            .http
            .get(format!("{}/v1/blob/{id}", self.inner.origin))
            .header("authorization", format!("Bearer {}", self.inner.token))
            .send()
            .await
            .with_context(|| format!("failed to read blob {id}"))?
            .error_for_status()
            .with_context(|| format!("the session host refused blob {id}"))?;
        Ok(response.bytes().await?)
    }

    /// Closes the session. The host is unaffected.
    pub async fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.writer.lock().await.take();
        self.inner.status.send_replace(GuestStatus::Closed);
        self.inner.inbox_notify.notify_waiters();
    }
}

impl Inner {
    async fn supervise(self: Arc<Self>, ws: String) {
        let mut backoff = RECONNECT_MIN;
        loop {
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            match self.stream(&ws).await {
                Ok(()) => backoff = RECONNECT_MIN,
                Err(error) => tracing::debug!(%error, "session connection ended"),
            }
            // Everything received before this point may be stale; the replica
            // reloads from the host when it reconnects.
            self.inbox.lock().await.clear();
            self.writer.lock().await.take();
            self.members.send_replace(Vec::new());
            self.status.send_replace(GuestStatus::Reconnecting);
            self.inbox_notify.notify_waiters();
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_MAX);
        }
        self.status.send_replace(GuestStatus::Closed);
        self.inbox_notify.notify_waiters();
    }

    /// Runs one connection until it drops.
    async fn stream(&self, ws: &str) -> Result<()> {
        let socket: Socket = connect(ws).await?;
        let mut generation = 0;
        self.generation.send_modify(|value| {
            *value += 1;
            generation = *value;
        });
        if generation > 1 {
            self.resync.store(true, Ordering::SeqCst);
        }
        let (mut sink, mut source) = socket.split();
        let (outbound, mut inbox) = mpsc::channel::<ClientMessage>(32);
        *self.writer.lock().await = Some(outbound);
        self.status.send_replace(GuestStatus::Connected);
        self.inbox_notify.notify_waiters();

        // Announce on every connect, but not before the application has said
        // who this is: the host lists a guest from its first presence.
        let mut presence = self.presence.subscribe();
        if *presence.borrow() != Presence::default() {
            presence.mark_changed();
        }
        let writer = tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    message = inbox.recv() => match message {
                        Some(message) => message,
                        None => return,
                    },
                    Ok(()) = presence.changed() => {
                        ClientMessage::Presence(presence.borrow_and_update().clone())
                    }
                };
                let text = serde_json::to_string(&message).unwrap_or_default();
                if sink
                    .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });

        while let Some(Ok(frame)) = source.next().await {
            match frame {
                tokio_tungstenite::tungstenite::Message::Text(text) => {
                    if let Some(inbound) = decode(&text, self) {
                        self.inbox.lock().await.push_back(inbound);
                        self.inbox_notify.notify_waiters();
                    }
                }
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                _ => {}
            }
        }
        writer.abort();
        anyhow::bail!("the host closed the connection")
    }
}

fn decode(text: &str, inner: &Inner) -> Option<Inbound> {
    let message: ServerMessage = serde_json::from_str(text).ok()?;
    match message {
        ServerMessage::Welcome {
            peer,
            revision: _,
            name,
            ..
        } => {
            inner.peer.send_replace(Some(peer));
            inner.name.send_replace(Some(name));
            None
        }
        ServerMessage::Commit { revision, patch } => Some(Inbound::Commit {
            revision,
            patch: decode_patch(&patch)?,
        }),
        ServerMessage::Ack { seq, revision } => Some(Inbound::Ack { seq, revision }),
        ServerMessage::Resync { seq, reason, .. } => Some(Inbound::Rejected { seq, reason }),
        ServerMessage::Members { members } => {
            inner.members.send_replace(members);
            None
        }
    }
}

fn decode_patch(value: &str) -> Option<Vec<u8>> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD.decode(value).ok()
}

async fn connect(ws: &str) -> Result<Socket> {
    let (socket, _) = tokio_tungstenite::connect_async(ws)
        .await
        .context("could not open the session stream")?;
    Ok(socket)
}

fn origin_of(link: &str) -> Result<String> {
    let url = url::Url::parse(link).context("that link is not a Koharu session link")?;
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
        anyhow::bail!("that link is not a Koharu session link");
    }
    let mut origin = url.clone();
    origin.set_path("");
    origin.set_query(None);
    origin.set_fragment(None);
    Ok(origin.to_string().trim_end_matches('/').to_owned())
}

fn websocket_url(origin: &str, token: &str) -> Result<String> {
    let mut url = url::Url::parse(origin)?;
    match url.scheme() {
        "https" => url.set_scheme("wss"),
        "http" => url.set_scheme("ws"),
        other => anyhow::bail!("unsupported session scheme {other}"),
    }
    .map_err(|_| anyhow::anyhow!("unsupported session scheme"))?;
    url.set_path("/v1/stream");
    url.set_query(Some(&format!("k={token}")));
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_parts_are_derived_from_the_pasted_url() {
        let link = "https://neat-words.trycloudflare.com/?k=secret";
        assert_eq!(
            origin_of(link).unwrap(),
            "https://neat-words.trycloudflare.com"
        );
        assert_eq!(
            websocket_url(&origin_of(link).unwrap(), "secret").unwrap(),
            "wss://neat-words.trycloudflare.com/v1/stream?k=secret"
        );
        assert!(origin_of("https://example.com").is_ok());
        assert!(origin_of("file:///tmp/x").is_err());
    }
}
