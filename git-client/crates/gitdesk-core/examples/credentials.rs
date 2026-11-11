//! credentials: where the secrets for a fetch will come from, and
//! how they stay out of the logs. Remote URLs are classified by
//! transport: ssh asks the agent, https asks the credential
//! helpers (on macOS that means the keychain), a plain path asks
//! nothing. The ssh-agent wire protocol is spoken by hand — one
//! length-prefixed request, one reply parsed into key algorithms.
//! A Secret wrapper refuses to print, and a redactor scrubs any
//! leak that slips past it anyway.
//!
//! Run with `cargo run --example credentials` from the workspace
//! root. Nothing is written to disk; with no ssh-agent running
//! the agent step says so and moves on.

use std::env;
use std::fmt;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

// ---- three transports, three answers -------------------------------

/// How a remote URL authenticates — or why it does not have to.
enum Transport {
    /// `ssh://…` or the scp-like `git@host:path`. The private key
    /// never moves; the agent holds it and answers signing
    /// requests over $SSH_AUTH_SOCK.
    Ssh {
        user: String,
        host: String,
        port: Option<u16>,
    },
    /// `https://…`. The credential is a token, and it lives in a
    /// credential store — on macOS the keychain, reached through
    /// the helper protocol step 12 wires into fetch.
    Https { host: String },
    /// A plain path. Nothing to authenticate.
    Local,
}

fn classify(url: &str) -> Transport {
    if let Some(rest) = url.strip_prefix("https://") {
        // The authority ends at the first '/' or ':' after it.
        let end = rest.find(['/', ':']).unwrap_or(rest.len());
        return Transport::Https {
            host: rest[..end].to_string(),
        };
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        // ssh://[user@]host[:port]/path
        let end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..end];
        let (user, hostport) = match authority.split_once('@') {
            Some((u, hp)) => (u.to_string(), hp.to_string()),
            None => ("git".to_string(), authority.to_string()),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()),
            None => (hostport, None),
        };
        return Transport::Ssh { user, host, port };
    }
    // scp-like: [user@]host:path — a colon with a '/' after it,
    // and no leading one. Anything else is a plain path.
    if !url.starts_with('/') {
        if let Some((authority, path)) = url.split_once(':') {
            if !authority.is_empty() && path.contains('/') {
                let (user, host) = match authority.split_once('@') {
                    Some((u, h)) => (u.to_string(), h.to_string()),
                    None => ("git".to_string(), authority.to_string()),
                };
                return Transport::Ssh {
                    user,
                    host,
                    port: None,
                };
            }
        }
    }
    Transport::Local
}

fn describe(t: &Transport) {
    match t {
        Transport::Ssh { user, host, port } => {
            match port {
                Some(p) => println!("  ssh — user '{user}', host '{host}', port {p}; the"),
                None => println!("  ssh — user '{user}', host '{host}'; the"),
            }
            println!("  private key never moves; the agent holds it and answers signing");
            println!("  requests, so the question becomes: what does $SSH_AUTH_SOCK hold?");
        }
        Transport::Https { host } => {
            println!("  https — the credential is a token for '{host}', and it lives in a");
            println!("  credential store; on macOS that is the keychain, reached through");
            println!("  the helper protocol step 12 wires into fetch.");
        }
        Transport::Local => {
            println!("  a plain path — nothing to authenticate.");
        }
    }
}

// ---- asking the agent ----------------------------------------------

const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
const SSH_AGENT_IDENTITIES_ANSWER: u8 = 12;

/// An SSH agent message is one big-endian length followed by the
/// payload. The request for "list your keys" is a single byte.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

enum AgentError {
    NoAgent,
    Io(std::io::Error),
    Refused(u8),
    Malformed(&'static str),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::NoAgent => write!(f, "$SSH_AUTH_SOCK is not set — no agent to ask"),
            AgentError::Io(e) => write!(f, "the agent socket failed: {e}"),
            AgentError::Refused(code) => {
                write!(f, "the agent answered message {code}, not identities")
            }
            AgentError::Malformed(what) => write!(f, "the reply ended mid-field ({what})"),
        }
    }
}

fn read_u32(buf: &[u8], cursor: &mut usize) -> Result<u32, AgentError> {
    if buf.len() < *cursor + 4 {
        return Err(AgentError::Malformed("length"));
    }
    let v = u32::from_be_bytes(buf[*cursor..*cursor + 4].try_into().unwrap());
    *cursor += 4;
    Ok(v)
}

fn read_bytes(buf: &[u8], cursor: &mut usize) -> Result<Vec<u8>, AgentError> {
    let n = read_u32(buf, cursor)? as usize;
    if buf.len() < *cursor + n {
        return Err(AgentError::Malformed("string body"));
    }
    let v = buf[*cursor..*cursor + n].to_vec();
    *cursor += n;
    Ok(v)
}

/// Connect to the agent, send the identities request, parse the
/// answer into (algorithm, blob length) pairs. Comments are read
/// and dropped — they never reach a log line.
fn list_identities() -> Result<Vec<(String, usize)>, AgentError> {
    let sock = env::var_os("SSH_AUTH_SOCK").ok_or(AgentError::NoAgent)?;
    let mut stream = UnixStream::connect(sock).map_err(AgentError::Io)?;
    stream
        .write_all(&frame(&[SSH_AGENTC_REQUEST_IDENTITIES]))
        .map_err(AgentError::Io)?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(AgentError::Io)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut reply = vec![0u8; len];
    stream.read_exact(&mut reply).map_err(AgentError::Io)?;

    if reply[0] != SSH_AGENT_IDENTITIES_ANSWER {
        return Err(AgentError::Refused(reply[0]));
    }
    let mut cursor = 1;
    let n = read_u32(&reply, &mut cursor)?;
    let mut keys = Vec::new();
    for _ in 0..n {
        let blob = read_bytes(&reply, &mut cursor)?;
        let _comment = read_bytes(&reply, &mut cursor)?;
        // The blob is itself SSH wire format; its first string is
        // the key's algorithm name.
        if blob.len() < 4 {
            return Err(AgentError::Malformed("blob"));
        }
        let n = u32::from_be_bytes(blob[..4].try_into().unwrap()) as usize;
        if blob.len() < 4 + n {
            return Err(AgentError::Malformed("algorithm name"));
        }
        keys.push((String::from_utf8_lossy(&blob[4..4 + n]).into_owned(), blob.len()));
    }
    Ok(keys)
}

// ---- secrets that refuse to print ----------------------------------

/// A token or passphrase. `Debug` and `Display` both mask it, so
/// a log macro or an accidental `println!("{cred:?}")` prints the
/// wrapper, never the bytes; `expose()` is the one deliberate way
/// out, for the single place a credential enters a request.
struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(***)")
    }
}

impl Secret {
    fn expose(&self) -> &str {
        &self.0
    }
}

/// What an https fetch actually sends: a username and a token.
/// The derived `Debug` masks the token automatically.
#[derive(Debug)]
struct HttpsCredential {
    username: String,
    token: Secret,
}

/// Last line of defence: scrub every known secret out of a line
/// before the log writes it. Cheap, and it catches the leak that
/// a stray `format!` already committed to.
fn redact(line: &str, secrets: &[&Secret]) -> String {
    let mut out = line.to_string();
    for s in secrets {
        out = out.replace(s.expose(), "[redacted]");
    }
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let remotes = [
        "https://github.com/hackingwithrust/examples.git",
        "git@github.com:hackingwithrust/examples.git",
        "ssh://git@ssh.github.com:443/hackingwithrust/examples.git",
        "../elsewhere/demo-repo",
    ];
    println!("three transports, three answers:");
    for url in remotes {
        println!("{url}");
        let t = classify(url);
        describe(&t);
    }

    println!("\nssh-agent — asking $SSH_AUTH_SOCK for its identities:");
    let request = frame(&[SSH_AGENTC_REQUEST_IDENTITIES]);
    println!("  request frame: {} (SSH_AGENTC_REQUEST_IDENTITIES)", hex(&request));
    match list_identities() {
        Ok(keys) => {
            let plural = if keys.len() == 1 { "y" } else { "ies" };
            println!("  the agent answered with {} identit{}:", keys.len(), plural);
            for (algo, blob_len) in keys {
                println!("    {algo} ({blob_len}-byte blob)");
            }
        }
        Err(e) => println!("  {e}"),
    }

    println!("\nsecrets that refuse to print:");
    let cred = HttpsCredential {
        username: "octocat".to_string(),
        token: Secret("ghp_16C7e42F291c4d3382bb55000f0bde4a1f3d92c".to_string()),
    };
    println!("  {{:?}} of a credential holding one: {cred:?}");
    println!("  Display masks it too: {}", cred.token);
    let careless = format!(
        "handshake ok user={} host=github.com token={}",
        cred.username,
        cred.token.expose()
    );
    println!("  a line built the careless way, before the redactor:");
    println!("    {careless}");
    println!("  the same line, after the redactor:");
    println!("    {}", redact(&careless, &[&cred.token]));
    Ok(())
}
