use clap::{Parser, Subcommand};

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

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Get { url, headers } => {
            if cli.verbose {
                eprintln!("GET {url}");
                for header in &headers {
                    eprintln!("  header: {header}");
                }
            }
            println!("would GET {url}");
        }
        Command::Post { url, body } => {
            if cli.verbose {
                eprintln!("POST {url}");
            }
            println!("would POST {url} with body {body:?}");
        }
    }
}
