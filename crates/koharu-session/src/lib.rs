//! Hosted project sessions: the session host serves its open project over a
//! local HTTP endpoint that a Cloudflare quick tunnel exposes, and guests edit
//! through it. Only the host writes project files; a guest that loses the host
//! keeps the last state it saw and can no longer edit.

pub mod client;
pub mod protocol;
pub mod server;
pub mod tunnel;

pub use client::{Guest, GuestStatus, Inbound};
pub use protocol::{ClientMessage, HOST_PEER, Member, Presence, ServerMessage, join_link};
pub use server::{Broadcaster, HostSource, HostStatus, Session};
pub use tunnel::{Tunnel, TunnelStatus};
