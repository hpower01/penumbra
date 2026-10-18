/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025-2026 Shomy
*/
#[macro_use]
mod macros;

#[cfg(feature = "tui")]
mod app;
#[cfg(feature = "tui")]
mod components;
#[cfg(feature = "tui")]
mod pages;
#[cfg(feature = "tui")]
mod themes;

mod auth;
mod cli;
mod config;
mod helpers;
mod logger;

use anyhow::Result;
use clap::Parser;
use cli::{CliArgs, run_cli, run_cli_multi};
use logger::init_logger;

use crate::auth::init_auth;
use crate::config::AntumbraConfig;

fn main() -> Result<()> {
    // `multi` is a mode keyword typed before the command (e.g.
    // `antumbra multi wf boot boot.img`). It isn't a clap subcommand, so strip
    // the first standalone `multi` token before parsing and remember it.
    let mut multi = false;
    let filtered_args: Vec<std::ffi::OsString> = std::env::args_os()
        .enumerate()
        .filter(|(i, a)| {
            if *i != 0 && !multi && a.to_str() == Some("multi") {
                multi = true;
                false
            } else {
                true
            }
        })
        .map(|(_, a)| a)
        .collect();

    let args = CliArgs::parse_from(filtered_args);

    #[cfg(all(windows, feature = "tui"))]
    let tui = args.tui || !cli_or_gui::is_launched_from_terminal();

    #[cfg(not(all(windows, feature = "tui")))]
    let tui = args.tui;

    init_logger(tui, args.verbose);

    let config = AntumbraConfig::load()?;

    init_auth(config.clone())?;

    if !tui || !cfg!(feature = "tui") {
        if multi {
            return run_cli_multi(args, &config);
        }
        return run_cli(&args, &config);
    }

    if multi {
        anyhow::bail!("multi-device mode is only available in CLI mode, not the TUI");
    }

    #[cfg(feature = "tui")]
    {
        use app::App;

        let mut terminal = ratatui::init();
        let mut app = App::new(&args, config);

        let app_result = app.run(&mut terminal);

        ratatui::restore();
        app_result
    }

    #[cfg(not(feature = "tui"))]
    unreachable!()
}
