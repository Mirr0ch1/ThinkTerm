//! What a pane showed when the process holding it stopped, kept on disk
//! until its Thread's layout is restored and then put back above the new
//! shell. On where the desktop runs unless Settings turns it off; off on a
//! remote host unless turned on there.
//!
//! Only a controlled exit keeps anything: a shutdown or logout, the session
//! server being stopped, ThinkTerm quitting with its terminals inside it. A
//! crash brings the layout back without it. A record is read once, by the
//! local domain spawning the pane that takes its place, and removed; nothing
//! is held in memory in between.
//!
//! Whoever restores a layout -- the session server for its own Threads, the
//! app for the ones it rebuilds, here or in the local session server -- says
//! in the spawn command which record a pane starts with, and the process that
//! spawns it reads that from this machine's disk.
//!
//! Kept: the primary screen and its scrollback, as text and colors. Not
//! kept: a full-screen program's frame, pictures, and whatever looks like a
//! password or token -- masked before anything is written, so the original
//! never reaches the disk.

use crate::domain::LocalDomain;
use crate::localpane::LocalPane;
use crate::pane::{Pane, PaneId};
use crate::Mux;
use anyhow::Context;
use portable_pty::CommandBuilder;
use regex::{Regex, RegexSet};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use termwiz::cell::{Cell, CellAttributes};
use termwiz::surface::{Change, Line, SEQ_ZERO};

/// Whose records: the session server's or the app's, each keyed by
/// workspace name and by the pane ids that process gave its panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Server,
    App,
}

impl Owner {
    fn dir_name(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::App => "app",
        }
    }
}

/// How long a Thread nobody reopens keeps its record.
const MAX_AGE: Duration = Duration::from_secs(15 * 24 * 60 * 60);
/// The most rows a pane keeps: past the default scrollback, and short
/// enough that a huge one cannot outlast the time a shutdown allows.
const MAX_LINES: usize = 10_000;
/// More than any scrollback is not a record of ours.
const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;
const HEADER: &str = "ThinkTerm kept output 1";
/// Names a restoring spawn's record, `owner:pane:workspace`, for
/// `LocalDomain::spawn_pane`, which takes it out of the environment before
/// it builds the command.
const SEED_ENV: &str = "THINKTERM_KEPT_OUTPUT";
/// The same, on a command made only to carry the name for a spawn that gave
/// none. The spawn makes it none again: a local domain fills in no command
/// with defaults of its own, a WSL domain's directory for one.
const BARE_SEED_ENV: &str = "THINKTERM_KEPT_OUTPUT_BARE";

/// The switch, for this machine alone: a small file beside the desktop's
/// settings that Settings, `thinkterm cli restore-output` and the session
/// server all go by. Not a key in settings.json: a command line tool
/// rewriting that file would reorder it, and one creating it would make the
/// desktop take a fresh install for an old one.
fn switch_path() -> PathBuf {
    config::native_settings_path().with_file_name("restore-pane-output")
}

/// Whether this machine keeps pane output, read when it matters: at an exit
/// and at a restore. Not until something turns it on: the desktop does on
/// its own machine (see `on_by_default`), a remote host only when told to.
pub fn enabled() -> bool {
    enabled_at(&switch_path())
}

fn enabled_at(switch: &Path) -> bool {
    std::fs::read_to_string(switch).is_ok_and(|text| text.trim() == "on")
}

/// Turn it on or off for this machine. Off removes what was kept, too, and
/// stays off: it is written down rather than the switch removed, which the
/// desktop would take for one never set.
pub fn set_enabled(on: bool) -> anyhow::Result<()> {
    set_enabled_at(&switch_path(), on)?;
    if !on {
        discard_all();
    }
    Ok(())
}

fn set_enabled_at(switch: &Path, on: bool) -> anyhow::Result<()> {
    if let Some(dir) = switch.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = switch.with_extension("partial");
    std::fs::write(&partial, if on { "on\n" } else { "off\n" })?;
    std::fs::rename(&partial, switch)?;
    Ok(())
}

/// The desktop, as it starts: its own machine keeps pane output unless it
/// has been told either way.
pub fn on_by_default() {
    if let Err(err) = on_by_default_at(&switch_path()) {
        log::warn!("turning on kept pane output: {err:#}");
    }
}

fn on_by_default_at(switch: &Path) -> anyhow::Result<()> {
    if switch.try_exists()? {
        return Ok(());
    }
    set_enabled_at(switch, true)
}

fn root() -> PathBuf {
    config::DATA_DIR.join("kept-output")
}

/// Before a controlled exit: replace what `key` kept with what `panes` show
/// now, under the pane ids its saved layout records them by. When none of
/// them has anything to keep, the old record stays: a Thread whose panes
/// were never restored still has what it had.
pub fn save(owner: Owner, key: &str, panes: &[(PaneId, Arc<dyn Pane>)]) {
    let kept: Vec<(PaneId, String)> = panes
        .iter()
        .filter_map(|(id, pane)| Some((*id, capture(&**pane)?)))
        .collect();
    if let Err(err) = write_records(&root(), owner, key, &kept, now()) {
        log::warn!("keeping the pane output of {key}: {err:#}");
    }
}

/// Remove what nobody will restore: the records of keys `keep` does not
/// name, and any older than [`MAX_AGE`] whoever kept them -- the other
/// process may never run here again to clear its own.
pub fn prune(owner: Owner, keep: &HashSet<String>) {
    prune_at(&root(), owner, keep, SystemTime::now());
    prune_stale();
}

/// Remove records older than [`MAX_AGE`], whoever kept them.
pub fn prune_stale() {
    prune_stale_at(&root(), SystemTime::now());
}

/// Everything `owner` kept, as when the setting is turned off.
pub fn discard(owner: Owner) {
    remove_tree(&root().join(owner.dir_name()));
}

/// Everything kept, by any owner.
pub fn discard_all() {
    remove_tree(&root());
}

/// `command`, made to start its terminal with what the pane `saved` in
/// `key`'s layout showed, when that was kept. For a spawn in a local domain,
/// in this process or the local session server: the one that spawns it
/// reads the record from this machine's disk. Unchanged otherwise.
pub fn with_seed(
    owner: Owner,
    key: &str,
    saved: PaneId,
    command: Option<CommandBuilder>,
) -> Option<CommandBuilder> {
    if !enabled() {
        return command;
    }
    with_seed_at(&root(), owner, key, saved, command)
}

fn with_seed_at(
    root: &Path,
    owner: Owner,
    key: &str,
    saved: PaneId,
    command: Option<CommandBuilder>,
) -> Option<CommandBuilder> {
    if !key_dir(root, owner, key).join(saved.to_string()).is_file() {
        return command;
    }
    let (mut command, name) = match command {
        Some(command) => (command, SEED_ENV),
        None => (CommandBuilder::new_default_prog(), BARE_SEED_ENV),
    };
    command.env(name, format!("{}:{saved}:{key}", owner.dir_name()));
    Some(command)
}

/// `command`, made to start with `key`'s record when it kept exactly one:
/// for a Thread that comes back without a recorded layout, which is a
/// Thread that only ever had the one pane. With more there is no telling
/// which pane was which, and none is used.
pub fn with_lone_seed(key: &str, command: Option<CommandBuilder>) -> Option<CommandBuilder> {
    if !enabled() {
        return command;
    }
    with_lone_seed_at(&root(), key, command)
}

fn with_lone_seed_at(
    root: &Path,
    key: &str,
    command: Option<CommandBuilder>,
) -> Option<CommandBuilder> {
    let mut kept = vec![];
    for owner in [Owner::Server, Owner::App] {
        let Ok(entries) = std::fs::read_dir(key_dir(root, owner, key)) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(pane) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<PaneId>().ok())
            {
                kept.push((owner, pane));
            }
        }
    }
    match kept.as_slice() {
        [(owner, pane)] => with_seed_at(root, *owner, key, *pane, command),
        _ => command,
    }
}

/// `key`'s layout is back: what was kept for it is spent, and a record no
/// pane took would only meet the wrong pane later, once ids come round again.
pub fn finish(key: &str) {
    finish_at(&root(), key);
}

fn finish_at(root: &Path, key: &str) {
    for owner in [Owner::Server, Owner::App] {
        remove_tree(&key_dir(root, owner, key));
    }
}

/// One pane's record: when it was kept, and the text with its colors.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Seed {
    closed: i64,
    text: String,
}

impl Seed {
    /// What the new terminal starts with: the kept text, a dim line saying
    /// when it stopped and came back, and the start of a fresh line for
    /// the shell. A pseudo console paints from the top of what it takes to
    /// be an empty screen, so there all of it goes up into the scrollback
    /// first and the shell starts at the top.
    pub(crate) fn bytes(&self, rows: usize, conpty: bool) -> Vec<u8> {
        let mut out = self.text.clone();
        out.push_str(&format!(
            "\r\n\x1b[2m── closed {} · restored {} ──\x1b[0m\r\n",
            local_time(self.closed),
            local_time(now())
        ));
        if conpty {
            // From any row, one fewer than the screen holds scrolls the
            // cursor's row to the top and everything above it off it.
            for _ in 1..rows {
                out.push_str("\r\n");
            }
            out.push_str("\x1b[H");
        }
        out.into_bytes()
    }
}

/// The record a restoring spawn names, taken out of its command before that
/// is built, so the shell never sees the variable, and moved aside so no
/// other pane gets it.
pub(crate) fn take_seed(command: &mut Option<CommandBuilder>) -> Option<Claim> {
    let named = take_named(command)?;
    if !enabled() {
        return None;
    }
    take_seed_at(&root(), &named)
}

/// A record taken for a pane being made: gone once the pane exists, back in
/// place if making it fails, so the next try -- in another folder after the
/// Project's was refused, say -- still starts with it.
pub(crate) struct Claim {
    seed: Seed,
    path: PathBuf,
    aside: PathBuf,
    spent: bool,
}

impl Claim {
    pub(crate) fn bytes(&self, rows: usize, conpty: bool) -> Vec<u8> {
        self.seed.bytes(rows, conpty)
    }

    /// The pane exists and starts with the record: it has done its work.
    pub(crate) fn spend(mut self) {
        self.spent = true;
        std::fs::remove_file(&self.aside).ok();
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.spent {
            std::fs::rename(&self.aside, &self.path).ok();
        }
    }
}

/// `command`, made only to carry a record's name for a spawn that gave none,
/// has been filled in by a client -- with the shell chosen in Settings, which
/// the session server does not know -- and is now one to run as it is.
pub fn filled_in(command: &mut CommandBuilder) {
    if let Some(named) = command
        .get_env(BARE_SEED_ENV)
        .map(|value| value.to_os_string())
    {
        command.env_remove(BARE_SEED_ENV);
        command.env(SEED_ENV, named);
    }
}

/// What `command` names, out of its environment whether or not it is of
/// use; a command that was only carrying the name is none again.
fn take_named(command: &mut Option<CommandBuilder>) -> Option<String> {
    let cmd = command.as_mut()?;
    let named = cmd.get_env(SEED_ENV).map(|value| value.to_os_string());
    let bare = cmd.get_env(BARE_SEED_ENV).map(|value| value.to_os_string());
    cmd.env_remove(SEED_ENV);
    cmd.env_remove(BARE_SEED_ENV);
    if bare.is_some() {
        *command = None;
    }
    named.or(bare)?.into_string().ok()
}

fn take_seed_at(root: &Path, named: &str) -> Option<Claim> {
    let mut parts = named.splitn(3, ':');
    let owner = match parts.next()? {
        "server" => Owner::Server,
        "app" => Owner::App,
        _ => return None,
    };
    let pane: PaneId = parts.next()?.parse().ok()?;
    let path = key_dir(root, owner, parts.next()?).join(pane.to_string());
    // Moved before it is read: of two spawns naming it, one gets it.
    let aside = path.with_extension("taken");
    std::fs::rename(&path, &aside).ok()?;
    match read_record(&aside) {
        Ok(seed) => Some(Claim {
            seed,
            path,
            aside,
            spent: false,
        }),
        Err(err) => {
            log::warn!("reading kept pane output {}: {err:#}", path.display());
            std::fs::remove_file(&aside).ok();
            None
        }
    }
}

/// The text a local pane's primary screen holds, masked, or None when it
/// holds nothing or is not a terminal on this machine: an ssh or tmux pane
/// is a `LocalPane` too, but its domain is not local and no restore of it
/// would read a record.
fn capture(pane: &dyn Pane) -> Option<String> {
    let local = Mux::try_get()?
        .get_domain(pane.domain_id())?
        .downcast_ref::<LocalDomain>()
        .is_some();
    if !local {
        return None;
    }
    keep(
        pane.downcast_ref::<LocalPane>()?
            .primary_screen_lines(MAX_LINES),
    )
}

fn keep(mut lines: Vec<Line>) -> Option<String> {
    while lines.last().is_some_and(Line::is_whitespace) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }
    mask_secrets(&mut lines);
    match render(&lines) {
        Ok(text) => Some(text),
        Err(err) => {
            log::warn!("rendering pane output to keep: {err:#}");
            None
        }
    }
}

/// `lines` as escape sequences. A row that wrapped runs on into the next
/// one, so the restored pane wraps the text again at its own width.
fn render(lines: &[Line]) -> anyhow::Result<String> {
    let mut changes = vec![];
    // The pen as the escapes leave it: what a row's changes last set, not its
    // last cell. A row of plain spaces sets nothing, and taking its last cell
    // would leave the next row in the colours of the one before. A clear to
    // the end of the row leaves the pen in its background alone: a row that
    // is only a background is nothing else.
    let mut attr = CellAttributes::blank();
    for (index, line) in lines.iter().enumerate() {
        let row = line.changes(&attr);
        if let Some(set) = row.iter().rev().find_map(|change| match change {
            Change::AllAttributes(set) => Some(set.clone()),
            Change::ClearToEndOfLine(background) => {
                Some(CellAttributes::blank().set_background(*background).clone())
            }
            _ => None,
        }) {
            attr = set;
        }
        changes.extend(row);
        if index + 1 < lines.len() && !line.last_cell_was_wrapped() {
            // A line feed that scrolls fills the new row with the pen's
            // background: plain, or a row ending in a colour paints the next.
            if attr != CellAttributes::blank() {
                attr = CellAttributes::blank();
                changes.push(Change::AllAttributes(attr.clone()));
            }
            changes.push(Change::Text("\r\n".to_string()));
        }
    }
    changes.push(Change::AllAttributes(CellAttributes::blank()));
    termwiz_funcs::changes_to_escapes(&changes)
}

/// What a value only its name vouches for must look like. Code says
/// `token = …` and `password: …` too; a secret is data, not an expression.
#[derive(Clone, Copy, PartialEq)]
enum Check {
    None,
    /// Not code: no substitution, call, index or structure.
    Literal,
    /// A literal with a digit in it, as generated tokens and keys have.
    Generated,
}

/// Where a private key's rows are, line by line, and the margin a listing
/// put before each of them (`margin_pattern`).
#[derive(Clone)]
enum KeyRows {
    Outside,
    /// After the BEGIN line: header lines, and the blank line that ends them.
    Header(Option<Regex>),
    Body(Option<Regex>),
}

struct Pattern {
    regex: Regex,
    /// The capture group that is the secret: 0 for the whole match. A value
    /// that may stand in quotes has a group for each form, and the one that
    /// took part is the secret.
    group: usize,
    check: Check,
}

struct Patterns {
    any: RegexSet,
    each: Vec<Pattern>,
    /// A key's BEGIN line: the marker ends it, as in a file. One that says
    /// more after it quotes the marker and starts no key.
    key_begin: Regex,
    /// A header line an encrypted PEM key or a PGP key has before its body.
    key_header: Regex,
    /// A row of a PEM key's body: base64 and nothing else.
    key_body: Regex,
    /// A value that starts as a call, an index or a generic type does.
    code: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let each: &[(&str, usize, Check)] = &[
            // A value named for a password: `PASSWORD=…`, `db_pass: …`,
            // `"pwd": "…"`. Passwords can be short and plain words. One in
            // quotes runs to the closing quote: a passphrase has spaces, and
            // a generated password `,` and `;`.
            (
                r#"(?i)\b(?:[a-z0-9_.-]*[_.-])?(?:password|passwd|passphrase|pass|pwd)["']?\s*[:=]\s*(?:"([^"]{4,})"|'([^']{4,})'|["']?([^\s"',;]{4,}))"#,
                1,
                Check::Literal,
            ),
            // The variables tools read one from, the word run on from a
            // prefix: `PGPASSWORD=…`, `SSHPASS=…`. Capitals and `=` only, as
            // an environment has them, so prose and code that say "bypass"
            // are left alone.
            (
                r#"\b[A-Z][A-Z0-9]*(?:PASSWORD|PASSWD|PASSPHRASE|PASS)=(?:"([^"]{4,})"|'([^']{4,})'|["']?([^\s"',;]{4,}))"#,
                1,
                Check::Literal,
            ),
            // A command's option: `--password …`, `--token=…`.
            (
                r#"(?i)(?:^|\s)--?(?:password|passwd|pass|pwd|token|secret|api-?key|auth-token)(?:=|\s+)(?:"([^"]{4,})"|'([^']{4,})'|["']?([^\s"']{4,}))"#,
                1,
                Check::Literal,
            ),
            // A value named for a token or key: `API_TOKEN=…`, `SECRET_KEY=…`,
            // `"client_secret": "…"`. A count or a type name is no secret.
            (
                r#"(?i)\b[a-z0-9_.-]*(?:secret|token|key|credentials?)["']?\s*[:=]\s*["']?([^\s"',;]{8,})"#,
                1,
                Check::Generated,
            ),
            (
                r"(?i)\bauthorization:\s*(?:basic|bearer|token)\s+([A-Za-z0-9._~+/=-]{8,})",
                1,
                Check::None,
            ),
            (r"(?i)\bbearer\s+([A-Za-z0-9._~+/=-]{16,})", 1, Check::None),
            // The password in `scheme://user:password@host`.
            (
                r"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s:/@]+:([^\s@/]+)@",
                1,
                Check::None,
            ),
            // A private key on one line, as a JSON key file holds it.
            (
                r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----(.+?)-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----",
                1,
                Check::None,
            ),
            // A PEM block in base64, as a Kubernetes secret holds one.
            (r"\bLS0tLS1CRUdJTi[A-Za-z0-9+/=]{16,}", 0, Check::None),
            // The formats services hand their keys out in.
            (r"\bsk-[A-Za-z0-9_-]{20,}", 0, Check::None),
            (r"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}", 0, Check::None),
            (r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}", 0, Check::None),
            (r"\bgithub_pat_[A-Za-z0-9_]{30,}", 0, Check::None),
            (r"\bglpat-[A-Za-z0-9_-]{20,}", 0, Check::None),
            (r"\bxox[abprs]-[A-Za-z0-9-]{10,}", 0, Check::None),
            (r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", 0, Check::None),
            (r"\bAIza[0-9A-Za-z_-]{35}", 0, Check::None),
            (
                r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
                0,
                Check::None,
            ),
        ];
        Patterns {
            any: RegexSet::new(each.iter().map(|(pattern, _, _)| *pattern))
                .expect("secret patterns compile"),
            each: each
                .iter()
                .map(|(pattern, group, check)| Pattern {
                    regex: Regex::new(pattern).expect("secret pattern"),
                    group: *group,
                    check: *check,
                })
                .collect(),
            key_begin: Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----\s*$")
                .expect("key pattern"),
            key_header: Regex::new(
                r"^\s*(?:Proc-Type|DEK-Info|Version|Comment|Hash|Charset|MessageID):",
            )
            .expect("key pattern"),
            key_body: Regex::new(r"^\s*[A-Za-z0-9+/=]+\s*$").expect("key pattern"),
            code: Regex::new(r"^[A-Za-z_][A-Za-z0-9_]+(?:(?:\.|::)[A-Za-z_][A-Za-z0-9_]*)*[(\[<]")
                .expect("code pattern"),
        }
    })
}

/// `value`, which stood in the quote `quote` names if any, against what
/// `check` asks of it.
fn passes(check: Check, value: &str, quote: Option<char>, patterns: &Patterns) -> bool {
    let literal = || !looks_like_code(value, quote, patterns);
    match check {
        Check::None => true,
        Check::Literal => literal(),
        Check::Generated => literal() && value.chars().any(|c| c.is_ascii_digit()),
    }
}

/// Whether `value` reads as an expression rather than data: a substitution,
/// a call, an index, a generic type or a structure. Generated passwords have
/// brackets and `&`, `*`, `$` in them too, so only the shape counts, and a
/// quoted value is a string unless a shell would still expand it: nothing
/// expands in single quotes.
fn looks_like_code(value: &str, quote: Option<char>, patterns: &Patterns) -> bool {
    if quote == Some('\'') {
        return false;
    }
    if value.starts_with('$') || value.contains("${") || value.contains("$(") {
        return true;
    }
    if quote.is_some() {
        return false;
    }
    value.starts_with(['(', '[', '{', '<', '&', '*']) || patterns.code.is_match(value)
}

/// A pattern for the margin `before` a key's BEGIN marker, which a listing
/// puts before every row: a line number, a diff's sign, a search hit's file
/// name. Numbers and runs of spaces may change from row to row, and a search
/// marks its hit with `:` and the rows around it with `-`. None when it is
/// only indentation, which the rows' own check allows for.
fn margin_pattern(before: &str) -> Option<Regex> {
    if before.trim().is_empty() {
        return None;
    }
    let mut pattern = String::from("^");
    let mut chars = before.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            while chars.next_if(char::is_ascii_digit).is_some() {}
            pattern.push_str(r"\d+");
        } else if c.is_whitespace() {
            while chars.next_if(|c| c.is_whitespace()).is_some() {}
            pattern.push_str(r"\s*");
        } else if c == ':' || c == '-' {
            pattern.push_str("[:-]");
        } else {
            pattern.push_str(&regex::escape(c.encode_utf8(&mut [0; 4])));
        }
    }
    Regex::new(&pattern).ok()
}

/// `text` past the margin `margin` matches at its start, or all of it.
fn after_margin<'a>(text: &'a str, margin: &Option<Regex>) -> &'a str {
    match margin.as_ref().and_then(|margin| margin.find(text)) {
        Some(found) => &text[found.end()..],
        None => text,
    }
}

/// Replace what looks like a password, token or private key with `•`, cell
/// by cell, keeping the colors, and drop links whose address holds one. A
/// logical line is matched whole, so a token that wrapped is found across
/// the break.
fn mask_secrets(lines: &mut [Line]) {
    let patterns = patterns();
    let mut key = KeyRows::Outside;
    let mut start = 0;
    while start < lines.len() {
        let mut end = start + 1;
        while end < lines.len() && lines[end - 1].last_cell_was_wrapped() {
            end += 1;
        }
        mask_logical_line(&mut lines[start..end], patterns, &mut key);
        start = end;
    }
}

fn mask_logical_line(rows: &mut [Line], patterns: &Patterns, key: &mut KeyRows) {
    let mut text = String::new();
    // Where each cell's text starts in `text`, and which cell it is.
    let mut cells = vec![];
    for (row, line) in rows.iter().enumerate() {
        for cell in line.visible_cells() {
            cells.push((text.len(), row, cell.cell_index(), cell.width()));
            text.push_str(cell.str());
        }
    }

    let mut secret: Vec<Range<usize>> = vec![];
    // A key's body runs from its BEGIN line to its END line, and is base64
    // throughout, after the header lines an encrypted PEM key or a PGP key
    // starts with and the blank line that ends them: any other line ends
    // it, so a search hit or a diff that shows only the BEGIN line masks
    // nothing after it. Each row is read past the margin the BEGIN line had.
    let (in_header, margin) = match std::mem::replace(key, KeyRows::Outside) {
        KeyRows::Outside => (false, None),
        KeyRows::Header(margin) => (true, Some(margin)),
        KeyRows::Body(margin) => (false, Some(margin)),
    };
    if let Some(margin) = margin {
        let rest = after_margin(&text, &margin);
        *key = if in_header && patterns.key_header.is_match(rest) {
            KeyRows::Header(margin)
        } else if in_header && rest.trim().is_empty() {
            KeyRows::Body(margin)
        } else if patterns.key_body.is_match(rest) {
            secret.push(text.len() - rest.len()..text.len());
            KeyRows::Body(margin)
        } else {
            KeyRows::Outside
        };
    }
    if matches!(*key, KeyRows::Outside) {
        if let Some(begin) = patterns.key_begin.find(&text) {
            *key = KeyRows::Header(margin_pattern(&text[..begin.start()]));
        }
    }
    if patterns.any.is_match(&text) {
        for pattern in &patterns.each {
            for captures in pattern.regex.captures_iter(&text) {
                if let Some(found) = captures.iter().skip(pattern.group).flatten().next() {
                    let quote = text[..found.start()]
                        .chars()
                        .next_back()
                        .filter(|c| matches!(c, '"' | '\''));
                    if passes(pattern.check, found.as_str(), quote, patterns) {
                        secret.push(found.range());
                    }
                }
            }
        }
    }

    let masked: HashSet<(usize, usize)> = cells
        .into_iter()
        .filter(|(offset, _, _, width)| {
            *width == 1
                && !text[*offset..].starts_with(' ')
                && secret.iter().any(|range| range.contains(offset))
        })
        .map(|(_, row, index, _)| (row, index))
        .collect();
    for (row, line) in rows.iter_mut().enumerate() {
        let replacements: Vec<(usize, Cell)> = line
            .visible_cells()
            .filter_map(|cell| {
                let masked = masked.contains(&(row, cell.cell_index()));
                let secret_link = cell
                    .attrs()
                    .hyperlink()
                    .is_some_and(|link| masked || patterns.any.is_match(link.uri()));
                if !masked && !secret_link {
                    return None;
                }
                let mut attrs = cell.attrs().clone();
                if secret_link {
                    attrs.set_hyperlink(None);
                }
                let replacement = if masked {
                    Cell::new('•', attrs)
                } else {
                    Cell::new_grapheme_with_width(cell.str(), cell.width(), attrs)
                };
                Some((cell.cell_index(), replacement))
            })
            .collect();
        for (index, cell) in replacements {
            line.set_cell(index, cell, SEQ_ZERO);
        }
    }
}

fn write_records(
    root: &Path,
    owner: Owner,
    key: &str,
    kept: &[(PaneId, String)],
    closed: i64,
) -> anyhow::Result<()> {
    if kept.is_empty() {
        return Ok(());
    }
    let dir = key_dir(root, owner, key);
    remove_tree(&dir);
    create_private_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (id, text) in kept {
        let plain = format!("{HEADER}\n{closed}\n{text}");
        let packed = zstd::encode_all(plain.as_bytes(), 3)?;
        let path = dir.join(id.to_string());
        write_private(&path, &packed).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

fn read_record(path: &Path) -> anyhow::Result<Seed> {
    let mut plain = vec![];
    zstd::stream::read::Decoder::new(std::fs::File::open(path)?)?
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut plain)?;
    anyhow::ensure!(
        plain.len() as u64 <= MAX_RECORD_BYTES,
        "kept output exceeds the size limit"
    );
    let plain = String::from_utf8(plain)?;
    let mut parts = plain.splitn(3, '\n');
    anyhow::ensure!(parts.next() == Some(HEADER), "not a kept output record");
    let closed = parts.next().context("record has no time")?.parse()?;
    Ok(Seed {
        closed,
        text: parts.next().unwrap_or_default().to_string(),
    })
}

/// A key as a folder name: Thread ids and workspace names are made of
/// these characters already, apart from a workspace name's colons.
fn key_dir(root: &Path, owner: Owner, key: &str) -> PathBuf {
    let name: String = key
        .chars()
        .take(200)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = if name.chars().all(|c| c == '.') {
        "_".to_string()
    } else {
        name
    };
    root.join(owner.dir_name()).join(name)
}

fn prune_at(root: &Path, owner: Owner, keep: &HashSet<String>, now: SystemTime) {
    let keep: HashSet<PathBuf> = keep.iter().map(|key| key_dir(root, owner, key)).collect();
    let Ok(entries) = std::fs::read_dir(root.join(owner.dir_name())) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_stale(&entry, now) || !keep.contains(&path) {
            remove_tree(&path);
        }
    }
}

fn prune_stale_at(root: &Path, now: SystemTime) {
    for owner in [Owner::Server, Owner::App] {
        let Ok(entries) = std::fs::read_dir(root.join(owner.dir_name())) else {
            continue;
        };
        for entry in entries.flatten() {
            if is_stale(&entry, now) {
                remove_tree(&entry.path());
            }
        }
    }
}

fn is_stale(entry: &std::fs::DirEntry, now: SystemTime) -> bool {
    entry
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age > MAX_AGE)
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Written whole or not at all, readable by this user alone on unix. On
/// Windows it takes the data directory's ACL, as the other private files
/// there do.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let partial = path.with_extension("partial");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&partial)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&partial, path)
}

fn remove_tree(path: &Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => log::warn!("removing kept pane output {}: {err:#}", path.display()),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

fn local_time(secs: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|time| time.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
