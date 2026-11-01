//! Live sessions: one machine hosts its open project through a Cloudflare
//! quick tunnel, other machines join it and edit. The host owns the files and
//! is the only machine that writes; guests hold a replica that follows the
//! host's revisions and becomes read-only whenever the host is unreachable.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt as _, future::BoxFuture};
use koharu_scene::{BlobId, Patch, Revision, Session};
use koharu_session::{
    Guest, GuestStatus, HOST_PEER, HostSource, HostStatus, Inbound, Member, Presence, Tunnel,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Manager as _, State, ipc::Channel};
use tauri_runtime_cef::CefRuntime;

use super::{
    ChannelExt as _, Error,
    canvas::CanvasChannel,
    lifecycle::{ProjectChannel, replace_project},
    project::{CurrentProject, Project, ProjectMode},
};

/// How often the guest looks for work while idle.
const TICK: Duration = Duration::from_millis(400);
/// Blob downloads a joined replica keeps in flight ahead of the viewer.
const PREFETCH_CONCURRENCY: usize = 4;

/// What this instance is doing with other machines, sent to the frontend.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    Hosting,
    Joined,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, Type)]
pub struct SessionInfo {
    /// `None` when no session is running.
    pub role: Option<SessionRole>,
    /// The link to hand out, while hosting.
    pub link: Option<String>,
    /// False once the other side has gone away.
    pub online: bool,
    /// Everyone in the session, the host first. Empty while offline.
    pub members: Vec<SessionMember>,
}

/// One participant of the running session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, Type)]
pub struct SessionMember {
    /// Stable for as long as the participant stays connected.
    #[specta(type = f64)]
    pub peer: u64,
    /// The participant's own name for themselves; empty when unknown.
    pub name: String,
    /// The id of the page they have open.
    pub page: Option<String>,
    pub host: bool,
    /// True for this instance's own entry.
    pub you: bool,
}

fn session_members(members: &[Member], you: Option<u64>) -> Vec<SessionMember> {
    members
        .iter()
        .map(|member| SessionMember {
            peer: member.peer,
            name: member.presence.name.clone(),
            page: member.presence.page.clone(),
            host: member.peer == HOST_PEER,
            you: Some(member.peer) == you,
        })
        .collect()
}

/// The longest display name, in characters; the rest is cut off.
const NAME_LIMIT: usize = 32;

/// The `session` section of the configuration file.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct SessionConfig {
    /// What this instance is called in sessions. Never blank once read: an
    /// empty name is replaced by a generated one, so every participant reads
    /// as somebody.
    name: String,
}

/// Reads the display name, generating and keeping one when there is none.
fn display_name() -> Result<String> {
    let config = koharu_config::load::<SessionConfig>("session")?;
    {
        let current = config.read()?;
        if !current.name.trim().is_empty() {
            return Ok(current.name.clone());
        }
    }
    let mut current = config.write()?;
    if current.name.trim().is_empty() {
        current.name = generated_name();
    }
    let name = current.name.clone();
    current.save()?;
    Ok(name)
}

/// A random two-word name, such as "Swift Otter".
fn generated_name() -> String {
    const ADJECTIVES: [&str; 16] = [
        "Swift", "Quiet", "Bright", "Brave", "Clever", "Gentle", "Lucky", "Merry", "Nimble",
        "Sunny", "Calm", "Bold", "Witty", "Cozy", "Keen", "Jolly",
    ];
    const ANIMALS: [&str; 16] = [
        "Otter", "Fox", "Panda", "Heron", "Lynx", "Koala", "Badger", "Falcon", "Hedgehog", "Owl",
        "Seal", "Rabbit", "Crane", "Tanuki", "Sparrow", "Wolf",
    ];
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let adjective = ADJECTIVES[usize::from(bytes[0]) % ADJECTIVES.len()];
    let animal = ANIMALS[usize::from(bytes[1]) % ANIMALS.len()];
    format!("{adjective} {animal}")
}

/// What this instance tells the other participants about itself: its display
/// name, and the page it has open.
pub(crate) fn presence(page: Option<koharu_scene::EntityId>) -> Presence {
    let name = display_name().unwrap_or_else(|error| {
        tracing::warn!(%error, "failed to read the session display name");
        String::new()
    });
    Presence {
        name,
        page: page.map(|page| page.to_string()),
    }
}

/// The name this instance goes by in sessions.
#[tauri::command]
#[specta::specta]
pub(crate) async fn get_session_name() -> Result<String, Error> {
    Ok(display_name()?)
}

/// Renames this instance and tells a running session at once. A blank name
/// is replaced by a generated one; the name in effect is returned.
#[tauri::command]
#[specta::specta]
pub(crate) async fn set_session_name(
    name: String,
    handle: AppHandle<CefRuntime>,
) -> Result<String, Error> {
    let name: String = name.trim().chars().take(NAME_LIMIT).collect();
    let name = if name.is_empty() {
        generated_name()
    } else {
        name
    };
    {
        let config = koharu_config::load::<SessionConfig>("session")?;
        let mut current = config.write()?;
        current.name = name.clone();
        current.save()?;
    }
    let current = handle.state::<CurrentProject>();
    if let Some(project) = current.project.lock().await.as_ref() {
        project.present();
    }
    Ok(name)
}

/// The session state the frontend is showing, kept so a late subscriber sees
/// the same thing.
#[derive(Default)]
pub(crate) struct SessionChannel {
    channel: Mutex<Option<Channel<SessionInfo>>>,
    current: Mutex<SessionInfo>,
}

impl SessionChannel {
    pub(crate) fn attach(&self, channel: Channel<SessionInfo>) {
        *self.channel.lock() = Some(channel);
        let current = self.current.lock().clone();
        self.channel.publish(current);
    }

    pub(crate) fn publish_info(&self, info: SessionInfo) {
        *self.current.lock() = info.clone();
        self.channel.publish(info);
    }

    /// Replaces the member list while hosting; a later stop or a dead tunnel
    /// has already said everything there is to say.
    fn publish_hosted_members(&self, members: Vec<SessionMember>) {
        let info = {
            let mut current = self.current.lock();
            if current.role != Some(SessionRole::Hosting) || current.members == members {
                return;
            }
            current.members = members;
            current.clone()
        };
        self.channel.publish(info);
    }
}

/// The sessions this instance takes part in.
#[derive(Default)]
pub(crate) struct SessionState {
    /// The link this instance hands out. It outlives each share, so a guest
    /// that lost the host finds it again once a project is shared again; only
    /// the tunnel dying ends it.
    host: tokio::sync::Mutex<Option<Host>>,
    guest: Mutex<Option<Joined>>,
}

struct Host {
    session: koharu_session::Session,
    link: String,
    status: tokio::sync::watch::Receiver<HostStatus>,
    sharing: bool,
}

struct Joined {
    guest: Guest,
    task: tokio::task::JoinHandle<()>,
}

/// The project side of a hosted session: it can read the project the host
/// owns and apply guests' edits to it.
struct HostProject {
    handle: AppHandle<CefRuntime>,
    name: String,
}

#[async_trait]
impl HostSource for HostProject {
    fn name(&self) -> String {
        self.name.clone()
    }

    async fn state(&self) -> Result<Vec<u8>> {
        let current = self.handle.state::<CurrentProject>();
        let project = current.project.lock().await;
        let project = project.as_ref().context("no project is open")?;
        project.snapshot().encode().map_err(Into::into)
    }

    async fn blob(&self, id: String) -> Result<Option<Bytes>> {
        let id = id.parse::<BlobId>().context("invalid blob id")?;
        // Read from a snapshot outside the project lock, so serving guests'
        // downloads never stalls the host's own edits.
        let snapshot = {
            let current = self.handle.state::<CurrentProject>();
            let project = current.project.lock().await;
            let Some(project) = project.as_ref() else {
                return Ok(None);
            };
            project.snapshot()
        };
        Ok(snapshot.read_blob(id).await.ok())
    }

    async fn apply(&self, patch: Vec<u8>) -> std::result::Result<u64, String> {
        let revision = {
            let current = self.handle.state::<CurrentProject>();
            let mut open = current.project.lock().await;
            let project = open
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())?;
            let snapshot = project.snapshot();
            let patch = Patch::decode(&patch, &snapshot)
                .map_err(|error| format!("your change conflicts with the host's copy: {error}"))?;
            if patch.is_empty() {
                return Err("that change was already applied".to_owned());
            }
            let commit = project
                .commit(patch)
                .await
                .map_err(|error| format!("{error:#}"))?;
            commit.revision.get()
        };
        // The host's own view moved under it; without this its next edit is
        // based on a revision that no longer exists.
        refresh(&self.handle).await;
        Ok(revision)
    }
}

/// A joined replica's blobs come from the host the first time they are read.
struct HostBlobs(Guest);

impl koharu_storage::BlobOrigin for HostBlobs {
    fn fetch(&self, id: BlobId) -> BoxFuture<'_, koharu_storage::Result<Bytes>> {
        Box::pin(async move {
            self.0
                .fetch_blob(id)
                .await
                .map_err(koharu_storage::Error::Task)
        })
    }
}

/// Where joined replicas keep the blobs they downloaded. Blobs are addressed
/// by content, so every session shares it and a rejoin skips what it has.
fn blob_cache() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("koharu")
        .join("session-blobs")
}

/// Opens a replica of the host's current state. Only the state is fetched
/// here; images arrive as they are read or prefetched.
async fn open_replica(guest: &Guest) -> Result<Session> {
    let encoded = guest
        .fetch_state()
        .await
        .context("failed to read the project")?;
    Session::replica(&encoded, &blob_cache(), Arc::new(HostBlobs(guest.clone())))
        .await
        .context("failed to open the project")
}

/// Stops a background download when the guest task ends or the replica it
/// was filling is replaced.
struct Prefetch(tokio::task::JoinHandle<()>);

impl Drop for Prefetch {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Downloads the open replica's blobs in project order, a few at a time, so
/// pages are ready before the guest opens them.
fn prefetch(handle: AppHandle<CefRuntime>) -> Prefetch {
    Prefetch(tokio::spawn(async move {
        let snapshot = {
            let current = handle.state::<CurrentProject>();
            let open = current.project.lock().await;
            let Some(project) = open.as_ref() else {
                return;
            };
            project.snapshot()
        };
        let mut downloads = futures::stream::iter(snapshot.blobs_in_page_order())
            .map(|id| {
                let snapshot = snapshot.clone();
                async move { (id, snapshot.fetch_blob(id).await) }
            })
            .buffered(PREFETCH_CONCURRENCY);
        while let Some((id, result)) = downloads.next().await {
            if let Err(error) = result {
                tracing::debug!(%id, %error, "blob prefetch failed; it is fetched when read");
            }
        }
    }))
}

/// Starts sharing the open project: brings up a quick tunnel and hands back
/// the link to give to other users.
#[tauri::command]
#[specta::specta]
pub(crate) async fn start_session(
    handle: AppHandle<CefRuntime>,
    session_state: State<'_, SessionState>,
    session_channel: State<'_, SessionChannel>,
) -> Result<SessionInfo, Error> {
    if session_state.guest.lock().is_some() {
        return Err(anyhow!("leave the joined session before sharing").into());
    }
    let name = {
        let current = handle.state::<CurrentProject>();
        let project = current.project.lock().await;
        project
            .as_ref()
            .context("open a project before starting a session")?
            .name
            .clone()
    };

    let mut host = session_state.host.lock().await;
    if host.as_ref().is_some_and(|host| host.sharing) {
        return Err(anyhow!("this project is already shared").into());
    }
    let host = match &mut *host {
        Some(host) => host,
        empty => {
            let tunnel = Tunnel::acquire()
                .await
                .context("cloudflared is unavailable")?;
            let (session, link, status) = koharu_session::Session::start(tunnel).await?;
            watch_host(handle.clone(), link.clone(), status.clone());
            watch_members(handle.clone(), session.members());
            empty.insert(Host {
                session,
                link,
                status,
                sharing: false,
            })
        }
    };
    let broadcaster = host
        .session
        .share(Arc::new(HostProject {
            handle: handle.clone(),
            name,
        }))
        .await?;
    {
        let current = handle.state::<CurrentProject>();
        let mut open = current.project.lock().await;
        let project = open.as_mut().context("no project is open")?;
        project.set_mode(ProjectMode::Host(broadcaster));
    }
    host.sharing = true;

    let info = SessionInfo {
        role: Some(SessionRole::Hosting),
        link: Some(host.link.clone()),
        online: *host.status.borrow() == HostStatus::Online,
        members: session_members(&host.session.members().borrow(), Some(HOST_PEER)),
    };
    session_channel.publish_info(info.clone());
    Ok(info)
}

/// Keeps the host's member list current for as long as its link lives.
fn watch_members(
    handle: AppHandle<CefRuntime>,
    mut members: tokio::sync::watch::Receiver<Vec<Member>>,
) {
    tokio::spawn(async move {
        while members.changed().await.is_ok() {
            let list = session_members(&members.borrow_and_update(), Some(HOST_PEER));
            handle
                .state::<SessionChannel>()
                .publish_hosted_members(list);
        }
    });
}

/// Ends the host's link once its tunnel dies, so the next share opens a new
/// one, and tells the frontend if a project was being shared through it.
fn watch_host(
    handle: AppHandle<CefRuntime>,
    link: String,
    mut status: tokio::sync::watch::Receiver<HostStatus>,
) {
    tokio::spawn(async move {
        while status.changed().await.is_ok() {
            if *status.borrow_and_update() != HostStatus::Offline {
                continue;
            }
            let state = handle.state::<SessionState>();
            let mut host = state.host.lock().await;
            if host.as_ref().is_none_or(|host| host.link != link) {
                return;
            }
            let ended = host.take().expect("the host was just checked");
            ended.session.stop().await;
            drop(host);
            if ended.sharing {
                {
                    let current = handle.state::<CurrentProject>();
                    let mut open = current.project.lock().await;
                    if let Some(project) = open.as_mut() {
                        project.set_mode(ProjectMode::Local);
                    }
                }
                publish(
                    &handle,
                    SessionInfo {
                        role: Some(SessionRole::Hosting),
                        link: Some(link),
                        online: false,
                        members: Vec::new(),
                    },
                );
            }
            return;
        }
    });
}

/// Stops sharing. The link stays reserved for the next share, and the project
/// and its files are untouched.
#[tauri::command]
#[specta::specta]
pub(crate) async fn stop_session(
    handle: AppHandle<CefRuntime>,
    session_channel: State<'_, SessionChannel>,
) -> Result<(), Error> {
    stop(&handle).await;
    session_channel.publish_info(SessionInfo::default());
    Ok(())
}

/// Joins someone else's session. The local library is not touched: the project
/// lives on the host and only its replica is held here. Joining while already
/// joined is how a guest follows a host whose link changed: the current
/// replica stays until the new link has delivered a project.
#[tauri::command]
#[specta::specta]
pub(crate) async fn join_session(
    link: String,
    handle: AppHandle<CefRuntime>,
    session_state: State<'_, SessionState>,
    session_channel: State<'_, SessionChannel>,
) -> Result<SessionInfo, Error> {
    let guest = Guest::join(link.trim()).await?;
    let session = match open_replica(&guest).await {
        Ok(session) => session,
        Err(error) => {
            guest.close().await;
            return Err(error.into());
        }
    };
    stop(&handle).await;
    let name = guest
        .project_name()
        .unwrap_or_else(|| "Shared project".to_owned());

    let opened = Project::new(session, name).with_mode(ProjectMode::Guest(guest.clone()));
    replace_project(&handle, opened).await?;

    let task = spawn_guest(handle.clone(), guest.clone());
    *session_state.guest.lock() = Some(Joined {
        guest: guest.clone(),
        task,
    });

    let info = SessionInfo {
        role: Some(SessionRole::Joined),
        link: None,
        online: *guest.status().borrow() == GuestStatus::Connected,
        members: session_members(&guest.members().borrow(), guest.peer()),
    };
    session_channel.publish_info(info.clone());
    Ok(info)
}

/// Leaves the joined session and closes the replica.
#[tauri::command]
#[specta::specta]
pub(crate) async fn leave_session(
    handle: AppHandle<CefRuntime>,
    session_channel: State<'_, SessionChannel>,
) -> Result<(), Error> {
    stop(&handle).await;
    super::lifecycle::close_current_project(&handle).await?;
    session_channel.publish_info(SessionInfo::default());
    Ok(())
}

/// Ends whatever the open project is doing with other machines: a shared
/// project is unshared, keeping the link, and a joined one is left. Safe to
/// call when there is nothing to end.
pub(crate) async fn stop(handle: &AppHandle<CefRuntime>) {
    let state = handle.state::<SessionState>();
    if let Some(host) = state.host.lock().await.as_mut()
        && host.sharing
    {
        host.session.unshare().await;
        host.sharing = false;
    }
    let joined = state.guest.lock().take();
    if let Some(Joined { guest, task }) = joined {
        guest.close().await;
        task.abort();
    }
    {
        let current = handle.state::<CurrentProject>();
        let mut open = current.project.lock().await;
        if let Some(project) = open.as_mut() {
            project.set_mode(ProjectMode::Local);
        }
    }
}

/// Publishes the current session state, so a late subscriber sees it too.
pub(crate) fn publish(handle: &AppHandle<CefRuntime>, info: SessionInfo) {
    handle.state::<SessionChannel>().publish_info(info);
}

/// Applies the host's commits to the replica, then refreshes the canvas.
///
/// The project lock is taken before anything is popped from the inbox, so the
/// replica applies commits in exactly the host's order; while idle the lock is
/// released, so edits are never queued behind an empty stream.
fn spawn_guest(handle: AppHandle<CefRuntime>, guest: Guest) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut status = guest.status();
        let mut members = guest.members();
        let mut published: Option<SessionInfo> = None;
        let mut _prefetch = prefetch(handle.clone());
        loop {
            let connected = *status.borrow() == GuestStatus::Connected;
            let info = SessionInfo {
                role: Some(SessionRole::Joined),
                link: None,
                online: connected,
                members: session_members(&members.borrow_and_update(), guest.peer()),
            };
            if published.as_ref() != Some(&info) {
                publish(&handle, info.clone());
                published = Some(info);
            }
            if *status.borrow() == GuestStatus::Closed {
                break;
            }

            if connected && guest.take_resync() {
                match reload(&handle, &guest).await {
                    Ok(true) => {
                        _prefetch = prefetch(handle.clone());
                        refresh(&handle).await;
                    }
                    Ok(false) => {
                        // The link now serves another project. Keep the replica
                        // read-only rather than swapping the guest's project.
                        tracing::warn!("the host is sharing a different project at this link");
                        guest.close().await;
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to reload the joined project");
                        tokio::time::sleep(TICK).await;
                        continue;
                    }
                }
            }
            if connected && apply_queued(&handle, &guest).await {
                refresh(&handle).await;
            }

            let idle = tokio::select! {
                _ = status.changed() => false,
                _ = members.changed() => false,
                result = guest.wait() => result.is_err(),
                _ = tokio::time::sleep(TICK) => false,
            };
            if idle {
                tokio::time::sleep(TICK).await;
            }
        }
    })
}

/// Drains whatever the host has sent, under the project lock.
async fn apply_queued(handle: &AppHandle<CefRuntime>, guest: &Guest) -> bool {
    let mut applied = false;
    {
        let current = handle.state::<CurrentProject>();
        let mut open = current.project.lock().await;
        let Some(project) = open.as_mut() else {
            return false;
        };
        while let Some(inbound) = guest.poll() {
            let Inbound::Commit {
                revision, patch, ..
            } = inbound
            else {
                // A verdict for an edit whose command already gave up.
                continue;
            };
            if let Err(error) = apply_commit(project, revision, &patch).await {
                tracing::warn!(%error, "host commit could not be applied");
                guest.request_resync();
                break;
            }
            applied = true;
        }
    }
    applied
}

/// Applies one commit the host announced, ignoring one already reflected here.
async fn apply_commit(project: &mut Project, revision: u64, patch: &[u8]) -> Result<()> {
    let revision = Revision::new(revision);
    let snapshot = project.snapshot();
    if revision <= snapshot.revision() {
        return Ok(());
    }
    let patch = Patch::decode(patch, &snapshot)?;
    project.session.commit_remote(patch, revision).await?;
    Ok(())
}

/// Rebuilds the replica from the host after a reconnect or a rejected edit.
/// Returns false, leaving the replica alone, when the host now shares a
/// different project.
async fn reload(handle: &AppHandle<CefRuntime>, guest: &Guest) -> Result<bool> {
    let session = open_replica(guest).await?;
    let current = handle.state::<CurrentProject>();
    let mut open = current.project.lock().await;
    let Some(project) = open.as_mut() else {
        return Ok(true);
    };
    if project.session.project_id() != session.project_id() {
        return Ok(false);
    }
    project.reload(session);
    Ok(true)
}

/// Publishes the open project's new state after a commit made by the other
/// side of the session landed here.
async fn refresh(handle: &AppHandle<CefRuntime>) {
    let state = handle.state::<CurrentProject>();
    let current = {
        let open = state.project.lock().await;
        open.as_ref()
            .map(|project| (project.snapshot(), project.active_page(), project.info()))
    };
    let Some((snapshot, page, info)) = current else {
        return;
    };
    let desktop = handle.state::<koharu_desktop::Desktop>();
    if let Err(error) = desktop.show_page(&snapshot, page).await {
        tracing::warn!(%error, "failed to show the refreshed page");
        return;
    }
    handle
        .state::<CanvasChannel>()
        .channel
        .publish(desktop.canvas_state());
    handle.state::<ProjectChannel>().channel.publish(Some(info));
}
