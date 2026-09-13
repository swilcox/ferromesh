//! ferromesh: the command-line client for a ferromeshd server.

mod render;
mod server;
mod when;

use std::io;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use ferromesh_model::{Filter, Kind};

use crate::render::Printer;
use crate::server::{Server, Start};
use crate::when::When;

const FILTER_HELP: &str = "\
Filters are space-separated terms that must all match:
  chan:#test,#wx   channel, any of          from:BNA*        sender name, * wildcard
  storm            words in the message     text:\"storm warning\"
  type:advert      packet type              node:4d1727      advert public key prefix
  observer:Tanyard observer name            'snr>-5' 'rssi<-100' 'hops>2'
A leading - negates a term. Quote anything with spaces, > or <.";

#[derive(Parser)]
#[command(version, about, after_help = FILTER_HELP)]
struct Cli {
    /// The ferromeshd server: a URL, or host[:port].
    #[arg(long, short, global = true, env = "FERROMESH_SERVER", default_value = "localhost")]
    server: String,

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
    let server = Server::new(&cli.server);
    match cli.command {
        Command::Tail { filter, kind, last, since, live, json } => {
            let filter = filter_text(&filter, kind)?;
            let start = match (live, since) {
                (true, _) => Start::Live,
                (false, Some(since)) => Start::Since(since.at()),
                (false, None) => Start::Last(last),
            };
            server::tail(&server, kind, filter, start, Printer::new(json)).await
        }
        Command::Query { filter, kind, limit, since, until, json } => {
            let filter = filter_text(&filter, kind)?;
            let window = (since.map(When::at), until.map(When::at));
            server::query(&server, kind, filter, limit, window, Printer::new(json)).await
        }
    }
}

/// Joins the filter arguments into one filter, re-quoting values the shell
/// unquoted, and checks it before contacting the server.
fn filter_text(args: &[String], kind: Kind) -> Result<Option<String>> {
    let text = args.iter().map(|arg| requote(arg)).collect::<Vec<_>>().join(" ");
    let filter: Filter = text.parse()?;
    filter.validate(kind)?;
    Ok((!filter.is_empty()).then_some(text))
}

/// `from:BNA Bot` (the shell ate the quotes) becomes `from:"BNA Bot"`.
fn requote(arg: &str) -> String {
    if !arg.contains(char::is_whitespace) || arg.contains('"') {
        return arg.to_owned();
    }
    match arg.split_once(':') {
        Some((key, value))
            if !key.trim_start_matches('-').is_empty()
                && key.trim_start_matches('-').chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            format!("{key}:\"{value}\"")
        }
        _ => format!("\"{arg}\""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requotes_shell_split_values() {
        assert_eq!(requote("from:BNA Bot"), r#"from:"BNA Bot""#);
        assert_eq!(requote("-from:BNA Bot"), r#"-from:"BNA Bot""#);
        assert_eq!(requote("storm warning"), r#""storm warning""#);
        assert_eq!(requote("chan:#test"), "chan:#test");
        assert_eq!(requote(r#"from:"BNA Bot""#), r#"from:"BNA Bot""#);
    }

    #[test]
    fn filter_arguments_are_checked_locally() {
        let args = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            filter_text(&args(&["chan:#test", "from:BNA Bot"]), Kind::Messages).unwrap(),
            Some(r#"chan:#test from:"BNA Bot""#.to_owned())
        );
        assert_eq!(filter_text(&[], Kind::Messages).unwrap(), None);
        assert!(filter_text(&args(&["snr>1"]), Kind::Messages).is_err());
        assert!(filter_text(&args(&["colour:red"]), Kind::Packets).is_err());
    }
}
