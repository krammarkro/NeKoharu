//! The wire protocol shared by a session host and its guests: three HTTP
//! routes for bulk data and one WebSocket route for commits.

use anyhow::{Context as _, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

/// Query parameter carrying the session token. The link is the credential, so
/// every request must present it.
pub const TOKEN_PARAM: &str = "k";

/// Path guests fetch the encoded project state from.
pub const STATE_PATH: &str = "/v1/state";
/// Path guests fetch one content-addressed blob from.
pub const BLOB_PATH: &str = "/v1/blob/";
/// Path guests upgrade to the commit stream on.
pub const STREAM_PATH: &str = "/v1/stream";

/// True when `request` carries the session token, either as the token query
/// parameter or as a bearer header.
pub fn authorized(uri_query: Option<&str>, bearer: Option<&str>, token: &str) -> bool {
    let from_query = uri_query
        .map(|query| url::form_urlencoded::parse(query.as_bytes()))
        .into_iter()
        .flatten()
        .any(|(key, value)| key == TOKEN_PARAM && value == token);
    from_query || bearer == Some(&format!("Bearer {token}"))
}

/// The token embedded in a join link, or `None` when the link is malformed.
pub fn token_from_link(link: &str) -> Option<String> {
    let url = url::Url::parse(link).ok()?;
    url.query_pairs()
        .find(|(key, _)| key == TOKEN_PARAM)
        .map(|(_, value)| value.into_owned())
}

/// The link a host shares: the tunnel origin plus the token that opens it.
pub fn join_link(origin: &str, token: &str) -> String {
    let separator = if origin.contains('?') { '&' } else { '?' };
    format!("{origin}{separator}{TOKEN_PARAM}={token}")
}

/// Strips the token from a link so it can be shown without granting access.
pub fn link_without_token(link: &str) -> Option<String> {
    let mut url = url::Url::parse(link).ok()?;
    let pairs = url
        .query_pairs()
        .filter(|(key, _)| key != TOKEN_PARAM)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
    Some(url.to_string())
}

fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn decode(value: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value)
        .context("session payload is not valid base64")
}

/// The peer id the host takes in its own session; guests count up from one.
pub const HOST_PEER: u64 = 0;

/// Who a participant is and which page they have open. It is view state, so it
/// travels beside the commits instead of in the scene.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    /// The participant's own name for themselves; empty when unknown.
    pub name: String,
    /// The page they have open, if any.
    pub page: Option<String>,
}

/// One participant of a session, as the host lists them.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub peer: u64,
    #[serde(flatten)]
    pub presence: Presence,
}

/// Messages a guest sends over the commit stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Applies one edit. `seq` identifies the send so its originator can wait
    /// for the host's verdict.
    Commit { seq: u64, patch: String },
    /// Replaces this guest's presence. A guest is listed as a member once it
    /// has sent one.
    Presence(Presence),
}

/// Messages the host sends over the commit stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        peer: u64,
        revision: u64,
        document: String,
        name: String,
    },
    /// One committed edit, in host order, sent to every guest including its
    /// author. Guests apply commits in the order they arrive.
    Commit { revision: u64, patch: String },
    /// The host applied a guest's edit and handed it this revision.
    Ack { seq: u64, revision: u64 },
    /// The host could not apply a guest's patch; the guest must reload the
    /// project state and drop the change.
    Resync {
        revision: u64,
        seq: u64,
        reason: String,
    },
    /// Everyone in the session, the host first, sent whenever anyone joins,
    /// leaves, or moves to another page.
    Members { members: Vec<Member> },
}

impl ServerMessage {
    pub fn commit(revision: u64, patch: &[u8]) -> Self {
        Self::Commit {
            revision,
            patch: encode(patch),
        }
    }

    pub fn into_commit(self) -> Option<(u64, Vec<u8>)> {
        match self {
            Self::Commit { revision, patch } => Some((revision, decode(&patch).ok()?)),
            _ => None,
        }
    }
}

impl ClientMessage {
    pub fn commit(seq: u64, patch: Vec<u8>) -> Self {
        Self::Commit {
            seq,
            patch: encode(&patch),
        }
    }

    pub fn into_commit(self) -> Option<(u64, Vec<u8>)> {
        match self {
            Self::Commit { seq, patch } => Some((seq, decode(&patch).ok()?)),
            Self::Presence(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_token_round_trips_and_can_be_hidden() {
        let link = "https://abc.trycloudflare.com/?k=s3cret-token";
        assert_eq!(token_from_link(link).as_deref(), Some("s3cret-token"));
        assert_eq!(
            link_without_token(link).as_deref(),
            Some("https://abc.trycloudflare.com/")
        );
        assert_eq!(token_from_link("not a link"), None);
    }

    #[test]
    fn authorization_requires_the_exact_token() {
        assert!(authorized(Some("k=alpha"), None, "alpha"));
        assert!(authorized(None, Some("Bearer alpha"), "alpha"));
        assert!(!authorized(Some("k=alpha"), None, "beta"));
        assert!(!authorized(Some("other=alpha"), None, "alpha"));
        assert!(!authorized(None, None, "alpha"));
    }

    #[test]
    fn commit_messages_round_trip() {
        let message = ClientMessage::commit(7, b"patch bytes".to_vec());
        let encoded = serde_json::to_string(&message).unwrap();
        let decoded: ClientMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.into_commit(), Some((7, b"patch bytes".to_vec())));

        let message = ServerMessage::commit(3, b"host bytes");
        let encoded = serde_json::to_string(&message).unwrap();
        let decoded: ServerMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.into_commit(), Some((3, b"host bytes".to_vec())));
    }
}
