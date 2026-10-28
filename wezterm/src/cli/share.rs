//! `thinkterm cli share …`: the shares of your panes, and the panes other
//! accounts on this machine show you.
//!
//! A pane is shared, and a viewer let type, only from the share button in
//! ThinkTerm: a person decides that, not a program in a pane. What is left
//! here takes sharing back -- stop, kick, revoke, deny -- or looks.
//!
//! The owner's commands talk to the owner's own server. `incoming` and
//! `watch` talk to other accounts' servers instead, each on the socket it
//! shares from, where that server reads who is asking from the socket.

use crate::cli::{printable, CliOutputFormatKind, NoSuchTarget, UsageError};
use clap::Parser;
use codec::{ShareInfo, ShareKick, ShareRevoke, ShareStop};
use std::time::SystemTime;
use tabout::{tabulate_output, Alignment, Column};
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct ShareCommand {
    #[command(subcommand)]
    sub: ShareSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum ShareSubCommand {
    /// List your shares: who watches, who may type, who asks to. A share
    /// is started from the share button in ThinkTerm.
    #[command(name = "list")]
    List(ListCommand),

    /// End a share; everyone watching it is dropped.
    #[command(name = "stop")]
    Stop(StopCommand),

    /// Drop one viewer from a share.
    #[command(name = "kick")]
    Kick(KickCommand),

    /// Take typing back.
    #[command(name = "revoke")]
    Revoke(RevokeCommand),

    /// Turn down a viewer's request to type; it goes on watching.
    #[command(name = "deny")]
    Deny(KickCommand),

    /// The panes other accounts on this machine share with you.
    #[command(name = "incoming")]
    Incoming(IncomingCommand),

    /// Watch a pane someone shares with you.
    #[command(name = "watch")]
    Watch(crate::cli::share_watch::WatchCommand),
}

#[derive(Debug, Parser, Clone)]
struct ListCommand {
    /// Controls the output format.
    /// "table" and "json" are possible formats.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,
}

#[derive(Debug, Parser, Clone)]
struct StopCommand {
    /// The share, as `list` shows it.
    share_id: u64,
}

#[derive(Debug, Parser, Clone)]
struct KickCommand {
    share_id: u64,
    /// The viewer: its id as `list` shows it, or its account name.
    viewer: String,
}

#[derive(Debug, Parser, Clone)]
struct RevokeCommand {
    share_id: u64,
}

#[derive(Debug, Parser, Clone)]
struct IncomingCommand {
    /// Controls the output format.
    /// "table" and "json" are possible formats.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,
}

impl ShareCommand {
    /// Whether this asks other accounts' servers rather than our own.
    pub fn is_viewer(&self) -> bool {
        matches!(
            self.sub,
            ShareSubCommand::Incoming(_) | ShareSubCommand::Watch(_)
        )
    }

    /// `incoming` and `watch`, which need no connection to our own server.
    pub async fn run_viewer(&self) -> anyhow::Result<()> {
        match &self.sub {
            ShareSubCommand::Incoming(cmd) => cmd.run().await,
            ShareSubCommand::Watch(cmd) => cmd.run().await,
            _ => unreachable!("owner commands run with a client"),
        }
    }

    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        match &self.sub {
            ShareSubCommand::List(cmd) => cmd.run(client).await,
            ShareSubCommand::Stop(cmd) => {
                client
                    .share_stop(ShareStop {
                        share_id: cmd.share_id,
                    })
                    .await?;
                Ok(())
            }
            ShareSubCommand::Kick(cmd) => {
                let viewer_id = find_viewer(&client, cmd.share_id, &cmd.viewer).await?;
                client
                    .share_kick(ShareKick {
                        share_id: cmd.share_id,
                        viewer_id,
                    })
                    .await?;
                Ok(())
            }
            ShareSubCommand::Revoke(cmd) => {
                client
                    .share_revoke(ShareRevoke {
                        share_id: cmd.share_id,
                    })
                    .await?;
                Ok(())
            }
            ShareSubCommand::Deny(cmd) => {
                let viewer_id = find_viewer(&client, cmd.share_id, &cmd.viewer).await?;
                client
                    .share_deny(codec::ShareDeny {
                        share_id: cmd.share_id,
                        viewer_id,
                    })
                    .await?;
                Ok(())
            }
            ShareSubCommand::Incoming(_) | ShareSubCommand::Watch(_) => {
                unreachable!("viewer commands run without our own server")
            }
        }
    }
}

/// A viewer named by id or by account name; a name more than one viewer
/// has is ambiguous.
async fn find_viewer(client: &Client, share_id: u64, viewer: &str) -> anyhow::Result<u64> {
    if let Ok(id) = viewer.parse::<u64>() {
        return Ok(id);
    }
    let shares = client.share_list().await?.shares;
    let share = shares
        .iter()
        .find(|share| share.share_id == share_id)
        .ok_or_else(|| NoSuchTarget(format!("there is no share {share_id}")))?;
    let mut matching = share.viewers.iter().filter(|v| v.name == viewer);
    match (matching.next(), matching.next()) {
        (Some(one), None) => Ok(one.viewer_id),
        (None, _) => Err(NoSuchTarget(format!("{viewer} is not watching share {share_id}")).into()),
        (Some(_), Some(_)) => Err(UsageError(format!(
            "{viewer} watches share {share_id} more than once; name the viewer by id"
        ))
        .into()),
    }
}

fn ago(secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let delta = now.saturating_sub(secs);
    match delta {
        0..=59 => format!("{delta}s"),
        60..=3599 => format!("{}m", delta / 60),
        _ => format!("{}h", delta / 3600),
    }
}

fn until(secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let left = secs.saturating_sub(now);
    match left {
        0..=59 => format!("{left}s left"),
        60..=3599 => format!("{}m left", left / 60),
        _ => format!("{}h left", left / 3600),
    }
}

impl ListCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let shares = client.share_list().await?.shares;
        match self.format {
            CliOutputFormatKind::Json => {
                println!("{}", serde_json::to_string_pretty(&shares_json(&shares))?);
            }
            CliOutputFormatKind::Table => {
                if shares.is_empty() {
                    println!("You share no panes.");
                    return Ok(());
                }
                let cols = [
                    ("SHARE", Alignment::Right),
                    ("PANE", Alignment::Right),
                    ("FOR", Alignment::Left),
                    ("WATCHING", Alignment::Left),
                    ("TYPING", Alignment::Left),
                    ("ASKING", Alignment::Left),
                    ("ENDS", Alignment::Left),
                ]
                .map(|(name, alignment)| Column {
                    name: name.to_string(),
                    alignment,
                });
                let viewers = |list: &[codec::ShareViewer]| {
                    if list.is_empty() {
                        "-".to_string()
                    } else {
                        list.iter()
                            .map(|v| format!("{} ({})", v.name, v.viewer_id))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                };
                let rows = shares
                    .iter()
                    .map(|share| {
                        vec![
                            share.share_id.to_string(),
                            share.pane_id.to_string(),
                            share
                                .audience
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(", "),
                            viewers(&share.viewers),
                            match &share.controller {
                                Some(c) => match c.until {
                                    Some(at) => format!("{} ({})", c.name, until(at)),
                                    None => c.name.clone(),
                                },
                                None => "only you".to_string(),
                            },
                            viewers(&share.requests),
                            share
                                .expires
                                .map(until)
                                .unwrap_or_else(|| "when stopped".into()),
                        ]
                    })
                    .collect::<Vec<_>>();
                let mut out = std::io::stdout();
                tabulate_output(&cols, &rows, &mut out)?;
            }
        }
        Ok(())
    }
}

fn shares_json(shares: &[ShareInfo]) -> serde_json::Value {
    serde_json::Value::Array(
        shares
            .iter()
            .map(|share| {
                let viewer = |v: &codec::ShareViewer| {
                    serde_json::json!({ "viewer_id": v.viewer_id, "name": v.name, "since": v.since })
                };
                serde_json::json!({
                    "share_id": share.share_id,
                    "pane_id": share.pane_id,
                    "to": share.audience.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "history": share.history,
                    "started": share.started,
                    "expires": share.expires,
                    "viewers": share.viewers.iter().map(viewer).collect::<Vec<_>>(),
                    "typing": share.controller.as_ref().map(|c| serde_json::json!({
                        "viewer_id": c.viewer_id, "name": c.name, "until": c.until,
                    })),
                    "asking": share.requests.iter().map(viewer).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

impl IncomingCommand {
    async fn run(&self) -> anyhow::Result<()> {
        let incoming = {
            let _quiet = crate::cli::share_watch::Quiet::new();
            wezterm_client::share::incoming().await
        };
        // What they share cannot be read: say who, beside the list.
        for owner in &incoming.other_version {
            eprintln!(
                "{} runs another version of ThinkTerm: what it shares shows once both run the same.",
                printable(owner)
            );
        }
        let found = incoming.found;
        match self.format {
            CliOutputFormatKind::Json => {
                let list: Vec<_> = found
                    .iter()
                    .flat_map(|(_, response)| {
                        response.shares.iter().map(move |share| {
                            serde_json::json!({
                                "owner": response.owner,
                                "share_id": share.share_id,
                                "pane_id": share.pane_id,
                                "title": share.title,
                                "history": share.history,
                                "started": share.started,
                                "typing": share.controller,
                            })
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&list)?);
            }
            CliOutputFormatKind::Table => {
                let rows: Vec<Vec<String>> = found
                    .iter()
                    .flat_map(|(_, response)| {
                        // Another account's server chose the title and the
                        // names; nothing in them reaches this terminal as a
                        // control character.
                        response.shares.iter().map(move |share| {
                            vec![
                                printable(&format!("{}/{}", response.owner, share.share_id)),
                                printable(&share.title),
                                ago(share.started),
                                share
                                    .controller
                                    .as_deref()
                                    .map(printable)
                                    .unwrap_or_else(|| "-".into()),
                            ]
                        })
                    })
                    .collect();
                if rows.is_empty() {
                    if incoming.other_version.is_empty() {
                        println!("Nobody on this machine shares a pane with you.");
                    }
                    return Ok(());
                }
                let cols = [
                    ("SHARE", Alignment::Left),
                    ("TITLE", Alignment::Left),
                    ("SINCE", Alignment::Left),
                    ("TYPING", Alignment::Left),
                ]
                .map(|(name, alignment)| Column {
                    name: name.to_string(),
                    alignment,
                });
                let mut out = std::io::stdout();
                tabulate_output(&cols, &rows, &mut out)?;
                println!("Watch one with: thinkterm cli share watch OWNER/SHARE");
            }
        }
        Ok(())
    }
}
