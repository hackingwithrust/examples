use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::blocking::{Client, Response};

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

fn main() -> Result<()> {
    let cli = Cli::parse();

    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build the HTTP client")?;

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
                let (name, value) = header.split_once(':').with_context(|| {
                    format!("invalid --header {header:?}: expected \"Name: value\"")
                })?;
                request = request.header(name.trim(), value.trim());
            }

            let response = request.send()?;
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

            let response = request.send()?;
            print_response(response)?;
        }
    }

    Ok(())
}

fn print_response(response: Response) -> Result<()> {
    println!("{}", response.status());
    for (name, value) in response.headers() {
        println!("{name}: {}", value.to_str().unwrap_or("<binary>"));
    }
    println!();
    let body = response.text()?;
    println!("{body}");
    Ok(())
}
