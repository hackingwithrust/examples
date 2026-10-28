//! Integration tests: the server binary as a child process, and a client
//! that speaks RESP from the outside. Nothing in here imports anything
//! from `src/main.rs` — a binary crate exports nothing, and that is the
//! point: these tests know only what a real client knows, a socket and
//! the protocol.

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The replies a client can receive, decoded from the wire.
#[derive(Debug, PartialEq)]
enum Reply {
    Simple(String),
    Error(String),
    Integer(i64),
    Bulk(Option<Vec<u8>>),
    Array(Vec<Reply>),
}

/// One connection to the server: the socket plus a read buffer, so a
/// reply split across two packets is reassembled the way the server
/// reassembles commands.
struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
    pos: usize,
}

/// Encode one command the way redis-cli does: an array of bulk strings.
fn encode(name: &str, args: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n${}\r\n{name}\r\n", args.len() + 1, name.len())
        .into_bytes();
    for arg in args {
        out.extend(format!("${}\r\n", arg.len()).into_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

impl Client {
    async fn connect(addr: SocketAddr) -> std::io::Result<Client> {
        Ok(Client {
            stream: TcpStream::connect(addr).await?,
            buf: Vec::new(),
            pos: 0,
        })
    }

    /// Read whatever the socket has, appended after what is already
    /// buffered. Zero bytes means the connection is gone.
    async fn fill(&mut self) -> std::io::Result<()> {
        let mut chunk = [0u8; 1024];
        let n = self.stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the server closed the connection",
            ));
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    /// One CRLF-terminated line, waiting for the rest of it if it has
    /// not all arrived yet.
    async fn line(&mut self) -> std::io::Result<Vec<u8>> {
        loop {
            if let Some(i) = self.buf[self.pos..].iter().position(|&b| b == b'\n') {
                let mut text = self.buf[self.pos..self.pos + i].to_vec();
                self.pos += i + 1;
                text.pop(); // the trailing \r
                return Ok(text);
            }
            self.fill().await?;
        }
    }

    /// Exactly `n` bytes of bulk data, then the CRLF that ends it.
    async fn take(&mut self, n: usize) -> std::io::Result<Vec<u8>> {
        while self.buf.len() - self.pos < n {
            self.fill().await?;
        }
        let data = self.buf[self.pos..self.pos + n].to_vec();
        self.pos += n;
        assert_eq!(self.line().await?, b"", "bulk data ends with CRLF");
        Ok(data)
    }

    /// One reply, decoded by its first byte. Arrays recurse.
    async fn reply(&mut self) -> std::io::Result<Reply> {
        let line = self.line().await?;
        let (kind, rest) = (line[0], &line[1..]);
        let text = String::from_utf8_lossy(rest).into_owned();
        match kind {
            b'+' => Ok(Reply::Simple(text)),
            b'-' => Ok(Reply::Error(text)),
            b':' => Ok(Reply::Integer(text.parse().expect("an integer"))),
            b'$' => {
                if text == "-1" {
                    Ok(Reply::Bulk(None))
                } else {
                    let n: usize = text.parse().expect("a length");
                    Ok(Reply::Bulk(Some(self.take(n).await?)))
                }
            }
            b'*' => {
                if text == "-1" {
                    Ok(Reply::Array(Vec::new()))
                } else {
                    let n: usize = text.parse().expect("a count");
                    let mut items = Vec::with_capacity(n);
                    for _ in 0..n {
                        // Boxed so the async fn does not recurse: the
                        // future's size would otherwise depend on itself.
                        items.push(Box::pin(self.reply()).await?);
                    }
                    Ok(Reply::Array(items))
                }
            }
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("not a RESP reply: {:?}", line),
            )),
        }
    }

    /// Send one command, read one reply.
    async fn cmd(&mut self, name: &str, args: &[&[u8]]) -> std::io::Result<Reply> {
        self.stream.write_all(&encode(name, args)).await?;
        self.reply().await
    }

    async fn set(&mut self, key: &str, value: &[u8]) -> std::io::Result<Reply> {
        self.cmd("SET", &[key.as_bytes(), value]).await
    }

    async fn get(&mut self, key: &str) -> std::io::Result<Reply> {
        self.cmd("GET", &[key.as_bytes()]).await
    }
}

/// The server binary, running on a port this test owns and in a
/// directory this test owns — so its snapshot file cannot collide with
/// another test's, and a `SAVE` in one test leaves nothing behind in
/// the next.
struct Server {
    child: Child,
    dir: PathBuf,
    port: u16,
    /// The read half of the server's stdout, held for the server's
    /// whole life. The sweeper prints to stdout when it reclaims a key,
    /// and if the read end were closed after the readiness line, that
    /// print would hit a broken pipe and kill the server mid-test.
    _stdout: std::io::BufReader<ChildStdout>,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

impl Server {
    async fn start() -> Server {
        let dir = std::env::temp_dir().join(format!(
            "mini-redis-it-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("makes the test directory");
        let mut child = Command::new(env!("CARGO_BIN_EXE_mini-redis"))
            .arg("0") // any free port; the ready line says which one
            .current_dir(&dir)
            .stdout(Stdio::piped())
            // The server's stderr carries per-connection error notes;
            // the tests provoke one on purpose and do not need it.
            .stderr(Stdio::null())
            .spawn()
            .expect("spawns the server binary");
        // The server prints `listening on 127.0.0.1:PORT` when it is
        // ready. Reading that line replaces every sleep-based readiness
        // hack: once it is printed, the port is bound and accepting.
        // Its absence at EOF means the child died starting up — bind
        // failure, bad argument — and the test should say so, not
        // connect to whatever comes next.
        let stdout = child.stdout.take().expect("stdout is piped");
        let mut reader = std::io::BufReader::new(stdout);
        let mut ready = String::new();
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).expect("reads the server's stdout");
            if n == 0 {
                break; // EOF: the child is gone
            }
            if line.starts_with("listening on") {
                ready = line.trim_end().to_string();
                break;
            }
        }
        assert!(
            !ready.is_empty(),
            "the server exited before it was ready: {:?}",
            child.wait()
        );
        let port: u16 = ready
            .rsplit(':')
            .next()
            .expect("the ready line has a port")
            .parse()
            .expect("the ready line ends with a port");
        Server {
            child,
            dir,
            port,
            _stdout: reader,
        }
    }

    fn addr(&self) -> SocketAddr {
        format!("127.0.0.1:{}", self.port).parse().expect("parses")
    }

    async fn client(&self) -> Client {
        Client::connect(self.addr()).await.expect("connects")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

#[tokio::test]
async fn the_server_answers_ping() {
    let server = Server::start().await;
    let mut client = server.client().await;
    assert_eq!(client.cmd("PING", &[]).await.unwrap(), Reply::Simple("PONG".into()));
}

#[tokio::test]
async fn a_set_is_visible_to_a_get_from_the_same_client() {
    let server = Server::start().await;
    let mut client = server.client().await;
    assert_eq!(client.set("a", b"1").await.unwrap(), Reply::Simple("OK".into()));
    assert_eq!(
        client.get("a").await.unwrap(),
        Reply::Bulk(Some(b"1".to_vec()))
    );
}

#[tokio::test]
async fn a_set_is_visible_to_a_second_connection() {
    let server = Server::start().await;
    let mut writer = server.client().await;
    writer.set("shared", b"hello").await.unwrap();
    let mut reader = server.client().await;
    assert_eq!(
        reader.get("shared").await.unwrap(),
        Reply::Bulk(Some(b"hello".to_vec()))
    );
}

#[tokio::test]
async fn a_get_on_a_missing_key_is_a_null_bulk() {
    let server = Server::start().await;
    let mut client = server.client().await;
    assert_eq!(client.get("nope").await.unwrap(), Reply::Bulk(None));
}

#[tokio::test]
async fn an_unknown_command_is_an_error() {
    let server = Server::start().await;
    let mut client = server.client().await;
    let reply = client.cmd("BADCMD", &[b"x"]).await.unwrap();
    assert_eq!(
        reply,
        Reply::Error("ERR unknown command 'BADCMD', with args beginning with: 'x' ".into())
    );
}

#[tokio::test]
async fn a_value_expires_when_its_ttl_runs_out() {
    let server = Server::start().await;
    let mut client = server.client().await;
    client.set("session", b"secret").await.unwrap();
    assert_eq!(
        client.cmd("EXPIRE", &[b"session", b"1"]).await.unwrap(),
        Reply::Integer(1)
    );
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(client.get("session").await.unwrap(), Reply::Bulk(None));
}

#[tokio::test]
async fn a_list_round_trips_through_lpush_and_lrange() {
    let server = Server::start().await;
    let mut client = server.client().await;
    assert_eq!(
        client.cmd("LPUSH", &[b"queue", b"a", b"b"]).await.unwrap(),
        Reply::Integer(2)
    );
    assert_eq!(
        client.cmd("LRANGE", &[b"queue", b"0", b"-1"]).await.unwrap(),
        Reply::Array(vec![
            Reply::Bulk(Some(b"b".to_vec())),
            Reply::Bulk(Some(b"a".to_vec())),
        ])
    );
}

#[tokio::test]
async fn reading_a_list_as_a_string_is_a_wrongtype_error() {
    let server = Server::start().await;
    let mut client = server.client().await;
    client.cmd("LPUSH", &[b"queue", b"a"]).await.unwrap();
    assert_eq!(
        client.get("queue").await.unwrap(),
        Reply::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
    );
}

#[tokio::test]
async fn publish_reaches_a_subscriber_without_being_asked() {
    let server = Server::start().await;
    let mut subscriber = server.client().await;
    assert_eq!(
        subscriber.cmd("SUBSCRIBE", &[b"news"]).await.unwrap(),
        Reply::Array(vec![
            Reply::Bulk(Some(b"subscribe".to_vec())),
            Reply::Bulk(Some(b"news".to_vec())),
            Reply::Integer(1),
        ])
    );
    let mut publisher = server.client().await;
    assert_eq!(
        publisher.cmd("PUBLISH", &[b"news", b"hi"]).await.unwrap(),
        Reply::Integer(1)
    );
    // The pushed message arrives on the subscriber's socket, unprompted.
    assert_eq!(
        subscriber.reply().await.unwrap(),
        Reply::Array(vec![
            Reply::Bulk(Some(b"message".to_vec())),
            Reply::Bulk(Some(b"news".to_vec())),
            Reply::Bulk(Some(b"hi".to_vec())),
        ])
    );
}

#[tokio::test]
async fn a_transaction_queues_and_execs_into_one_array() {
    let server = Server::start().await;
    let mut client = server.client().await;
    assert_eq!(
        client.cmd("MULTI", &[]).await.unwrap(),
        Reply::Simple("OK".into())
    );
    assert_eq!(client.set("a", b"1").await.unwrap(), Reply::Simple("QUEUED".into()));
    assert_eq!(client.get("a").await.unwrap(), Reply::Simple("QUEUED".into()));
    assert_eq!(
        client.cmd("EXEC", &[]).await.unwrap(),
        Reply::Array(vec![
            Reply::Simple("OK".into()),
            Reply::Bulk(Some(b"1".to_vec())),
        ])
    );
}

#[tokio::test]
async fn pipelined_commands_come_back_in_order() {
    let server = Server::start().await;
    let mut client = server.client().await;
    let mut both = encode("PING", &[]);
    both.extend(encode("ECHO", &[b"hello"]));
    client.stream.write_all(&both).await.unwrap();
    assert_eq!(client.reply().await.unwrap(), Reply::Simple("PONG".into()));
    assert_eq!(
        client.reply().await.unwrap(),
        Reply::Bulk(Some(b"hello".to_vec()))
    );
}

#[tokio::test]
async fn a_protocol_error_ends_the_connection_without_a_reply() {
    let server = Server::start().await;
    let mut client = server.client().await;
    client.stream.write_all(b"?\r\n").await.unwrap();
    assert!(client.reply().await.is_err(), "the server hangs up");
}
