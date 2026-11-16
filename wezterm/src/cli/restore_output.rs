//! `thinkterm cli restore-output …`: whether this machine keeps what its
//! panes show when its terminals stop, and puts it back when their layout is
//! restored. The desktop's Settings has the same switch for the machine it
//! runs on, where it is on unless turned off; on any other machine, a remote
//! host say, it is off until this turns it on. See `mux::kept_output`.

use clap::Parser;

#[derive(Debug, Parser, Clone)]
pub struct RestoreOutputCommand {
    #[command(subcommand)]
    sub: RestoreOutputSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum RestoreOutputSubCommand {
    /// Show whether this machine keeps pane output.
    #[command(name = "status")]
    Status,

    /// Keep each pane's output when this machine's terminals stop (quit,
    /// shutdown, the session server stopping) and put it back when their
    /// layout is restored. Passwords and tokens are masked before saving.
    #[command(name = "on")]
    On,

    /// Stop keeping pane output, and delete what was kept.
    #[command(name = "off")]
    Off,
}

impl RestoreOutputCommand {
    pub fn run(&self) -> anyhow::Result<()> {
        match self.sub {
            RestoreOutputSubCommand::Status => {}
            RestoreOutputSubCommand::On => mux::kept_output::set_enabled(true)?,
            RestoreOutputSubCommand::Off => mux::kept_output::set_enabled(false)?,
        }
        if mux::kept_output::enabled() {
            println!("on: this machine keeps pane output when its terminals stop");
        } else {
            println!("off: this machine keeps no pane output");
        }
        Ok(())
    }
}
