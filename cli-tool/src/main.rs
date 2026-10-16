use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use comfy_table::presets::UTF8_FULL;
use comfy_table::Table;
use futures::future::join_all;
use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::{OwoColorize, Stream};
use reqwest::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
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

    /// Save a GET request to reuse later
    Save {
        /// Name to store the request under
        name: String,

        /// The URL to request
        url: String,

        /// Send this header, for example -H "Accept: application/json"
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
    },

    /// Send saved requests; more than one runs concurrently
    Run {
        /// The names they were saved under
        names: Vec<String>,
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

    #[error("invalid name {name:?}: use a plain file name, no path separators")]
    InvalidName { name: String },

    #[error("could not locate a data directory")]
    NoDataDir,

    #[error("could not write {path}: {source}")]
    SaveWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not encode the request for {name:?}")]
    SaveEncode {
        name: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("no saved request named {name:?}")]
    NoSuchRequest { name: String },

    #[error("could not read {path}: {source}")]
    LoadRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not parse {path}")]
    LoadParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
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

#[derive(Debug, Serialize, Deserialize)]
struct SavedRequest {
    url: String,
    headers: Vec<String>,
}

fn requests_dir() -> Option<PathBuf> {
    let dirs = directories::BaseDirs::new()?;
    Some(dirs.data_dir().join("hit"))
}

fn request_path(name: &str) -> Result<PathBuf, HitError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return Err(HitError::InvalidName {
            name: name.to_string(),
        });
    }
    let Some(dir) = requests_dir() else {
        return Err(HitError::NoDataDir);
    };
    Ok(dir.join(format!("{name}.json")))
}

fn save_request(name: &str, url: &str, headers: &[String]) -> Result<PathBuf, HitError> {
    let path = request_path(name)?;
    let saved = SavedRequest {
        url: url.to_string(),
        headers: headers.to_vec(),
    };
    let text = serde_json::to_string_pretty(&saved).map_err(|source| HitError::SaveEncode {
        name: name.to_string(),
        source,
    })?;
    let Some(dir) = path.parent() else {
        return Err(HitError::NoDataDir);
    };
    fs::create_dir_all(dir).map_err(|source| HitError::SaveWrite {
        path: path.clone(),
        source,
    })?;
    fs::write(&path, text).map_err(|source| HitError::SaveWrite {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

fn load_request(name: &str) -> Result<SavedRequest, HitError> {
    let path = request_path(name)?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(HitError::NoSuchRequest {
                name: name.to_string(),
            });
        }
        Err(e) => return Err(HitError::LoadRead { path, source: e }),
    };
    serde_json::from_str(&text).map_err(|source| HitError::LoadParse { path, source })
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

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(cli).await {
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

async fn run(cli: Cli) -> Result<(), HitError> {
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

            send_get(&client, &config, timeout, url, headers).await?;
        }

        Command::Post { url, body } => {
            if cli.verbose {
                eprintln!("POST {url}");
            }

            let mut request = add_default_headers(client.post(&url), &config.headers);
            if let Some(body) = body {
                request = request.body(body);
            }

            let response = send(request, &url, timeout).await?;
            print_response(response, &config.format).await?;
        }

        Command::Save { name, url, headers } => {
            let path = save_request(&name, &url, &headers)?;
            println!("saved {name} to {}", path.display());
        }

        Command::Run { names } => {
            if let [name] = names.as_slice() {
                let saved = load_request(name)?;
                if cli.verbose {
                    eprintln!("GET {}", saved.url);
                    for header in &saved.headers {
                        eprintln!("  header: {header}");
                    }
                }

                send_get(&client, &config, timeout, saved.url, saved.headers).await?;
            } else {
                run_many(&client, &config, timeout, names).await?;
            }
        }
    }

    Ok(())
}

async fn send_get(
    client: &Client,
    config: &Config,
    timeout: Duration,
    url: String,
    headers: Vec<String>,
) -> Result<(), HitError> {
    let request = build_request(client, config, &url, &headers)?;
    let response = send(request, &url, timeout).await?;
    print_response(response, &config.format).await?;
    Ok(())
}

fn build_request(
    client: &Client,
    config: &Config,
    url: &str,
    headers: &[String],
) -> Result<RequestBuilder, HitError> {
    let mut request = add_default_headers(client.get(url), &config.headers);
    for header in headers {
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| HitError::InvalidHeader { header: header.clone() })?;
        request = request.header(name.trim(), value.trim());
    }
    Ok(request)
}

async fn run_many(
    client: &Client,
    config: &Config,
    timeout: Duration,
    names: Vec<String>,
) -> Result<(), HitError> {
    let mut jobs = Vec::new();
    for name in names {
        let saved = load_request(&name)?;
        let request = build_request(client, config, &saved.url, &saved.headers)?;
        jobs.push((name, saved.url, request));
    }

    // Every request runs at once. Each future measures its own elapsed time
    // and keeps its error instead of returning early with one.
    let futures = jobs.into_iter().map(|(name, url, request)| async move {
        let start = Instant::now();
        let result = match request.send().await {
            Ok(response) => {
                let status = response.status();
                match response.text().await {
                    Ok(body) => Ok((status, body.len())),
                    Err(e) => Err(classify(e, &url, timeout)),
                }
            }
            Err(e) => Err(classify(e, &url, timeout)),
        };
        (name, start.elapsed(), result)
    });

    let mut error = None;
    for (name, elapsed, result) in join_all(futures).await {
        match result {
            Ok((status, bytes)) => println!(
                "{name:<10} {} · {} · {}",
                paint_status(status),
                fmt_bytes(bytes),
                fmt_elapsed(elapsed),
            ),
            Err(e) => error = error.or(Some(e)),
        }
    }

    match error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn classify(error: reqwest::Error, url: &str, timeout: Duration) -> HitError {
    if error.is_timeout() {
        HitError::Timeout {
            url: url.to_string(),
            timeout,
            source: error,
        }
    } else if error.is_connect() {
        HitError::Connect {
            url: url.to_string(),
            source: error,
        }
    } else {
        HitError::Request(error)
    }
}

fn fmt_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

fn fmt_elapsed(elapsed: Duration) -> String {
    if elapsed.as_secs() > 0 {
        format!("{:.2} s", elapsed.as_secs_f64())
    } else {
        format!("{} ms", elapsed.as_millis())
    }
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

async fn send(request: RequestBuilder, url: &str, timeout: Duration) -> Result<Response, HitError> {
    let spinner = start_spinner(url);

    let result = request
        .send()
        .await
        .map_err(|error| classify(error, url, timeout));

    spinner.finish_and_clear();
    result
}

fn start_spinner(url: &str) -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    // The request no longer blocks a thread, but the spinner still gets its
    // own plain thread: indicatif knows nothing about async, and it does not
    // need to. Nothing is drawn until the watchdog speaks: a fast request
    // leaves no trace on the terminal.
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

async fn print_response(response: Response, format: &Format) -> Result<(), HitError> {
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
    let body = response.text().await?;
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
