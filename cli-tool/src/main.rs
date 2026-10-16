use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use comfy_table::presets::UTF8_FULL;
use comfy_table::Table;
use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::{OwoColorize, Stream};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::Deserialize;
use thiserror::Error;

/// Requests that answer inside this window never show a spinner.
const SLOW_AFTER: Duration = Duration::from_millis(200);

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
    #[error("{url} did not answer within {timeout:?}")]
    Timeout {
        url: String,
        timeout: Duration,
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

    #[error("could not read {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not parse {path}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error(transparent)]
    Request(#[from] reqwest::Error),
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    /// Request timeout in seconds.
    timeout: u64,

    /// How responses are printed: "table" (default) or "plain".
    format: Format,

    /// Headers sent with every request, unless -H overrides them.
    headers: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            timeout: 10,
            format: Format::Table,
            headers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Format {
    #[default]
    Table,
    Plain,
}

fn load_config() -> Result<Config, HitError> {
    let Some(dirs) = directories::UserDirs::new() else {
        return Ok(Config::default());
    };
    let path = dirs.home_dir().join(".hit.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(HitError::ConfigRead { path, source: e }),
    };
    toml::from_str(&text).map_err(|source| HitError::ConfigParse { path, source })
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
    let config = load_config()?;
    let timeout = Duration::from_secs(config.timeout);

    let client = Client::builder().timeout(timeout).build()?;

    match cli.command {
        Command::Get { url, headers } => {
            if cli.verbose {
                eprintln!("GET {url}");
                for header in &headers {
                    eprintln!("  header: {header}");
                }
            }

            let mut request = add_default_headers(client.get(&url), &config.headers);
            for header in &headers {
                let (name, value) = header
                    .split_once(':')
                    .ok_or_else(|| HitError::InvalidHeader { header: header.clone() })?;
                request = request.header(name.trim(), value.trim());
            }

            let response = send(request, &url, timeout)?;
            print_response(response, &config.format)?;
        }

        Command::Post { url, body } => {
            if cli.verbose {
                eprintln!("POST {url}");
            }

            let mut request = add_default_headers(client.post(&url), &config.headers);
            if let Some(body) = body {
                request = request.body(body);
            }

            let response = send(request, &url, timeout)?;
            print_response(response, &config.format)?;
        }
    }

    Ok(())
}

fn add_default_headers(
    request: RequestBuilder,
    headers: &BTreeMap<String, String>,
) -> RequestBuilder {
    let mut request = request;
    for (name, value) in headers {
        request = request.header(name, value);
    }
    request
}

fn send(request: RequestBuilder, url: &str, timeout: Duration) -> Result<Response, HitError> {
    let spinner = start_spinner(url);

    let result = request.send().map_err(|error| match error {
        e if e.is_timeout() => HitError::Timeout {
            url: url.to_string(),
            timeout,
            source: e,
        },
        e if e.is_connect() => HitError::Connect {
            url: url.to_string(),
            source: e,
        },
        e => HitError::Request(e),
    });

    spinner.finish_and_clear();
    result
}

fn start_spinner(url: &str) -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    // The request blocks this thread, so a watchdog thread decides whether
    // the wait is long enough to be worth showing. Nothing is drawn until
    // the watchdog speaks: a fast request leaves no trace on the terminal.
    let url = url.to_string();
    let watchdog_spinner = spinner.clone();
    thread::spawn(move || {
        thread::sleep(SLOW_AFTER);
        if !watchdog_spinner.is_finished() {
            watchdog_spinner.set_message(format!("waiting for {url}"));
            watchdog_spinner.enable_steady_tick(Duration::from_millis(100));
        }
    });

    spinner
}

fn print_response(response: Response, format: &Format) -> Result<(), HitError> {
    println!("{}", paint_status(response.status()));

    if let Format::Table = format {
        let mut table = Table::new();
        table.load_style(UTF8_FULL);
        table.set_header(vec!["Header", "Value"]);
        for (name, value) in response.headers() {
            table.add_row(vec![name.as_str(), value.to_str().unwrap_or("<binary>")]);
        }
        println!("{table}");
    }

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
