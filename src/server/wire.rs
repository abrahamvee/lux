//! The hub-remote protocol: length-prefixed bincode frames over the stdio
//! of a `lux proxy` run through ssh.

use std::io::{Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use termwiz::input::{KeyCode, Modifiers};
use wezterm_term::{Line, MouseEvent};

use crate::server::agent::{AgentKind, AgentState};
use crate::server::layout::WindowId;
use crate::server::palette::TermColors;
use crate::server::persist::NodeSnapshot;

/// Bumped whenever a message changes shape.
const PROTOCOL: u32 = 1;

/// Frames past this are corrupt, not content.
const MAX_FRAME: usize = 1 << 30;

pub fn version() -> String {
    format!("{} (protocol {PROTOCOL})", env!("CARGO_PKG_VERSION"))
}

pub type RemoteTabId = usize;
pub type RemoteSessionId = usize;

/// A tab the hub spawned is addressed by the token it chose until the
/// remote's id comes back, and by that token for the rest of the
/// connection.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TabRef {
    Id(RemoteTabId),
    Token(u64),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum HubMsg {
    Hello {
        version: String,
        instance: u64,
    },
    Key {
        tab: TabRef,
        code: KeyCode,
        mods: Modifiers,
    },
    Paste {
        tab: TabRef,
        text: String,
    },
    Mouse {
        tab: TabRef,
        event: MouseEvent,
    },
    Resize {
        tab: TabRef,
        cols: u16,
        rows: u16,
    },
    /// Starts in `cwd_of`'s working directory when given.
    Spawn {
        token: u64,
        session: RemoteSessionId,
        cwd_of: Option<TabRef>,
        cols: u16,
        rows: u16,
    },
    Kill(TabRef),
    /// `None` returns the tab to automatic naming.
    SetName {
        tab: TabRef,
        name: Option<String>,
    },
    Seen(TabRef),
    Layout {
        session: RemoteSessionId,
        layout: LayoutSnap,
    },
    NewSession {
        request: u64,
        name: Option<String>,
        cols: u16,
        rows: u16,
    },
    RenameSession {
        session: RemoteSessionId,
        name: String,
    },
    KillSession(RemoteSessionId),
    /// What the hub's client terminal reported, for answering programs'
    /// color queries.
    Colors(TermColors),
    Resync(RemoteTabId),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum RemoteMsg {
    Hello {
        version: String,
        instance: u64,
    },
    Refused(String),
    /// Every local session, sent once after the handshake.
    Sessions(Vec<SessionSnap>),
    SessionAdded {
        request: Option<u64>,
        session: SessionSnap,
    },
    NewSessionFailed {
        request: u64,
        reason: String,
    },
    SessionEnded(RemoteSessionId),
    Spawned {
        token: u64,
        tab: TabSnap,
    },
    SpawnFailed(u64),
    Update {
        tab: RemoteTabId,
        update: TabUpdate,
    },
    Full {
        tab: RemoteTabId,
        snap: TabSnap,
    },
    Exited(RemoteTabId),
    Notice {
        tab: RemoteTabId,
        blocked: bool,
        summary: Option<String>,
    },
    Clipboard {
        tab: RemoteTabId,
        text: String,
    },
    /// Another hub took over this host.
    Evicted,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct LayoutSnap {
    pub tree: NodeSnapshot,
    pub windows: Vec<WindowLayout>,
    pub minimized: Vec<WindowId>,
    pub focus: WindowId,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct WindowLayout {
    pub id: WindowId,
    pub active: usize,
    pub tabs: Vec<TabRef>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SessionSnap {
    pub id: RemoteSessionId,
    pub name: String,
    pub tree: NodeSnapshot,
    pub windows: Vec<WindowSnap>,
    pub minimized: Vec<WindowId>,
    pub focus: WindowId,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WindowSnap {
    pub id: WindowId,
    pub active: usize,
    pub tabs: Vec<TabSnap>,
}

/// A tab's whole state. On the alternate screen `lines` holds that screen
/// only, and the primary screen follows once the program leaves it.
#[derive(Serialize, Deserialize, Debug)]
pub struct TabSnap {
    pub id: RemoteTabId,
    pub meta: TabMeta,
    pub screen: ScreenState,
    pub lines: Vec<Line>,
}

/// What changed since the last update. `lines` are keyed by the remote's
/// stable row index.
#[derive(Serialize, Deserialize, Debug)]
pub struct TabUpdate {
    pub meta: TabMeta,
    pub screen: ScreenState,
    pub lines: Vec<(isize, Line)>,
    /// PTY bytes since the last update, which pace the rule's shimmer.
    pub bytes: usize,
    pub bell: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct TabMeta {
    pub name: String,
    pub progress: Option<u8>,
    pub mouse_grabbed: bool,
    pub agent: Option<AgentReport>,
}

impl TabMeta {
    /// Equal but for the time the agent state has held, which the hub
    /// counts on its own.
    pub fn same(&self, other: &TabMeta) -> bool {
        let agent = |m: &TabMeta| m.agent.as_ref().map(|a| (a.kind, a.state, a.seen));
        self.name == other.name
            && self.progress == other.progress
            && self.mouse_grabbed == other.mouse_grabbed
            && agent(self) == agent(other)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct ScreenState {
    pub cols: u16,
    pub rows: u16,
    pub alt: bool,
    /// Stable row index of the top visible row.
    pub top: isize,
    /// Rows held, scrollback included.
    pub held: usize,
    pub cursor_x: usize,
    pub cursor_y: i64,
    pub cursor_visible: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct AgentReport {
    pub kind: AgentKind,
    pub state: AgentState,
    /// How long the state has held, in milliseconds.
    pub held_ms: u64,
    pub seen: bool,
}

pub fn write_frame<T: Serialize>(out: &mut impl Write, msg: &T) -> std::io::Result<()> {
    out.write_all(&encode(msg)?)?;
    out.flush()
}

pub fn encode<T: Serialize>(msg: &T) -> std::io::Result<Vec<u8>> {
    let body = bincode::serialize(msg).map_err(std::io::Error::other)?;
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// `None` at a clean end of stream.
pub fn read_frame<T: DeserializeOwned>(input: &mut impl Read) -> std::io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other("oversized frame"));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body)?;
    bincode::deserialize(&body)
        .map(Some)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_back_to_back() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &HubMsg::Paste {
                tab: TabRef::Token(7),
                text: "hi".into(),
            },
        )
        .unwrap();
        write_frame(&mut buf, &HubMsg::Seen(TabRef::Id(3))).unwrap();
        let mut input = buf.as_slice();
        match read_frame::<HubMsg>(&mut input).unwrap() {
            Some(HubMsg::Paste { tab, text }) => {
                assert_eq!(tab, TabRef::Token(7));
                assert_eq!(text, "hi");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            read_frame::<HubMsg>(&mut input).unwrap(),
            Some(HubMsg::Seen(TabRef::Id(3)))
        ));
        assert!(read_frame::<HubMsg>(&mut input).unwrap().is_none());
    }

    #[test]
    fn a_truncated_frame_is_an_error() {
        let mut buf = encode(&HubMsg::Seen(TabRef::Id(1))).unwrap();
        buf.pop();
        assert!(read_frame::<HubMsg>(&mut buf.as_slice()).is_err());
    }
}
