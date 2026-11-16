//! Finding the panes other accounts on this machine share with this one.
//!
//! Each account that shares listens on its own socket,
//! `/tmp/thinkterm-share-<uid>/share.sock` (`codec::share_socket_dir`), and
//! answers there, for whoever connects, with the shares meant for them: it
//! reads who connected from the socket. A socket is believed to be an
//! account's only when the directory it is in belongs to that account and
//! nobody else may write to it.

use crate::client::Client;
use anyhow::{anyhow, Context as _};
use codec::ShareIncomingResponse;
use config::UnixDomain;
use std::path::{Path, PathBuf};
use std::time::Duration;
use wezterm_uds::UnixStream;

/// An account that shares panes from this machine, by its share socket.
#[derive(Debug, Clone)]
pub struct Owner {
    pub uid: u32,
    pub socket: PathBuf,
}

/// At most this many entries of `/tmp` are looked at for share sockets.
const MAX_TMP_ENTRIES: usize = 65536;

/// How long one account's server gets to answer before it is passed over.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(3);

/// A reply or push from another account, before and after decompression.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// The accounts sharing panes on this machine: the share directories in
/// `/tmp` that belong to the account they are named for and nobody else
/// can write to, with a socket in them.
pub fn owners() -> Vec<Owner> {
    owners_in(Path::new("/tmp"))
}

fn owners_in(root: &Path) -> Vec<Owner> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return vec![];
    };
    let mut owners: Vec<Owner> = entries
        .take(MAX_TMP_ENTRIES)
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let uid = name
                .to_str()?
                .strip_prefix(codec::SHARE_SOCKET_DIR_PREFIX)?
                .parse::<u32>()
                .ok()?;
            let dir = root.join(format!("{}{uid}", codec::SHARE_SOCKET_DIR_PREFIX));
            // An alias entry must not send us to another entry's directory.
            if entry.path() != dir {
                return None;
            }
            let socket = dir.join(codec::SHARE_SOCKET_NAME);
            (trusted_dir(&dir, uid) && socket.exists()).then_some(Owner { uid, socket })
        })
        .collect();
    owners.sort_by_key(|owner| owner.uid);
    owners
}

#[cfg(unix)]
fn trusted_dir(dir: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(dir).is_ok_and(|meta| {
        meta.file_type().is_dir() && meta.uid() == uid && meta.mode() & 0o022 == 0
    })
}

#[cfg(not(unix))]
fn trusted_dir(_dir: &Path, _uid: u32) -> bool {
    false
}

/// `work`, or an error once `limit` has passed without an answer.
pub async fn within<T>(
    limit: Duration,
    work: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    smol::future::or(work, async {
        smol::Timer::after(limit).await;
        Err(anyhow!("no answer within {limit:?}"))
    })
    .await
}

/// A connection to `owner`'s share socket, past the version handshake. A
/// socket with no room for another connection is passed over at once: a
/// server that has stopped accepting never makes room, no deadline cuts
/// that wait short, and the accounts after it wait their turn.
pub async fn connect(owner: &Owner) -> anyhow::Result<Client> {
    let domain = UnixDomain {
        name: format!("share-{}", owner.uid),
        socket_path: Some(owner.socket.clone()),
        no_serve_automatically: true,
        // Not ours: `owners` has checked who the directory belongs to.
        skip_permissions_check: true,
        ..Default::default()
    };
    let stream = UnixStream::connect_without_waiting(&owner.socket)
        .with_context(|| format!("connecting to {}", owner.socket.display()))?;
    let client = Client::new_unix_stream_bounded(&domain, stream, MAX_RESPONSE_BYTES)?;
    let ui = mux::connui::ConnectionUI::new_headless();
    within(ANSWER_WITHIN, client.verify_version_compat(&ui))
        .await
        .context("the other account's ThinkTerm did not answer as this version does")?;
    Ok(client)
}

/// The name this machine gives `uid`, which is what a sharing account is
/// called here: not the name its server gives itself, which any account
/// could make up.
#[cfg(unix)]
pub fn account_name(uid: u32) -> String {
    match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
        Ok(Some(user)) => user.name,
        _ => format!("uid{uid}"),
    }
}

#[cfg(not(unix))]
pub fn account_name(uid: u32) -> String {
    format!("uid{uid}")
}

/// What asking every account came to.
#[derive(Default)]
pub struct Incoming {
    /// Each account that shares panes with this one, with its shares.
    pub found: Vec<(Owner, ShareIncomingResponse)>,
    /// The accounts whose ThinkTerm is another version: what they share
    /// cannot be read from this one.
    pub other_version: Vec<String>,
}

/// Every account's shares for this one, asked one account at a time. An
/// account whose server does not answer, or answers with nothing for us,
/// is left out; one that runs another version is named.
pub async fn incoming() -> Incoming {
    let mut incoming = Incoming::default();
    for owner in owners() {
        let answer = async {
            let client = connect(&owner).await?;
            within(ANSWER_WITHIN, client.share_incoming()).await
        };
        match answer.await {
            Ok(mut response) if !response.shares.is_empty() => {
                response.owner = account_name(owner.uid);
                incoming.found.push((owner, response));
            }
            Ok(_) => {}
            Err(err) if is_other_version(&err) => {
                incoming.other_version.push(account_name(owner.uid));
            }
            Err(err) => log::debug!("uid {}: {err:#}", owner.uid),
        }
    }
    incoming
}

fn is_other_version(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.is::<crate::client::IncompatibleVersionError>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    #[cfg(unix)]
    #[test]
    fn aliases_do_not_repeat_a_sharing_account() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let root = tempfile::tempdir().unwrap();
        let uid = std::fs::metadata(root.path()).unwrap().uid();
        let name = format!("{}{uid}", codec::SHARE_SOCKET_DIR_PREFIX);
        let dir = root.path().join(&name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o711)).unwrap();
        std::fs::write(dir.join(codec::SHARE_SOCKET_NAME), []).unwrap();
        assert_eq!(owners_in(root.path()).len(), 1);

        for suffix in [format!("0{uid}"), format!("+{uid}"), format!("+00{uid}")] {
            let alias = root
                .path()
                .join(format!("{}{suffix}", codec::SHARE_SOCKET_DIR_PREFIX));
            std::fs::write(alias, []).unwrap();
        }
        let found = owners_in(root.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].uid, uid);
        assert_eq!(found[0].socket, dir.join(codec::SHARE_SOCKET_NAME));

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(owners_in(root.path()).is_empty());

        let moved = root.path().join("original");
        std::fs::rename(&dir, &moved).unwrap();
        std::fs::set_permissions(&moved, std::fs::Permissions::from_mode(0o711)).unwrap();
        std::os::unix::fs::symlink(&moved, &dir).unwrap();
        assert!(owners_in(root.path()).is_empty());
    }

    #[test]
    fn another_version_is_told_from_other_failures() {
        let refused: anyhow::Result<()> = Err(crate::client::IncompatibleVersionError {
            version: "older".into(),
            codec_vers: 1,
        }
        .into());
        let err = refused
            .context("the other account's ThinkTerm did not answer as this version does")
            .unwrap_err();
        assert!(is_other_version(&err));
        assert!(!is_other_version(&anyhow!("no answer within 3s")));
    }
}
