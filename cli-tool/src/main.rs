use std::error::Error;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use comfy_table::presets::UTF8_FULL;
use comfy_table::Table;
use owo_colors::{OwoColorize, Stream};
use reqwest::blocking::{Client, RequestBuilder, Response};
use thiserror::Error;

#[derive(Parser)]
#[command(name = "hit", version, about = "A tiny command-line HTTP client")]
struct Cli {
    /// Print what we are about to do
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send a GET request
    Get {
        /// The URL to request
        url: String,

        /// Send this header, for example -H "Accept: application/json"
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
    },

    /// Send a POST request with a body
    Post {
        /// The URL to request
        url: String,

        /// Request body
        #[arg(short, long)]
        body: Option<String>,
    },
}

#[derive(Debug, Error)]
enum HitError {
    #[error("{url} did not answer within 10 seconds")]
    Timeout {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("could not reach {url}")]
    Connect {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("invalid --header {header:?}: expected \"Name: value\"")]
    InvalidHeader { header: String },

    #[error(transparent)]
    Request(#[from] reqwest::Error),
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hit: {e}");
            let mut source = e.source();
            while let Some(err) = source {
                eprintln!("  because: {err}");
                source = err.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), HitError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    match cli.command {
        Command::Get { url, headers } => {
            if cli.verbose {
                eprintln!("GET {url}");
                for header in &headers {
                    eprintln!("  header: {header}");
                }
            }

            let mut request = client.get(&url);
            for header in &headers {
                let (name, value) = header
                    .split_once(':')
                    .ok_or_else(|| HitError::InvalidHeader { header: header.clone() })?;
                request = request.header(name.trim(), value.trim());
            }

            let response = send(request, &url)?;
            print_response(response)?;
        }

        Command::Post { url, body } => {
            if cli.verbose {
                eprintln!("POST {url}");
            }

            let mut request = client.post(&url);
            if let Some(body) = body {
                request = request.body(body);
            }

            let response = send(request, &url)?;
            print_response(response)?;
        }
    }

    Ok(())
}

fn send(request: RequestBuilder, url: &str) -> Result<Response, HitError> {
    request.send().map_err(|error| match error {
        e if e.is_timeout() => HitError::Timeout {
            url: url.to_string(),
            source: e,
        },
        e if e.is_connect() => HitError::Connect {
            url: url.to_string(),
            source: e,
        },
        e => HitError::Request(e),
    })
}

fn print_response(response: Response) -> Result<(), HitError> {
    println!("{}", paint_status(response.status()));

    let mut table = Table::new();
    table.load_style(UTF8_FULL);
    table.set_header(vec!["Header", "Value"]);
    for (name, value) in response.headers() {
        table.add_row(vec![name.as_str(), value.to_str().unwrap_or("<binary>")]);
    }
    println!("{table}");

    println!();
    let body = response.text()?;
    println!("{body}");
    Ok(())
}

fn paint_status(status: reqwest::StatusCode) -> String {
    let text = status.to_string();
    if status.is_success() {
        text.if_supports_color(Stream::Stdout, |t| t.green()).to_string()
    } else if status.is_redirection() {
        text.if_supports_color(Stream::Stdout, |t| t.cyan()).to_string()
    } else if status.is_client_error() {
        text.if_supports_color(Stream::Stdout, |t| t.yellow()).to_string()
    } else if status.is_server_error() {
        text.if_supports_color(Stream::Stdout, |t| t.red()).to_string()
    } else {
        text
    }
}
