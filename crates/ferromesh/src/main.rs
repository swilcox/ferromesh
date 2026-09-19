//! ferromesh: the command-line client for a ferromeshd server.

mod channels;
mod config;
mod direct;
mod render;
mod server;
mod tui;
mod when;

use std::io;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use ferromesh_model::{Filter, GuessChannels, Kind};

use crate::config::Settings;
use crate::render::Printer;
use crate::server::{Server, Start};
use crate::when::When;

const FILTER_HELP: &str = "\
Filters are space-separated terms that must all match:
  chan:#test,#wx   channel, any of          from:BNA*        sender name, * wildcard
  storm            words in the message     text:\"storm warning\"
  type:advert      packet type              node:4d1727      advert public key prefix
  observer:Tanyard observer name            'snr>-5' 'rssi<-100' 'hops>2'
A leading - negates a term. Quote > and < from the shell, and put double quotes
around values with spaces: 'from:\"BNA Bot\"'. One quoted argument can hold a
whole filter: 'type:advert snr>-5'.

Defaults for --server and --token can go in ~/.config/ferromesh/config.toml:
  server = \"truffles.local\"";

#[derive(Parser)]
#[command(version, about, after_help = FILTER_HELP)]
struct Cli {
    /// The ferromeshd server: a URL, or host[:port]. [default: localhost]
    #[arg(long, short, global = true, env = "FERROMESH_SERVER")]
    server: Option<String>,

    /// Token for changes such as adding channels (the server's api.token).
    #[arg(long, global = true, env = "FERROMESH_TOKEN", hide_env_values = true)]
    token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show recent traffic, then follow it live, reconnecting as needed.
    Tail {
        /// Filter terms (see below).
        filter: Vec<String>,
        /// messages, packets or observations.
        #[arg(long, short, default_value_t = Kind::Messages)]
        kind: Kind,
        /// How many recent matches to show first.
        #[arg(long, short = 'n', default_value_t = 20)]
        last: usize,
        /// Instead of --last, show everything since a time (RFC 3339) or a
        /// duration ago (90m, 6h, 2d).
        #[arg(long, conflicts_with = "live")]
        since: Option<When>,
        /// Skip history and show only new traffic.
        #[arg(long)]
        live: bool,
        /// Print one JSON event per line.
        #[arg(long)]
        json: bool,
    },
    /// Print matching history, oldest first, and exit.
    Query {
        /// Filter terms (see below).
        filter: Vec<String>,
        /// messages, packets or observations.
        #[arg(long, short, default_value_t = Kind::Messages)]
        kind: Kind,
        /// How many of the newest matches to fetch (at most 1000).
        #[arg(long, short = 'n', default_value_t = 100)]
        limit: usize,
        /// Only since a time (RFC 3339) or a duration ago (90m, 6h, 2d).
        #[arg(long)]
        since: Option<When>,
        /// Only until a time or a duration ago.
        #[arg(long)]
        until: Option<When>,
        /// Print one JSON event per line.
        #[arg(long)]
        json: bool,
    },
    /// Direct messages sent to your companion radio, oldest first.
    Dms {
        /// How many of the newest to show (at most 1000).
        #[arg(long, short = 'n', default_value_t = 50)]
        limit: usize,
        /// Print one JSON message per line.
        #[arg(long)]
        json: bool,
    },
    /// The channels the server decrypts, and finding more. Lists them by default.
    Channels {
        #[command(subcommand)]
        command: Option<ChannelsCommand>,
    },
    /// Browse channels, packets, RF and nodes live, full screen. Press ? inside
    /// for keys. Watches are kept in ~/.config/ferromesh/watches.toml.
    Tui {
        /// Print one screen as plain text once everything has loaded, and exit.
        #[arg(long)]
        snapshot: bool,
        /// With --snapshot, keys to press first: characters as typed, plus
        /// <enter>, <esc>, <tab>, <up>, <down>, <pgup>, <pgdn>, <home>, <end>,
        /// <bs> and <lt>.
        #[arg(long, requires = "snapshot")]
        keys: Option<String>,
        /// With --snapshot, the screen size. [default: 120x40]
        #[arg(long, requires = "snapshot", value_parser = parse_size)]
        size: Option<(u16, u16)>,
    },
}

#[derive(Subcommand)]
enum ChannelsCommand {
    /// List the channels the server decrypts.
    List {
        /// Print one JSON channel per line.
        #[arg(long)]
        json: bool,
    },
    /// Add a channel and decrypt stored traffic that was waiting for it.
    /// Quote the name in the shell: '#wx'.
    Add {
        /// `#name` for a hashtag channel, or any name with --key.
        name: String,
        /// A private channel's secret, in hex (as in MeshCore QR codes) or base64.
        #[arg(long)]
        key: Option<String>,
    },
    /// Channel hashes on stored traffic that no known key opens.
    Unknown {
        /// Print one JSON entry per line.
        #[arg(long)]
        json: bool,
    },
    /// Try hashtag names against undecrypted traffic.
    Guess {
        /// Names to try, with or without #.
        names: Vec<String>,
        /// Skip the server's list of common names.
        #[arg(long)]
        no_builtin: bool,
        /// Skip hashtags mentioned in decoded messages.
        #[arg(long)]
        no_mentions: bool,
        /// Add every channel found.
        #[arg(long)]
        add: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        // Piping into `head` closes stdout early; that isn't a failure.
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("ferromesh: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let settings = match config::dir() {
        Some(dir) => config::load_settings(&dir)?,
        None => Settings::default(),
    };
    let server = Server::new(cli.server.or(settings.server).as_deref().unwrap_or("localhost"));
    let token = cli.token.or(settings.token);
    let token = token.as_deref();
    match cli.command {
        Command::Tail { filter, kind, last, since, live, json } => {
            let filter = filter_text(&filter, kind)?;
            let start = match (live, since) {
                (true, _) => Start::Live,
                (false, Some(since)) => Start::Since(since.at()),
                (false, None) => Start::Last(last),
            };
            server::tail(&server, kind, filter, start, &mut Printer::new(json)).await
        }
        Command::Query { filter, kind, limit, since, until, json } => {
            let filter = filter_text(&filter, kind)?;
            let window = (since.map(When::at), until.map(When::at));
            server::query(&server, kind, filter, limit, window, Printer::new(json)).await
        }
        Command::Dms { limit, json } => direct::list(&server, limit, json).await,
        Command::Channels { command } => match command
            .unwrap_or(ChannelsCommand::List { json: false })
        {
            ChannelsCommand::List { json } => channels::list(&server, json).await,
            ChannelsCommand::Add { name, key } => channels::add(&server, name, key, token).await,
            ChannelsCommand::Unknown { json } => channels::unknown(&server, json).await,
            ChannelsCommand::Guess { names, no_builtin, no_mentions, add } => {
                let request = GuessChannels { names, builtin: !no_builtin, mentions: !no_mentions };
                channels::guess(&server, request, add, token).await
            }
        },
        Command::Tui { snapshot, keys, size } => {
            let snapshot = if snapshot {
                let (width, height) = size.unwrap_or((120, 40));
                let keys = tui::parse_keys(keys.as_deref().unwrap_or_default())?;
                Some(tui::Snapshot { width, height, keys })
            } else {
                None
            };
            tui::run(server, snapshot).await
        }
    }
}

/// Joins the filter arguments into one filter and checks it before contacting
/// the server. An argument may hold one term or several; values with spaces
/// need their own double quotes, since the shell's quotes are already gone.
fn filter_text(args: &[String], kind: Kind) -> Result<Option<String>> {
    let text = args.join(" ");
    let filter: Filter = text.parse()?;
    filter.validate(kind)?;
    Ok((!filter.is_empty()).then_some(text))
}

/// `WIDTHxHEIGHT`, as for `--size 120x40`.
fn parse_size(text: &str) -> Result<(u16, u16)> {
    let size = text.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
    match size {
        Some((width, height)) if width >= 40 && height >= 10 => Ok((width, height)),
        Some(_) => bail!("the screen must be at least 40x10"),
        None => bail!("expected WIDTHxHEIGHT, such as 120x40"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn filter_arguments_join_and_are_checked_locally() {
        assert_eq!(
            filter_text(&args(&["chan:#test", r#"from:"BNA Bot""#]), Kind::Messages).unwrap(),
            Some(r#"chan:#test from:"BNA Bot""#.to_owned())
        );
        assert_eq!(filter_text(&[], Kind::Messages).unwrap(), None);
        assert!(filter_text(&args(&["snr>1"]), Kind::Messages).is_err());
        assert!(filter_text(&args(&["colour:red"]), Kind::Packets).is_err());
    }

    #[test]
    fn one_argument_can_hold_several_terms() {
        let text = filter_text(&args(&["type:advert snr>-5"]), Kind::Observations).unwrap();
        let filter: Filter = text.unwrap().parse().unwrap();
        assert_eq!(filter.terms().len(), 2);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("120x40").unwrap(), (120, 40));
        assert!(parse_size("20x5").is_err());
        assert!(parse_size("wide").is_err());
    }

    #[test]
    fn snapshot_options_need_snapshot() {
        assert!(Cli::try_parse_from(["ferromesh", "tui", "--keys", "3"]).is_err());
        assert!(Cli::try_parse_from(["ferromesh", "tui", "--snapshot", "--keys", "3"]).is_ok());
    }
}
