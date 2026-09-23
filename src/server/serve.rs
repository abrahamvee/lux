//! Serving an adopting hub: this host's own sessions, streamed over a
//! `lux proxy` connection.

use std::collections::HashMap;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Instant;

use ratatui::layout::Rect;
use serde::Serialize;

use crate::server::window::{self, Notice, Synced, Tab, TabId};
use crate::server::wire::{self, HubMsg, RemoteMsg, TabRef};
use crate::server::{ConnId, Server, SessionId};

/// Updates wait while more than this is queued for the hub, and pile up
/// as changed rows in the engines instead.
const BACKLOG_LIMIT: usize = 1 << 20;

/// Dropping it closes the connection once everything queued is written.
pub struct Hub {
    conn: ConnId,
    out: Sender<Vec<u8>>,
    backlog: Arc<AtomicUsize>,
    /// Set once the handshake passes; until then nothing is sent.
    adopted: bool,
    /// Tabs the hub spawned, by the token it chose.
    tokens: HashMap<u64, TabId>,
    synced: HashMap<TabId, Synced>,
}

impl Hub {
    fn send<T: Serialize>(&self, msg: &T) {
        if let Ok(frame) = wire::encode(msg) {
            self.backlog.fetch_add(frame.len(), Ordering::Relaxed);
            let _ = self.out.send(frame);
        }
    }

    fn resolve(&self, tab: TabRef) -> Option<TabId> {
        match tab {
            TabRef::Id(id) => Some(id),
            TabRef::Token(token) => self.tokens.get(&token).copied(),
        }
    }
}

impl Server {
    pub(super) fn hub_attach(&mut self, conn: ConnId, stream: UnixStream) {
        let Ok(mut writer) = stream.try_clone() else {
            return;
        };
        let (out, frames) = mpsc::channel::<Vec<u8>>();
        let backlog = Arc::new(AtomicUsize::new(0));
        let drained = backlog.clone();
        thread::spawn(move || {
            for frame in frames {
                if writer.write_all(&frame).is_err() {
                    break;
                }
                drained.fetch_sub(frame.len(), Ordering::Relaxed);
            }
            let _ = writer.shutdown(Shutdown::Both);
        });
        drop(stream);
        let hub = Hub {
            conn,
            out,
            backlog,
            adopted: false,
            tokens: HashMap::new(),
            synced: HashMap::new(),
        };
        // A hub that never finishes its handshake leaves the adopted one
        // in place, and one mid-handshake gives way.
        if self.hub.as_ref().is_none_or(|h| !h.adopted) {
            self.hub = Some(hub);
        } else {
            self.pending_hubs.push(hub);
        }
    }

    pub(super) fn hub_gone(&mut self, conn: ConnId) {
        self.pending_hubs.retain(|h| h.conn != conn);
        if self.hub.as_ref().is_some_and(|h| h.conn == conn) {
            self.hub = None;
            window::set_hub_colors(None);
        }
    }

    /// Whether a hub holds this host's sessions, which locks out local
    /// attaches.
    pub(super) fn adopted(&self) -> bool {
        self.hub.as_ref().is_some_and(|h| h.adopted)
    }

    pub(super) fn hub_msg(&mut self, conn: ConnId, msg: HubMsg) {
        if let HubMsg::Hello { version, instance } = msg {
            self.hub_hello(conn, version, instance);
            return;
        }
        if !self
            .hub
            .as_ref()
            .is_some_and(|h| h.conn == conn && h.adopted)
        {
            return;
        }
        match msg {
            HubMsg::Hello { .. } => {}
            HubMsg::Key { tab, code, mods } => {
                if let Some(tab) = self.served_tab(tab) {
                    tab.key_down(code, mods);
                }
            }
            HubMsg::Paste { tab, text } => {
                if let Some(tab) = self.served_tab(tab) {
                    tab.send_paste(&text);
                }
            }
            HubMsg::Mouse { tab, event } => {
                if let Some(tab) = self.served_tab(tab) {
                    tab.mouse_event(event);
                }
            }
            HubMsg::Resize { tab, cols, rows } => {
                if let Some(tab) = self.served_tab(tab) {
                    tab.resize(Rect::new(0, 0, cols, rows));
                }
            }
            HubMsg::Spawn {
                token,
                session,
                cwd_of,
                cols,
                rows,
            } => self.hub_spawn(token, session, cwd_of, cols, rows),
            HubMsg::Kill(tab) => {
                if let Some(tab) = self.served_tab(tab) {
                    tab.kill();
                }
            }
            HubMsg::SetName { tab, name } => {
                let osc_titles = self.config.osc_titles;
                if let Some(tab) = self.served_tab(tab) {
                    match name {
                        Some(name) => tab.set_name(name),
                        None => tab.clear_name(osc_titles),
                    }
                }
            }
            HubMsg::Seen(tab) => {
                if let Some(tab) = self.served_tab(tab) {
                    tab.mark_seen();
                }
            }
            HubMsg::Layout { session, layout } => self.hub_layout(session, &layout),
            HubMsg::NewSession {
                request,
                name,
                cols,
                rows,
            } => self.hub_new_session(request, name, cols, rows),
            HubMsg::RenameSession { session, name } => {
                if !name.contains('@')
                    && self.session_by_name(&name).is_none()
                    && let Some(s) = self.served_session(session)
                {
                    s.name = name;
                }
            }
            HubMsg::KillSession(session) => {
                if self.served_session(session).is_some() {
                    self.end_session(session);
                }
            }
            HubMsg::Colors(colors) => window::set_hub_colors(Some(&colors)),
            HubMsg::Resync(tab) => {
                let now = Instant::now();
                let Some(t) = self.served_tab(TabRef::Id(tab)) else {
                    return;
                };
                let (snap, base) = t.wire_snapshot(now);
                if let Some(hub) = self.hub.as_mut() {
                    hub.synced.insert(tab, base);
                    hub.send(&RemoteMsg::Full { tab, snap });
                }
            }
        }
    }

    fn hub_hello(&mut self, conn: ConnId, version: String, instance: u64) {
        let hub = if self.hub.as_ref().is_some_and(|h| h.conn == conn) {
            self.hub.take()
        } else {
            let pos = self.pending_hubs.iter().position(|h| h.conn == conn);
            pos.map(|pos| self.pending_hubs.remove(pos))
        };
        let Some(mut hub) = hub else {
            return;
        };
        let ours = wire::version();
        if instance == self.instance {
            hub.send(&RemoteMsg::Refused("that is the hub itself".into()));
            return;
        }
        // The hub reports the mismatch; this side just stays out.
        if version != ours {
            hub.send(&RemoteMsg::Hello {
                version: ours,
                instance: self.instance,
            });
            return;
        }
        if let Some(old) = self.hub.take() {
            old.send(&RemoteMsg::Evicted);
            window::set_hub_colors(None);
        }
        let conns: Vec<ConnId> = self.clients.keys().copied().collect();
        for conn in conns {
            self.detach(conn);
        }
        hub.adopted = true;
        hub.send(&RemoteMsg::Hello {
            version: ours,
            instance: self.instance,
        });
        let now = Instant::now();
        let mut snaps = Vec::new();
        for (&sid, session) in self.sessions.iter_mut().filter(|(_, s)| !s.is_remote()) {
            let (snap, synced) = session.remote_snapshot(sid, now);
            hub.synced.extend(synced);
            snaps.push(snap);
        }
        hub.send(&RemoteMsg::Sessions(snaps));
        self.hub = Some(hub);
    }

    /// A local session's tab, by the hub's address for it.
    fn served_tab(&mut self, tab: TabRef) -> Option<&mut Tab> {
        let id = self.hub.as_ref()?.resolve(tab)?;
        self.sessions
            .values_mut()
            .filter(|s| !s.is_remote())
            .find_map(|s| s.find_tab_mut(id))
    }

    fn served_session(&mut self, sid: SessionId) -> Option<&mut crate::server::session::Session> {
        self.sessions.get_mut(&sid).filter(|s| !s.is_remote())
    }

    fn hub_spawn(
        &mut self,
        token: u64,
        session: SessionId,
        cwd_of: Option<TabRef>,
        cols: u16,
        rows: u16,
    ) {
        let cwd = cwd_of
            .and_then(|r| self.served_tab(r))
            .and_then(|t| t.working_dir());
        let rect = Rect::new(0, 0, cols, rows);
        let tx = self.tx.clone();
        let spawned = self
            .served_session(session)
            .map(|_| Tab::spawn(rect, cwd, tx));
        let now = Instant::now();
        match spawned {
            Some(Ok(mut tab)) => {
                let (snap, base) = tab.wire_snapshot(now);
                let id = tab.id;
                if let Some(s) = self.served_session(session) {
                    s.adopt_tab(tab);
                }
                if let Some(hub) = self.hub.as_mut() {
                    hub.tokens.insert(token, id);
                    hub.synced.insert(id, base);
                    hub.send(&RemoteMsg::Spawned { token, tab: snap });
                }
            }
            _ => {
                if let Some(hub) = self.hub.as_ref() {
                    hub.send(&RemoteMsg::SpawnFailed(token));
                }
            }
        }
    }

    /// Rearranges a session as the hub laid it out, pulling in tabs the
    /// hub moved here from other sessions.
    fn hub_layout(&mut self, sid: SessionId, layout: &wire::LayoutSnap) {
        let Some(hub) = self.hub.as_ref() else {
            return;
        };
        let wanted: Vec<TabId> = layout
            .windows
            .iter()
            .flat_map(|w| &w.tabs)
            .filter_map(|&r| hub.resolve(r))
            .collect();
        let resolve = {
            let tokens = hub.tokens.clone();
            move |r: TabRef| match r {
                TabRef::Id(id) => Some(id),
                TabRef::Token(token) => tokens.get(&token).copied(),
            }
        };
        if self.served_session(sid).is_none() {
            return;
        }
        let mut pool = HashMap::new();
        let mut emptied = Vec::new();
        for id in wanted {
            if self.sessions[&sid].has_tab(id) {
                continue;
            }
            let from = self
                .sessions
                .iter_mut()
                .filter(|(_, s)| !s.is_remote())
                .find(|(_, s)| s.has_tab(id));
            if let Some((&other, session)) = from
                && let Some((tab, ended)) = session.extract_tab(id)
            {
                pool.insert(id, tab);
                if ended {
                    emptied.push(other);
                }
            }
        }
        let left = self
            .sessions
            .get_mut(&sid)
            .is_some_and(|s| s.relayout(layout, resolve, pool));
        if !left {
            emptied.push(sid);
        }
        for sid in emptied {
            self.end_session(sid);
        }
    }

    fn hub_new_session(&mut self, request: u64, name: Option<String>, cols: u16, rows: u16) {
        let area = Rect::new(0, 0, cols, rows);
        let created = match &name {
            Some(n) if n.contains('@') => Err("session names cannot contain '@'".to_string()),
            Some(n) if self.session_by_name(n).is_some() => {
                Err(format!("a session named '{n}' exists"))
            }
            _ => self.create_session(name, area),
        };
        let now = Instant::now();
        let msg = match created {
            Ok(sid) => {
                let session = self.sessions.get_mut(&sid).expect("just created");
                let (snap, synced) = session.remote_snapshot(sid, now);
                if let Some(hub) = self.hub.as_mut() {
                    hub.synced.extend(synced);
                }
                RemoteMsg::SessionAdded {
                    request: Some(request),
                    session: snap,
                }
            }
            Err(reason) => RemoteMsg::NewSessionFailed { request, reason },
        };
        if let Some(hub) = self.hub.as_ref() {
            hub.send(&msg);
        }
    }

    /// Sends what changed in every served tab, unless the hub is behind.
    pub(super) fn pump_hub(&mut self) {
        let Some(hub) = self.hub.as_mut().filter(|h| h.adopted) else {
            return;
        };
        if hub.backlog.load(Ordering::Relaxed) > BACKLOG_LIMIT {
            return;
        }
        let now = Instant::now();
        for session in self.sessions.values_mut().filter(|s| !s.is_remote()) {
            for tab in session.tabs_mut() {
                let Some(synced) = hub.synced.get_mut(&tab.id) else {
                    continue;
                };
                if let Some(update) = tab.wire_update(synced, now) {
                    hub.send(&RemoteMsg::Update {
                        tab: tab.id,
                        update,
                    });
                }
            }
        }
    }

    /// For a served tab's exit, after the session dealt with it.
    pub(super) fn hub_tab_exited(&mut self, tab: TabId) {
        if let Some(hub) = self.hub.as_mut().filter(|h| h.adopted)
            && hub.synced.remove(&tab).is_some()
        {
            hub.send(&RemoteMsg::Exited(tab));
        }
    }

    pub(super) fn hub_session_ended(&mut self, sid: SessionId) {
        if let Some(hub) = self.hub.as_ref().filter(|h| h.adopted) {
            hub.send(&RemoteMsg::SessionEnded(sid));
        }
    }

    /// Passes a served tab's notification to the hub. Returns whether it
    /// went there.
    pub(super) fn hub_notice(&mut self, notice: &Notice) -> bool {
        let Some(hub) = self.hub.as_ref().filter(|h| h.adopted) else {
            return false;
        };
        if !hub.synced.contains_key(&notice.id) {
            return false;
        }
        hub.send(&RemoteMsg::Notice {
            tab: notice.id,
            blocked: notice.blocked,
            summary: notice.summary.clone(),
        });
        true
    }

    /// Passes a served tab's clipboard write to the hub. Returns whether
    /// it went there.
    pub(super) fn hub_clipboard(&mut self, tab: TabId, text: &str) -> bool {
        let Some(hub) = self.hub.as_ref().filter(|h| h.adopted) else {
            return false;
        };
        if !hub.synced.contains_key(&tab) {
            return false;
        }
        hub.send(&RemoteMsg::Clipboard {
            tab,
            text: text.to_string(),
        });
        true
    }
}
