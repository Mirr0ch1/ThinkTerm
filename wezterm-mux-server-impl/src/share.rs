//! Sharing a pane with other accounts on this machine.
//!
//! A share names one pane and who may see it: accounts, or every account
//! in a group. While any share exists the server listens on a second
//! socket, `/tmp/thinkterm-share-<uid>/share.sock`, which every account on
//! the machine can reach. Who is at the other end of a connection there is
//! read from the socket itself, so nothing the client says about itself is
//! believed, and a connection from an account no share is for is closed
//! as soon as it is made. A viewer may read the shared pane's rows and
//! nothing else, and may type into it only while the owner lets it, one
//! viewer at a time. With no share there is no socket, no thread and no
//! work.
//!
//! The pane's rows reach a viewer the way they reach any client: one more
//! connection the server pushes changes to. Nothing here touches the
//! pane's output path.

use crate::sessionhandler::PduSender;
use anyhow::anyhow;
#[cfg(unix)]
use anyhow::Context as _;
use codec::{
    DecodedPdu, IncomingShare, Pdu, ShareAudience, ShareController, ShareIncomingResponse,
    ShareInfo, ShareJoinResponse, ShareStart, ShareStateChanged, ShareViewer,
};
use mux::pane::PaneId;
use mux::Mux;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use wezterm_term::StableRowIndex;

/// Shares one server holds at once.
const MAX_SHARES: usize = 32;
/// Viewers of one share at once.
pub const MAX_VIEWERS: usize = 20;
/// Viewers of one share from the same account at once.
const MAX_VIEWERS_PER_ACCOUNT: usize = 3;
/// Connections on the share socket at once, and from one account.
const MAX_CONNECTIONS: usize = 64;
const MAX_CONNECTIONS_PER_ACCOUNT: usize = 4;
/// Accounts and groups one share may name.
const MAX_AUDIENCE: usize = 64;
/// The longest a right to type may be given for.
const MAX_DURATION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// The longest a share lasts, in seconds: one forgotten about ends.
const SHARE_FOR_AT_MOST: u64 = 30 * 60;
/// Rows one viewer may read in one ask: screens of several hundred rows,
/// more than a viewer asks for at once, and no more than that.
const MAX_ROWS_PER_ASK: StableRowIndex = 2000;
/// A viewer asking to type again this soon is not heard again.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(5);
/// How often expiries, lapsed typing rights and closed panes are noticed.
const TICK: Duration = Duration::from_secs(2);
/// The audit log is started afresh past this, keeping one older file.
const LOG_LIMIT: u64 = 1024 * 1024;

/// A connection on the share socket, as the kernel identified it.
#[derive(Debug, Clone)]
pub struct SharePeer {
    pub uid: u32,
    pub name: String,
    /// The account's primary group, which a group share also admits.
    pub gid: u32,
    /// Closed to drop the connection: when its viewer is removed, or its
    /// share ends.
    pub dropped: smol::channel::Receiver<()>,
    dropper: smol::channel::Sender<()>,
}

impl SharePeer {
    pub fn new(uid: u32, name: String, gid: u32) -> Self {
        let (dropper, dropped) = smol::channel::bounded(1);
        Self {
            uid,
            name,
            gid,
            dropped,
            dropper,
        }
    }
}

struct Viewer {
    name: String,
    uid: u32,
    since: SystemTime,
    sender: PduSender,
    dropper: smol::channel::Sender<()>,
    /// When it last asked to type.
    asked: Option<Instant>,
}

struct Controller {
    viewer_id: u64,
    until: Option<SystemTime>,
}

/// The connection a person shared panes from: their desktop's, to this
/// server. What it shared ends when it closes -- ThinkTerm quit, or lost
/// its way here -- so nobody goes on watching or typing once they have
/// gone.
#[derive(Clone)]
pub struct Sharer {
    connected: Arc<AtomicBool>,
}

/// A connection still open.
impl Default for Sharer {
    fn default() -> Self {
        Self {
            connected: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl Sharer {
    /// Its connection closed: end what it shared.
    pub fn left(&self) {
        self.connected.store(false, Ordering::SeqCst);
        let mut registry = REGISTRY.lock().unwrap();
        let shared: Vec<u64> = registry
            .shares
            .iter()
            .filter(|(_, share)| Arc::ptr_eq(&share.sharer.connected, &self.connected))
            .map(|(id, _)| *id)
            .collect();
        for share_id in shared {
            let _ = registry.end(share_id, "owner left", codec::share_end::STOPPED);
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }
}

struct Share {
    pane_id: PaneId,
    /// Where it was started; it ends with that.
    sharer: Sharer,
    audience: Vec<ShareAudience>,
    /// Whether viewers may read the pane's history, or only its screen.
    history: bool,
    started: SystemTime,
    expires: Option<SystemTime>,
    viewers: BTreeMap<u64, Viewer>,
    controller: Option<Controller>,
    /// Viewers asking to type, oldest first.
    requests: Vec<u64>,
    /// Accounts the owner removed, which may not come back to this share.
    removed: HashSet<u32>,
}

#[derive(Default)]
struct Registry {
    shares: BTreeMap<u64, Share>,
    next_share_id: u64,
    next_viewer_id: u64,
    #[cfg(unix)]
    listener: Option<listener::Listener>,
    ticking: bool,
}

lazy_static::lazy_static! {
    static ref REGISTRY: Mutex<Registry> = Mutex::new(Registry::default());
    /// Connections open on the share socket, by account.
    static ref CONNECTIONS: Mutex<HashMap<u32, usize>> = Mutex::new(HashMap::new());
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `secs` from now, for a share or a right to type; refused past
/// `MAX_DURATION`.
fn after(secs: u64) -> anyhow::Result<SystemTime> {
    let span = Duration::from_secs(secs);
    anyhow::ensure!(
        span <= MAX_DURATION,
        "at most {} days at a time",
        MAX_DURATION.as_secs() / 86400
    );
    SystemTime::now()
        .checked_add(span)
        .ok_or_else(|| anyhow!("{secs} seconds from now is past what this clock can say"))
}

/// Whether an account name, or a group name, can be looked up as given:
/// the characters accounts are made of, nothing a path or a shell would
/// read otherwise.
#[cfg_attr(not(unix), allow(dead_code))]
fn plausible_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '$' | '@'))
}

/// Whether `peer` is one of the accounts `audience` names. The owner always
/// is: sharing with yourself is how a second screen watches. A group is
/// looked up at each check, so an account added to it while the share runs
/// is let in and one taken out is kept out; the lookup may be slow (a
/// directory server), so no lock is held across it.
fn admitted(audience: &[ShareAudience], peer: &SharePeer) -> bool {
    if peer.uid == own_uid() {
        return true;
    }
    audience.iter().any(|who| match who {
        ShareAudience::User(name) => *name == peer.name,
        ShareAudience::Group(group) => group_has(group, peer),
    })
}

impl Share {
    fn info(&self, share_id: u64) -> ShareInfo {
        let viewer = |viewer_id: u64| {
            self.viewers.get(&viewer_id).map(|v| ShareViewer {
                viewer_id,
                name: v.name.clone(),
                since: unix_secs(v.since),
            })
        };
        ShareInfo {
            share_id,
            pane_id: self.pane_id,
            audience: self.audience.clone(),
            history: self.history,
            started: unix_secs(self.started),
            expires: self.expires.map(unix_secs),
            viewers: self.viewers.keys().filter_map(|id| viewer(*id)).collect(),
            controller: self.controller.as_ref().and_then(|c| {
                self.viewers.get(&c.viewer_id).map(|v| ShareController {
                    viewer_id: c.viewer_id,
                    name: v.name.clone(),
                    until: c.until.map(unix_secs),
                })
            }),
            requests: self.requests.iter().filter_map(|id| viewer(*id)).collect(),
        }
    }

    fn controller_name(&self) -> Option<String> {
        self.controller
            .as_ref()
            .and_then(|c| self.viewers.get(&c.viewer_id))
            .map(|v| v.name.clone())
    }

    fn is_controller(&self, viewer_id: u64) -> bool {
        self.controller
            .as_ref()
            .is_some_and(|c| c.viewer_id == viewer_id)
    }

    /// Tell every viewer where typing stands now.
    fn announce(&self, share_id: u64) {
        let controller = self.controller_name();
        for (viewer_id, viewer) in &self.viewers {
            let state = ShareStateChanged {
                share_id,
                controlling: self.is_controller(*viewer_id),
                requested: self.requests.contains(viewer_id),
                controller: controller.clone(),
                ended: None,
            };
            push(&viewer.sender, state);
        }
    }
}

fn push(sender: &PduSender, state: ShareStateChanged) {
    let _ = sender.send(DecodedPdu {
        serial: 0,
        pdu: Pdu::ShareStateChanged(state),
    });
}

/// End a viewer's watch: say why, then drop its connection.
fn dismiss(share_id: u64, viewer: Viewer, reason: &str) {
    push(
        &viewer.sender,
        ShareStateChanged {
            share_id,
            controlling: false,
            requested: false,
            controller: None,
            ended: Some(reason.to_string()),
        },
    );
    viewer.dropper.close();
}

#[cfg(unix)]
fn own_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

#[cfg(not(unix))]
fn own_uid() -> u32 {
    u32::MAX
}

/// The account this server runs as.
pub fn own_name() -> String {
    #[cfg(unix)]
    {
        if let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::geteuid()) {
            return user.name;
        }
    }
    std::env::var("USER").unwrap_or_default()
}

#[cfg(unix)]
fn group_has(group: &str, peer: &SharePeer) -> bool {
    match nix::unistd::Group::from_name(group) {
        Ok(Some(group)) => group.gid.as_raw() == peer.gid || group.mem.contains(&peer.name),
        _ => false,
    }
}

#[cfg(not(unix))]
fn group_has(_group: &str, _peer: &SharePeer) -> bool {
    false
}

/// `who`, as the machine spells it: a lookup may take another case, or
/// another of an account's names (macOS does), and a viewer is told
/// apart by the name the machine gives it.
#[cfg(unix)]
fn check_audience(who: &ShareAudience) -> anyhow::Result<ShareAudience> {
    Ok(match who {
        ShareAudience::User(name) => {
            anyhow::ensure!(plausible_name(name), "`{name}` is not an account name");
            let user = nix::unistd::User::from_name(name)
                .with_context(|| format!("looking up the account `{name}`"))?
                .ok_or_else(|| anyhow!("there is no account `{name}` on this machine"))?;
            ShareAudience::User(user.name)
        }
        ShareAudience::Group(name) => {
            anyhow::ensure!(plausible_name(name), "`{name}` is not a group name");
            let group = nix::unistd::Group::from_name(name)
                .with_context(|| format!("looking up the group `{name}`"))?
                .ok_or_else(|| anyhow!("there is no group `{name}` on this machine"))?;
            ShareAudience::Group(group.name)
        }
    })
}

#[cfg(not(unix))]
fn check_audience(_who: &ShareAudience) -> anyhow::Result<ShareAudience> {
    anyhow::bail!("sharing terminals is not supported on Windows yet")
}

/// How long a share asked to last `expires_in_secs` lasts: half an hour
/// at most.
fn share_for(expires_in_secs: Option<u64>) -> u64 {
    expires_in_secs.map_or(SHARE_FOR_AT_MOST, |secs| secs.min(SHARE_FOR_AT_MOST))
}

/// Start a share, for as long as `sharer` stays. Runs on a thread of its
/// own: the accounts it names are looked up, maybe in a directory slow to
/// answer.
pub fn start(request: ShareStart, sharer: Sharer) -> anyhow::Result<ShareInfo> {
    // What other accounts see is a person's to decide: a program in a pane
    // -- an agent -- cannot share it.
    anyhow::ensure!(
        request.from_desktop,
        "a pane is shared from the share button in ThinkTerm"
    );
    anyhow::ensure!(
        !request.audience.is_empty(),
        "say who the pane is for: an account, or a group"
    );
    anyhow::ensure!(
        request.audience.len() <= MAX_AUDIENCE,
        "a share names at most {MAX_AUDIENCE} accounts and groups"
    );
    let mut audience = vec![];
    for who in &request.audience {
        let who = check_audience(who)?;
        if !audience.contains(&who) {
            audience.push(who);
        }
    }
    anyhow::ensure!(
        Mux::get().get_pane(request.pane_id).is_some(),
        "no such pane {}",
        request.pane_id
    );
    let expires = after(share_for(request.expires_in_secs))?;
    let share = Share {
        pane_id: request.pane_id,
        sharer,
        audience,
        // Only what is on the pane's screen: what came before is not
        // shared, whatever the request says.
        history: false,
        started: SystemTime::now(),
        expires: Some(expires),
        viewers: BTreeMap::new(),
        controller: None,
        requests: vec![],
        removed: HashSet::new(),
    };

    let mut registry = REGISTRY.lock().unwrap();
    // Asked for, then gone while the accounts were looked up: under the
    // lock its leaving takes, so the share is never left behind.
    anyhow::ensure!(
        share.sharer.is_connected(),
        "the window that asked to share has gone"
    );
    // A pane is shared once: one panel shows its share, and stopping that
    // leaves nobody watching through another.
    if let Some(share_id) = registry.share_of(request.pane_id) {
        anyhow::bail!(
            "pane {} is already shared (share {share_id}); stop that share first",
            request.pane_id
        );
    }
    anyhow::ensure!(
        registry.shares.len() < MAX_SHARES,
        "this server already holds {MAX_SHARES} shares; stop one first"
    );
    registry.ensure_listener()?;
    registry.next_share_id += 1;
    let share_id = registry.next_share_id;
    let info = share.info(share_id);
    audit(
        "started",
        share_id,
        share.pane_id,
        None,
        &serde_json::json!({
            "audience": share.audience.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "history": share.history,
            "expires": share.expires.map(unix_secs),
        }),
    );
    registry.shares.insert(share_id, share);
    registry.ensure_ticking();
    Ok(info)
}

pub fn list() -> Vec<ShareInfo> {
    let registry = REGISTRY.lock().unwrap();
    registry
        .shares
        .iter()
        .map(|(id, share)| share.info(*id))
        .collect()
}

pub fn stop(share_id: u64) -> anyhow::Result<()> {
    let mut registry = REGISTRY.lock().unwrap();
    registry.end(share_id, "stopped", codec::share_end::STOPPED)
}

/// Drop a viewer, and keep its account out of this share from now on.
pub fn kick(share_id: u64, viewer_id: u64) -> anyhow::Result<()> {
    let mut registry = REGISTRY.lock().unwrap();
    let share = registry.share_mut(share_id)?;
    let viewer = share
        .viewers
        .remove(&viewer_id)
        .ok_or_else(|| anyhow!("no viewer {viewer_id} in share {share_id}"))?;
    if viewer.uid != own_uid() {
        share.removed.insert(viewer.uid);
    }
    // The account's other connections to this share go with it.
    let same_account: Vec<u64> = share
        .viewers
        .iter()
        .filter(|(_, v)| v.uid == viewer.uid && viewer.uid != own_uid())
        .map(|(id, _)| *id)
        .collect();
    let mut dropped = vec![(viewer_id, viewer)];
    for id in same_account {
        if let Some(other) = share.viewers.remove(&id) {
            dropped.push((id, other));
        }
    }
    let mut had_control = false;
    for (id, viewer) in dropped {
        had_control |= share.is_controller(id);
        share.requests.retain(|asking| *asking != id);
        audit(
            "removed",
            share_id,
            share.pane_id,
            Some(&viewer.name),
            &serde_json::json!({}),
        );
        dismiss(share_id, viewer, codec::share_end::REMOVED);
    }
    if had_control {
        share.controller = None;
        share.announce(share_id);
    }
    Ok(())
}

/// Let a viewer type. A person decides that too: only the desktop's
/// share panel asks.
pub fn grant(
    share_id: u64,
    viewer_id: u64,
    for_secs: Option<u64>,
    from_desktop: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        from_desktop,
        "a viewer is let type from the share panel in ThinkTerm"
    );
    let until = for_secs.map(after).transpose()?;
    let mut registry = REGISTRY.lock().unwrap();
    let share = registry.share_mut(share_id)?;
    let name = share
        .viewers
        .get(&viewer_id)
        .map(|v| v.name.clone())
        .ok_or_else(|| anyhow!("no viewer {viewer_id} in share {share_id}"))?;
    share.controller = Some(Controller { viewer_id, until });
    share.requests.retain(|id| *id != viewer_id);
    audit(
        "may type",
        share_id,
        share.pane_id,
        Some(&name),
        &serde_json::json!({ "for_secs": for_secs }),
    );
    share.announce(share_id);
    Ok(())
}

/// Turn down a request to type; the viewer goes on watching.
pub fn deny(share_id: u64, viewer_id: u64) -> anyhow::Result<()> {
    let mut registry = REGISTRY.lock().unwrap();
    let share = registry.share_mut(share_id)?;
    if share.requests.contains(&viewer_id) {
        share.requests.retain(|id| *id != viewer_id);
        let name = share.viewers.get(&viewer_id).map(|v| v.name.clone());
        audit(
            "turned down",
            share_id,
            share.pane_id,
            name.as_deref(),
            &serde_json::json!({}),
        );
        share.announce(share_id);
    }
    Ok(())
}

pub fn revoke(share_id: u64) -> anyhow::Result<()> {
    let mut registry = REGISTRY.lock().unwrap();
    let share = registry.share_mut(share_id)?;
    if let Some(controller) = share.controller.take() {
        let name = share
            .viewers
            .get(&controller.viewer_id)
            .map(|v| v.name.clone());
        audit(
            "typing taken back",
            share_id,
            share.pane_id,
            name.as_deref(),
            &serde_json::json!({}),
        );
        share.announce(share_id);
    }
    Ok(())
}

/// What a share looks like before deciding whether a peer may see it.
struct Candidate {
    share_id: u64,
    pane_id: PaneId,
    audience: Vec<ShareAudience>,
    history: bool,
    started: SystemTime,
    controller: Option<String>,
}

/// The shares `peer` might see: copied out, so that whether it may is
/// decided with no lock held.
fn candidates(peer: &SharePeer) -> Vec<Candidate> {
    let registry = REGISTRY.lock().unwrap();
    registry
        .shares
        .iter()
        .filter(|(_, share)| !share.removed.contains(&peer.uid))
        .map(|(id, share)| Candidate {
            share_id: *id,
            pane_id: share.pane_id,
            audience: share.audience.clone(),
            history: share.history,
            started: share.started,
            controller: share.controller_name(),
        })
        .collect()
}

/// At most this many accounts, and as many groups, are offered to pick
/// from.
const MAX_CANDIDATES: usize = 500;

/// The accounts and groups of this machine a pane can be shared with:
/// people's, not the system's own.
pub fn candidates_to_share_with() -> codec::ShareCandidatesResponse {
    let (mut accounts, mut groups) = people_and_groups();
    accounts.sort();
    accounts.dedup();
    accounts.truncate(MAX_CANDIDATES);
    groups.sort();
    groups.dedup();
    groups.truncate(MAX_CANDIDATES);
    let me = own_name();
    if !me.is_empty() && !accounts.contains(&me) {
        accounts.insert(0, me.clone());
    }
    codec::ShareCandidatesResponse {
        me,
        accounts,
        groups,
    }
}

/// From `/etc/passwd` and `/etc/group`: accounts from uid 1000, with a
/// shell to log in with, and groups from gid 1000. Accounts a directory
/// service holds are not listed there, and are not offered.
#[cfg(all(unix, not(target_os = "macos")))]
fn people_and_groups() -> (Vec<String>, Vec<String>) {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let accounts = read("/etc/passwd")
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let uid: u32 = fields.get(2)?.parse().ok()?;
            let shell = fields.get(6).copied().unwrap_or("");
            let logs_in = !shell.ends_with("nologin") && !shell.ends_with("false");
            (uid >= 1000 && uid != 65534 && logs_in).then(|| fields[0].to_string())
        })
        .collect();
    let groups = read("/etc/group")
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let gid: u32 = fields.get(2)?.parse().ok()?;
            (gid >= 1000 && gid != 65534).then(|| fields[0].to_string())
        })
        .collect();
    (accounts, groups)
}

/// From the directory service: accounts and groups from id 500, without
/// the system's own: names starting with an underscore, and the groups
/// macOS makes for its sharing services (`com.apple.sharepoint.group.1`).
#[cfg(target_os = "macos")]
fn people_and_groups() -> (Vec<String>, Vec<String>) {
    let list = |kind: &str, id: &str| -> Vec<String> {
        let output = std::process::Command::new("/usr/bin/dscl")
            .args([".", "-list", kind, id])
            .output();
        let Ok(output) = output else {
            return vec![];
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let name = words.next()?;
                let id: i64 = words.next()?.parse().ok()?;
                let system = name.starts_with('_') || name.starts_with("com.apple.");
                (id >= 500 && !system).then(|| name.to_string())
            })
            .collect()
    };
    (
        list("/Users", "UniqueID"),
        list("/Groups", "PrimaryGroupID"),
    )
}

#[cfg(not(unix))]
fn people_and_groups() -> (Vec<String>, Vec<String>) {
    (vec![], vec![])
}

/// Whether any share is for `peer`: a connection from an account none is
/// for is closed at once.
pub fn admits_anywhere(peer: &SharePeer) -> bool {
    candidates(peer)
        .iter()
        .any(|share| admitted(&share.audience, peer))
}

/// The shares `peer` may watch, with their titles read without waiting
/// on the panes.
pub fn incoming(peer: &SharePeer) -> ShareIncomingResponse {
    let mux = Mux::get();
    ShareIncomingResponse {
        owner: own_name(),
        shares: candidates(peer)
            .into_iter()
            .filter(|share| admitted(&share.audience, peer))
            .map(|share| IncomingShare {
                share_id: share.share_id,
                pane_id: share.pane_id,
                title: mux
                    .get_pane(share.pane_id)
                    .map(|pane| pane.summary_without_waiting().title)
                    .unwrap_or_default(),
                history: share.history,
                started: unix_secs(share.started),
                controller: share.controller,
            })
            .collect(),
    }
}

/// Start `peer` watching a share. `sender` carries what the server tells
/// the viewer later; the connection ends when its share or its place in it
/// does.
pub fn join(
    peer: &SharePeer,
    share_id: u64,
    sender: PduSender,
) -> anyhow::Result<ShareJoinResponse> {
    let not_here = || anyhow!("no share {share_id} for you here");
    let audience = candidates(peer)
        .into_iter()
        .find(|share| share.share_id == share_id)
        .map(|share| share.audience)
        .ok_or_else(not_here)?;
    anyhow::ensure!(admitted(&audience, peer), not_here());

    let mut registry = REGISTRY.lock().unwrap();
    registry.next_viewer_id += 1;
    let viewer_id = registry.next_viewer_id;
    let share = registry
        .shares
        .get_mut(&share_id)
        .filter(|share| !share.removed.contains(&peer.uid))
        .ok_or_else(not_here)?;
    anyhow::ensure!(
        share.viewers.len() < MAX_VIEWERS,
        "share {share_id} already has {MAX_VIEWERS} viewers"
    );
    let from_account = share.viewers.values().filter(|v| v.uid == peer.uid).count();
    anyhow::ensure!(
        from_account < MAX_VIEWERS_PER_ACCOUNT,
        "you already watch share {share_id} {MAX_VIEWERS_PER_ACCOUNT} times"
    );
    share.viewers.insert(
        viewer_id,
        Viewer {
            name: peer.name.clone(),
            uid: peer.uid,
            since: SystemTime::now(),
            sender,
            dropper: peer.dropper.clone(),
            asked: None,
        },
    );
    audit(
        "joined",
        share_id,
        share.pane_id,
        Some(&peer.name),
        &serde_json::json!({}),
    );
    let response = ShareJoinResponse {
        share_id,
        pane_id: share.pane_id,
        viewer_id,
        history: share.history,
    };
    // Who types now, so a viewer arriving mid-way knows.
    share.announce(share_id);
    Ok(response)
}

/// A viewer asks to type. Asking again within `ASK_AGAIN_AFTER` is not
/// heard: one request is enough for the owner to see.
pub fn request_control(viewer_id: u64, share_id: u64) -> anyhow::Result<()> {
    let mut registry = REGISTRY.lock().unwrap();
    let share = registry.share_of_viewer(share_id, viewer_id)?;
    if share.is_controller(viewer_id) || share.requests.contains(&viewer_id) {
        return Ok(());
    }
    let Some(viewer) = share.viewers.get_mut(&viewer_id) else {
        return Ok(());
    };
    if viewer
        .asked
        .is_some_and(|at| at.elapsed() < ASK_AGAIN_AFTER)
    {
        return Ok(());
    }
    viewer.asked = Some(Instant::now());
    let name = viewer.name.clone();
    share.requests.push(viewer_id);
    audit(
        "asked to type",
        share_id,
        share.pane_id,
        Some(&name),
        &serde_json::json!({}),
    );
    share.announce(share_id);
    Ok(())
}

pub fn release_control(viewer_id: u64, share_id: u64) -> anyhow::Result<()> {
    let mut registry = REGISTRY.lock().unwrap();
    let share = registry.share_of_viewer(share_id, viewer_id)?;
    let controlling = share.is_controller(viewer_id);
    let requested = share.requests.contains(&viewer_id);
    if controlling {
        share.controller = None;
        let name = share.viewers.get(&viewer_id).map(|v| v.name.clone());
        audit(
            "gave typing back",
            share_id,
            share.pane_id,
            name.as_deref(),
            &serde_json::json!({}),
        );
    }
    share.requests.retain(|id| *id != viewer_id);
    if controlling || requested {
        share.announce(share_id);
    }
    Ok(())
}

/// A viewer's connection closed.
pub fn leave(viewer_id: u64, share_id: u64) {
    let mut registry = REGISTRY.lock().unwrap();
    let Some(share) = registry.shares.get_mut(&share_id) else {
        return;
    };
    let Some(viewer) = share.viewers.remove(&viewer_id) else {
        return;
    };
    let had_control = share.is_controller(viewer_id);
    if had_control {
        share.controller = None;
    }
    share.requests.retain(|id| *id != viewer_id);
    audit(
        "left",
        share_id,
        share.pane_id,
        Some(&viewer.name),
        &serde_json::json!({}),
    );
    if had_control {
        share.announce(share_id);
    }
}

/// Whether a viewer may read `pane_id`, and if so whether its history:
/// `Some(true)` for the whole pane, `Some(false)` for its screen only.
pub fn may_read(viewer_id: u64, share_id: u64, pane_id: PaneId) -> Option<bool> {
    let registry = REGISTRY.lock().unwrap();
    let share = registry.shares.get(&share_id)?;
    (share.pane_id == pane_id && share.viewers.contains_key(&viewer_id)).then_some(share.history)
}

/// Whether a viewer may type into `pane_id` now.
pub fn may_type(viewer_id: u64, share_id: u64, pane_id: PaneId) -> bool {
    let registry = REGISTRY.lock().unwrap();
    registry.shares.get(&share_id).is_some_and(|share| {
        share.pane_id == pane_id
            && share.controller.as_ref().is_some_and(|c| {
                c.viewer_id == viewer_id && c.until.is_none_or(|until| SystemTime::now() < until)
            })
    })
}

/// The rows a viewer asked for, as few ranges as hold them: overlapping
/// ones merged, and the earliest `MAX_ROWS_PER_ASK` rows of them. Asking
/// for the same rows over and over, or for all of a long history at once,
/// makes no work beyond that.
pub fn viewer_rows(mut ranges: Vec<Range<StableRowIndex>>) -> Vec<Range<StableRowIndex>> {
    ranges.retain(|range| range.start < range.end);
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<StableRowIndex>> = vec![];
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    let mut left = MAX_ROWS_PER_ASK;
    let mut rows = vec![];
    for range in merged {
        if left == 0 {
            break;
        }
        let take = range.end.saturating_sub(range.start).min(left);
        rows.push(range.start..range.start + take);
        left -= take;
    }
    rows
}

/// A place among the connections on the share socket, given back when the
/// connection ends.
pub struct ConnectionSlot {
    uid: u32,
}

impl ConnectionSlot {
    /// A place for one more connection from `uid`, if the socket has room
    /// for it.
    pub fn take(uid: u32) -> Option<Self> {
        let mut open = CONNECTIONS.lock().unwrap();
        let total: usize = open.values().sum();
        let mine = open.get(&uid).copied().unwrap_or(0);
        if total >= MAX_CONNECTIONS || mine >= MAX_CONNECTIONS_PER_ACCOUNT {
            return None;
        }
        open.insert(uid, mine + 1);
        Some(Self { uid })
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut open = CONNECTIONS.lock().unwrap();
        if let Some(count) = open.get_mut(&self.uid) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&self.uid);
            }
        }
    }
}

impl Registry {
    fn share_mut(&mut self, share_id: u64) -> anyhow::Result<&mut Share> {
        self.shares
            .get_mut(&share_id)
            .ok_or_else(|| anyhow!("no share {share_id}"))
    }

    /// The share of `pane_id`, while it has one.
    fn share_of(&self, pane_id: PaneId) -> Option<u64> {
        self.shares
            .iter()
            .find(|(_, share)| share.pane_id == pane_id)
            .map(|(share_id, _)| *share_id)
    }

    fn share_of_viewer(&mut self, share_id: u64, viewer_id: u64) -> anyhow::Result<&mut Share> {
        self.shares
            .get_mut(&share_id)
            .filter(|share| share.viewers.contains_key(&viewer_id))
            .ok_or_else(|| anyhow!("you are not watching share {share_id}"))
    }

    /// End a share: `event` for the log, `reason` for its viewers.
    fn end(&mut self, share_id: u64, event: &str, reason: &str) -> anyhow::Result<()> {
        let share = self
            .shares
            .remove(&share_id)
            .ok_or_else(|| anyhow!("no share {share_id}"))?;
        audit(event, share_id, share.pane_id, None, &serde_json::json!({}));
        for (_, viewer) in share.viewers {
            dismiss(share_id, viewer, reason);
        }
        if self.shares.is_empty() {
            self.close_listener();
        }
        Ok(())
    }

    #[cfg(unix)]
    fn ensure_listener(&mut self) -> anyhow::Result<()> {
        if self.listener.is_none() {
            self.listener = Some(listener::Listener::open()?);
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn ensure_listener(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("sharing terminals is not supported on Windows yet")
    }

    /// Closed here and now, the socket gone before the lock is let go: a
    /// share started next opens a socket of its own where this one was.
    fn close_listener(&mut self) {
        #[cfg(unix)]
        if let Some(listener) = self.listener.take() {
            listener.close();
        }
    }

    /// One thread notices what lapses with time, while there is anything
    /// to notice; it ends with the last share.
    fn ensure_ticking(&mut self) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        let spawned = std::thread::Builder::new()
            .name("share-tick".into())
            .spawn(|| loop {
                std::thread::sleep(TICK);
                if !tick() {
                    break;
                }
            });
        if let Err(err) = spawned {
            log::error!("cannot start the thread that ends lapsed shares: {err:#}");
            self.ticking = false;
        }
    }
}

/// End what has lapsed. False once there is nothing left to watch over,
/// which ends the thread.
fn tick() -> bool {
    let mux = Mux::try_get();
    let mut registry = REGISTRY.lock().unwrap();
    let now = SystemTime::now();
    let lapsed: Vec<(u64, &'static str, &'static str)> = registry
        .shares
        .iter()
        .filter_map(|(id, share)| {
            let pane_gone = mux
                .as_ref()
                .is_some_and(|mux| mux.get_pane(share.pane_id).is_none());
            if pane_gone {
                Some((*id, "pane closed", codec::share_end::CLOSED))
            } else if share.expires.is_some_and(|at| at <= now) {
                Some((*id, "expired", codec::share_end::EXPIRED))
            } else {
                None
            }
        })
        .collect();
    for (share_id, event, reason) in lapsed {
        let _ = registry.end(share_id, event, reason);
    }
    for (share_id, share) in registry.shares.iter_mut() {
        let lapsed = share
            .controller
            .as_ref()
            .and_then(|c| c.until)
            .is_some_and(|until| until <= now);
        if lapsed {
            share.controller = None;
            audit(
                "typing lapsed",
                *share_id,
                share.pane_id,
                None,
                &serde_json::json!({}),
            );
            share.announce(*share_id);
        }
    }
    if registry.shares.is_empty() {
        registry.ticking = false;
        return false;
    }
    true
}

/// The server is going away: end every share, telling its viewers, and
/// take the socket down so nobody finds it left behind.
pub fn shut_down() {
    let mut registry = REGISTRY.lock().unwrap();
    let ids: Vec<u64> = registry.shares.keys().copied().collect();
    for share_id in ids {
        let _ = registry.end(share_id, "server stopped", codec::share_end::STOPPED);
    }
    // The last share's end took the socket down; this is for one that
    // outlived its shares.
    registry.close_listener();
}

pub fn log_path() -> PathBuf {
    config::DATA_DIR.join("share-log.jsonl")
}

/// One line in the share log: who came and went, and who could type.
/// Tests leave the log alone: it is in the data directory of whoever runs
/// them.
fn audit(
    event: &str,
    share_id: u64,
    pane_id: PaneId,
    who: Option<&str>,
    detail: &serde_json::Value,
) {
    if cfg!(test) {
        return;
    }
    let line = serde_json::json!({
        "time": unix_secs(SystemTime::now()),
        "event": event,
        "share": share_id,
        "pane": pane_id,
        "who": who,
        "detail": detail,
    });
    if let Err(err) = append_log(&log_path(), &line.to_string()) {
        log::warn!("cannot write the share log: {err:#}");
    }
}

fn append_log(path: &Path, line: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > LOG_LIMIT) {
        let older = path.with_extension("1.jsonl");
        std::fs::rename(path, older)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

#[cfg(unix)]
mod listener {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
    use wezterm_uds::{UnixListener, UnixStream};

    pub struct Listener {
        path: PathBuf,
        closing: Arc<AtomicBool>,
    }

    /// Make the directory the socket goes in: ours, a real directory, and
    /// writable by nobody else, so nobody else can put a socket of their
    /// own where viewers look for ours. Others may pass through it, which
    /// is all a connect needs.
    pub fn prepare_dir(dir: &Path, uid: u32) -> anyhow::Result<()> {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) => {
                anyhow::ensure!(
                    meta.file_type().is_dir(),
                    "{} is not a directory; remove it to share panes",
                    dir.display()
                );
                anyhow::ensure!(
                    meta.uid() == uid,
                    "{} belongs to another account; an administrator has to remove it \
                     before panes can be shared",
                    dir.display()
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                std::fs::DirBuilder::new()
                    .mode(0o711)
                    .create(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            Err(err) => return Err(err).with_context(|| format!("checking {}", dir.display())),
        }
        // Whatever the umask left, or a mode someone changed since.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o711))
            .with_context(|| format!("setting the mode of {}", dir.display()))?;
        Ok(())
    }

    impl Listener {
        pub fn open() -> anyhow::Result<Self> {
            let uid = own_uid();
            let dir = codec::share_socket_dir(uid);
            prepare_dir(&dir, uid)?;
            let path = dir.join(codec::SHARE_SOCKET_NAME);
            // One server per account shares: a second one -- a test server
            // beside the usual one, say -- would take the viewers' socket
            // away from the first.
            anyhow::ensure!(
                !crate::local::someone_listens(&path),
                "another ThinkTerm server of this account already shares panes from {}; \
                 stop its shares first",
                path.display()
            );
            crate::local::claim_socket_path(&path)?;
            let listener = UnixListener::bind(&path)
                .with_context(|| format!("listening on {}", path.display()))?;
            // Any account may connect; who did is read from the socket.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
                .with_context(|| format!("setting the mode of {}", path.display()))?;
            let closing = Arc::new(AtomicBool::new(false));
            {
                let closing = Arc::clone(&closing);
                std::thread::Builder::new()
                    .name("share-accept".into())
                    .spawn(move || accept_loop(listener, closing))
                    .context("starting the share listener")?;
            }
            log::info!("sharing panes on {}", path.display());
            Ok(Self { path, closing })
        }

        /// Stop accepting and remove the socket, before this returns. The
        /// accept loop is woken with a connection of our own, which it
        /// drops on seeing why. That connection does not wait for room:
        /// the loop may itself be waiting for the registry, held by whoever
        /// closes. The directory stays: removed, another account could make
        /// one of the same name, and a viewer that had looked at ours would
        /// connect to theirs.
        pub fn close(self) {
            self.closing.store(true, Ordering::SeqCst);
            match UnixStream::connect_without_waiting(&self.path) {
                Ok(_) => {}
                // No room: the loop has others to accept first, and sees
                // why on the next of them.
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => log::warn!("cannot wake the share listener to close it: {err:#}"),
            }
            let _ = std::fs::remove_file(&self.path);
            log::info!("no longer sharing panes on {}", self.path.display());
        }
    }

    fn accept_loop(listener: UnixListener, closing: Arc<AtomicBool>) {
        loop {
            let accepted = listener.accept();
            if closing.load(Ordering::SeqCst) {
                break;
            }
            let stream = match accepted {
                Ok((stream, _)) => stream,
                Err(err) => {
                    // Out of descriptors, say: wait a moment and go on, for
                    // as long as a share wants the socket.
                    log::warn!("share listener: accept failed: {err:#}");
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
            };
            let peer = match identify(&stream) {
                Ok(peer) => peer,
                Err(err) => {
                    log::warn!("share listener: refusing a connection: {err:#}");
                    continue;
                }
            };
            // Nothing is decoded, nor any notification subscribed, for an
            // account no share is for.
            if !admits_anywhere(&peer) {
                log::debug!("share listener: no share is for uid {}", peer.uid);
                continue;
            }
            let Some(slot) = ConnectionSlot::take(peer.uid) else {
                log::warn!(
                    "share listener: too many connections; refusing uid {}",
                    peer.uid
                );
                continue;
            };
            crate::connections::spawn(async move {
                let _slot = slot;
                if let Err(err) = crate::dispatch::process(
                    stream,
                    crate::sessionhandler::ConnectionPeer::Share(peer),
                )
                .await
                {
                    log::debug!("share connection: {err:#}");
                }
            });
        }
    }

    /// The account at the other end, as the kernel reports it.
    fn identify(stream: &UnixStream) -> anyhow::Result<SharePeer> {
        let uid = peer_uid(stream)?;
        let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
            .context("looking up the connecting account")?
            .ok_or_else(|| anyhow!("uid {uid} has no account"))?;
        Ok(SharePeer::new(uid, user.name, user.gid.as_raw()))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_uid(stream: &UnixStream) -> anyhow::Result<u32> {
        use nix::sys::socket::{getsockopt, sockopt};
        Ok(getsockopt(stream, sockopt::PeerCredentials)
            .context("reading the peer's credentials")?
            .uid())
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    fn peer_uid(stream: &UnixStream) -> anyhow::Result<u32> {
        use nix::sys::socket::{getsockopt, sockopt};
        Ok(getsockopt(stream, sockopt::LocalPeerCred)
            .context("reading the peer's credentials")?
            .uid())
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )))]
    fn peer_uid(_stream: &UnixStream) -> anyhow::Result<u32> {
        anyhow::bail!("this system does not say who is at the other end of a socket")
    }
}

#[cfg(unix)]
pub use listener::prepare_dir;

/// A share of `pane_id` with one viewer in it, put in place without a mux,
/// for tests of what a viewer may do. Returns the share and the viewer.
#[cfg(test)]
pub(crate) fn share_for_test(pane_id: PaneId, history: bool, peer: &SharePeer) -> (u64, u64) {
    let mut registry = REGISTRY.lock().unwrap();
    registry.next_share_id += 1;
    let share_id = registry.next_share_id;
    registry.next_viewer_id += 1;
    let viewer_id = registry.next_viewer_id;
    let mut viewers = BTreeMap::new();
    viewers.insert(
        viewer_id,
        Viewer {
            name: peer.name.clone(),
            uid: peer.uid,
            since: SystemTime::now(),
            sender: PduSender::new(|_| Ok(())),
            dropper: peer.dropper.clone(),
            asked: None,
        },
    );
    registry.shares.insert(
        share_id,
        Share {
            pane_id,
            sharer: Sharer::default(),
            audience: vec![ShareAudience::User(peer.name.clone())],
            history,
            started: SystemTime::now(),
            expires: None,
            viewers,
            controller: None,
            requests: vec![],
            removed: HashSet::new(),
        },
    );
    (share_id, viewer_id)
}

/// Let `viewer_id` type in a share made by `share_for_test`.
#[cfg(test)]
pub(crate) fn grant_for_test(share_id: u64, viewer_id: u64) {
    let mut registry = REGISTRY.lock().unwrap();
    registry.shares.get_mut(&share_id).unwrap().controller = Some(Controller {
        viewer_id,
        until: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Single ranges and an empty one are what is being asked about.
    #[allow(clippy::single_range_in_vec_init, clippy::reversed_empty_ranges)]
    #[test]
    fn a_viewer_asks_for_rows_once_and_not_too_many() {
        // The same screen asked for again and again is read once.
        let asked: Vec<_> = std::iter::repeat_n(100..124, 100_000).collect();
        assert_eq!(viewer_rows(asked), vec![100..124]);
        // Overlapping and touching ranges merge; empty and backward ones
        // go.
        #[allow(clippy::reversed_empty_ranges)]
        let asked = vec![10..20, 0..5, 15..30, 30..31, 50..50, 60..55];
        assert_eq!(viewer_rows(asked), vec![0..5, 10..31]);
        // All of a long history is cut to the limit, earliest first.
        assert_eq!(viewer_rows(vec![0..1_000_000]), vec![0..MAX_ROWS_PER_ASK]);
        assert_eq!(
            viewer_rows(vec![0..1500, 5000..6000]),
            vec![0..1500, 5000..5000 + MAX_ROWS_PER_ASK - 1500]
        );
    }

    #[test]
    fn names_are_plain() {
        assert!(plausible_name("fox"));
        assert!(plausible_name("team-3"));
        assert!(plausible_name("svc$"));
        // As a directory service names its accounts.
        assert!(plausible_name("alice@example.test"));
        assert!(!plausible_name(""));
        assert!(!plausible_name("../etc"));
        assert!(!plausible_name("a b"));
        assert!(!plausible_name(&"x".repeat(65)));
    }

    #[test]
    fn durations_are_bounded() {
        assert!(after(60).is_ok());
        assert!(after(MAX_DURATION.as_secs() + 1).is_err());
        assert!(after(u64::MAX).is_err());
    }

    #[test]
    fn the_log_starts_afresh_past_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("share-log.jsonl");
        append_log(&path, "first").unwrap();
        std::fs::write(&path, vec![b'x'; (LOG_LIMIT + 1) as usize]).unwrap();
        append_log(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
        assert!(path.with_extension("1.jsonl").exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_directory_must_be_ours() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("share");
        prepare_dir(&dir, own_uid()).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o711);
        // Someone else's directory is refused.
        let err = prepare_dir(&dir, own_uid() + 1).unwrap_err();
        assert!(err.to_string().contains("another account"), "{}", err);
        // So is a symlink standing in for one.
        let link = base.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(prepare_dir(&link, own_uid()).is_err());
    }

    #[test]
    fn the_owner_and_the_named_are_admitted() {
        let audience = vec![ShareAudience::User("someone-else".into())];
        let me = SharePeer::new(own_uid(), "me".into(), 0);
        assert!(admitted(&audience, &me));
        let named = SharePeer::new(own_uid().wrapping_add(1), "someone-else".into(), 12345);
        assert!(admitted(&audience, &named));
        let stranger = SharePeer::new(own_uid().wrapping_add(2), "stranger".into(), 12345);
        assert!(!admitted(&audience, &stranger));
    }

    #[test]
    fn a_removed_account_cannot_come_back() {
        let peer = SharePeer::new(own_uid().wrapping_add(3), "removed-viewer".into(), 12345);
        let (share_id, viewer_id) = share_for_test(1, false, &peer);
        kick(share_id, viewer_id).unwrap();
        let err = join(&peer, share_id, PduSender::new(|_| Ok(()))).unwrap_err();
        assert!(err.to_string().contains("no share"), "{}", err);
        assert!(
            !admits_anywhere(&peer) || candidates(&peer).iter().all(|s| s.share_id != share_id)
        );
        let _ = stop(share_id);
    }

    #[test]
    fn asking_again_at_once_is_not_heard() {
        let peer = SharePeer::new(own_uid().wrapping_add(4), "asker".into(), 12345);
        let (share_id, viewer_id) = share_for_test(2, false, &peer);
        request_control(viewer_id, share_id).unwrap();
        release_control(viewer_id, share_id).unwrap();
        request_control(viewer_id, share_id).unwrap();
        let info = list().into_iter().find(|s| s.share_id == share_id).unwrap();
        assert!(info.requests.is_empty(), "{:?}", info.requests);
        let _ = stop(share_id);
    }

    #[test]
    fn a_pane_has_one_share_at_a_time() {
        let peer = SharePeer::new(own_uid().wrapping_add(6), "one-share".into(), 12345);
        let (share_id, _) = share_for_test(3, false, &peer);
        // What `start` refuses a second share of the pane on.
        assert_eq!(REGISTRY.lock().unwrap().share_of(3), Some(share_id));
        stop(share_id).unwrap();
        assert_eq!(REGISTRY.lock().unwrap().share_of(3), None);
    }

    #[test]
    fn only_a_person_at_the_desktop_shares_or_lets_type() {
        let asked_by_a_program = ShareStart {
            pane_id: 3,
            audience: vec![ShareAudience::User(own_name())],
            history: false,
            expires_in_secs: None,
            from_desktop: false,
        };
        let err = start(asked_by_a_program, Sharer::default()).unwrap_err();
        assert!(err.to_string().contains("share button"), "{:#}", err);
        let err = grant(1, 1, None, false).unwrap_err();
        assert!(err.to_string().contains("share panel"), "{:#}", err);
    }

    #[test]
    fn a_share_lasts_half_an_hour_at_most() {
        assert_eq!(share_for(None), 30 * 60);
        assert_eq!(share_for(Some(15 * 60)), 15 * 60);
        assert_eq!(share_for(Some(2 * 60 * 60)), 30 * 60);
    }

    #[test]
    fn shares_end_when_the_desktop_that_started_them_leaves() {
        let peer = SharePeer::new(own_uid().wrapping_add(7), "left-behind".into(), 12345);
        let sharer = Sharer::default();
        let (left, viewer_id) = share_for_test(7, false, &peer);
        grant_for_test(left, viewer_id);
        REGISTRY.lock().unwrap().shares.get_mut(&left).unwrap().sharer = sharer.clone();
        let (kept, _) = share_for_test(8, false, &peer);
        sharer.left();
        assert!(!sharer.is_connected());
        let shares: Vec<u64> = list().iter().map(|s| s.share_id).collect();
        assert!(!shares.contains(&left), "{:?}", shares);
        assert!(!may_type(viewer_id, left, 7));
        assert!(shares.contains(&kept), "{:?}", shares);
        stop(kept).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_account_is_kept_as_the_machine_spells_it() {
        let me = own_name();
        assert_eq!(
            check_audience(&ShareAudience::User(me.clone())).unwrap(),
            ShareAudience::User(me.clone())
        );
        // macOS finds an account in whatever case it is typed; the share
        // names it as a viewer's connection will be named.
        #[cfg(target_os = "macos")]
        assert_eq!(
            check_audience(&ShareAudience::User(me.to_uppercase())).unwrap(),
            ShareAudience::User(me)
        );
    }

    #[test]
    fn connections_are_capped_per_account() {
        let uid = own_uid().wrapping_add(5);
        let slots: Vec<_> = (0..MAX_CONNECTIONS_PER_ACCOUNT)
            .map(|_| ConnectionSlot::take(uid).unwrap())
            .collect();
        assert!(ConnectionSlot::take(uid).is_none());
        drop(slots);
        assert!(ConnectionSlot::take(uid).is_some());
    }
}
