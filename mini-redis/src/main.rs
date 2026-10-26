//! mini-redis, step 5: a deadline on every key, `EXPIRE` and `TTL`, and
//! the sweeper that reclaims what has expired.
//!
//! Start state: the tag `mini-ch04` — one task per connection, a shared
//! store, and `SET`, `GET`, `DEL`, `LPUSH` and `LRANGE`. End state: the
//! tag `mini-ch05` — every key can carry a time to live, `EXPIRE` and
//! `TTL` answer for it, the read path expires keys lazily, and a
//! background task sweeps the ones nobody reads again.

use std::collections::hash_map::Entry as Slot;
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One RESP2 value: the six things a frame can be.
#[derive(Debug, PartialEq)]
enum Frame {
    /// `+OK\r\n` — a short status line, no length prefix.
    Simple(String),
    /// `-ERR ...\r\n` — an error, the same shape as a simple string.
    Error(String),
    /// `:42\r\n` — signed, because `TTL` answers -1 and -2.
    Integer(i64),
    /// `$5\r\nhello\r\n` — length-prefixed bytes. Bytes, not text: a bulk
    /// string may hold anything, CRLF included.
    Bulk(Vec<u8>),
    /// `$-1\r\n` — a bulk string that is not there.
    Null,
    /// `*2\r\n` followed by two frames — how every request arrives.
    Array(Vec<Frame>),
}

impl Frame {
    /// Write the frame's wire form onto `dst`.
    fn encode(&self, dst: &mut Vec<u8>) {
        match self {
            Frame::Simple(s) => {
                dst.push(b'+');
                dst.extend_from_slice(s.as_bytes());
                dst.extend_from_slice(b"\r\n");
            }
            Frame::Error(s) => {
                dst.push(b'-');
                dst.extend_from_slice(s.as_bytes());
                dst.extend_from_slice(b"\r\n");
            }
            Frame::Integer(n) => {
                dst.push(b':');
                dst.extend_from_slice(n.to_string().as_bytes());
                dst.extend_from_slice(b"\r\n");
            }
            Frame::Bulk(b) => {
                dst.push(b'$');
                dst.extend_from_slice(b.len().to_string().as_bytes());
                dst.extend_from_slice(b"\r\n");
                dst.extend_from_slice(b);
                dst.extend_from_slice(b"\r\n");
            }
            Frame::Null => dst.extend_from_slice(b"$-1\r\n"),
            Frame::Array(items) => {
                dst.push(b'*');
                dst.extend_from_slice(items.len().to_string().as_bytes());
                dst.extend_from_slice(b"\r\n");
                for item in items {
                    item.encode(dst);
                }
            }
        }
    }
}

/// Why a buffer did not hold one whole frame.
#[derive(Debug, PartialEq)]
enum DecodeError {
    /// The buffer ends inside the frame. Not a protocol error: the rest of
    /// the frame may still be on the wire, and the connection's buffer now
    /// waits for it instead of refusing it.
    Incomplete,
    /// The bytes are all there, and they are not RESP.
    Invalid(String),
}

/// Decode one complete frame from the front of `buf`, and report how many
/// bytes it used. Anything after that belongs to the next frame.
fn decode(buf: &[u8]) -> Result<(Frame, usize), DecodeError> {
    let mut pos = 0;
    let frame = decode_value(buf, &mut pos)?;
    Ok((frame, pos))
}

fn decode_value(buf: &[u8], pos: &mut usize) -> Result<Frame, DecodeError> {
    let tag = *buf.get(*pos).ok_or(DecodeError::Incomplete)?;
    *pos += 1;
    match tag {
        b'+' => Ok(Frame::Simple(text(read_line(buf, pos)?)?.to_string())),
        b'-' => Ok(Frame::Error(text(read_line(buf, pos)?)?.to_string())),
        b':' => {
            let line = text(read_line(buf, pos)?)?;
            let n: i64 = line
                .parse()
                .map_err(|_| DecodeError::Invalid(format!("bad integer {line:?}")))?;
            Ok(Frame::Integer(n))
        }
        b'$' => match length(read_line(buf, pos)?)? {
            None => Ok(Frame::Null),
            Some(len) => {
                let data = take(buf, pos, len)?;
                expect_crlf(buf, pos)?;
                Ok(Frame::Bulk(data.to_vec()))
            }
        },
        b'*' => match length(read_line(buf, pos)?)? {
            None => Ok(Frame::Null),
            Some(count) => {
                // No with_capacity: the count is the client's claim, the
                // buffer is the truth, and every element costs bytes.
                let mut items = Vec::new();
                for _ in 0..count {
                    items.push(decode_value(buf, pos)?);
                }
                Ok(Frame::Array(items))
            }
        },
        other => Err(DecodeError::Invalid(format!("unknown type byte {other:#04x}"))),
    }
}

/// The bytes before the next CRLF, with `pos` moved past it.
fn read_line<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8], DecodeError> {
    let rest = buf.get(*pos..).ok_or(DecodeError::Incomplete)?;
    match rest.windows(2).position(|pair| pair == b"\r\n") {
        Some(end) => {
            let line = &rest[..end];
            *pos += end + 2;
            Ok(line)
        }
        None => Err(DecodeError::Incomplete),
    }
}

/// `n` bytes, or Incomplete when the buffer is short of them.
fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], DecodeError> {
    let end = pos
        .checked_add(n)
        .ok_or_else(|| DecodeError::Invalid("length too large".to_string()))?;
    let slice = buf.get(*pos..end).ok_or(DecodeError::Incomplete)?;
    *pos = end;
    Ok(slice)
}

/// The CRLF that closes a bulk string's payload.
fn expect_crlf(buf: &[u8], pos: &mut usize) -> Result<(), DecodeError> {
    match buf.get(*pos..*pos + 2) {
        Some([b'\r', b'\n']) => {
            *pos += 2;
            Ok(())
        }
        Some(_) => Err(DecodeError::Invalid(
            "bulk string not followed by CRLF".to_string(),
        )),
        None => Err(DecodeError::Incomplete),
    }
}

/// A length or a count line. Negative means "null"; anything else has to
/// parse and fit in a usize.
fn length(line: &[u8]) -> Result<Option<usize>, DecodeError> {
    let line = text(line)?;
    let n: i64 = line
        .parse()
        .map_err(|_| DecodeError::Invalid(format!("bad length {line:?}")))?;
    if n < 0 {
        Ok(None)
    } else {
        Ok(Some(
            usize::try_from(n).map_err(|_| DecodeError::Invalid("length too large".into()))?,
        ))
    }
}

/// Simple strings and lengths are ASCII in practice; reject the rest rather
/// than guess.
fn text(bytes: &[u8]) -> Result<&str, DecodeError> {
    std::str::from_utf8(bytes).map_err(|_| DecodeError::Invalid("not UTF-8".to_string()))
}

/// One client connection, and the bytes that have arrived for it but do not
/// make a frame yet.
struct Connection {
    stream: TcpStream,
    /// Read off the socket, not yet decoded. A whole frame can sit here
    /// next to the first half of the next one.
    buffer: Vec<u8>,
}

impl Connection {
    fn new(stream: TcpStream) -> Self {
        Connection {
            stream,
            buffer: Vec::new(),
        }
    }

    /// One frame, however many reads it takes.
    ///
    /// `Ok(None)` is a clean hangup: the client closed the connection
    /// between frames, and a half-arrived frame goes with it.
    ///
    /// The step-3 loop with one `.await` added: the buffer does the
    /// waiting exactly as it did when the socket was blocking, because
    /// `Incomplete` and `Invalid` are facts about the bytes, not about how
    /// they arrived.
    async fn read_frame(&mut self) -> std::io::Result<Option<Frame>> {
        loop {
            match decode(&self.buffer) {
                Ok((frame, used)) => {
                    // The frame is out. Whatever follows it in the buffer
                    // belongs to the next call, and stays where it is.
                    self.buffer.drain(..used);
                    return Ok(Some(frame));
                }
                // Not all here yet: fall through and read more.
                Err(DecodeError::Incomplete) => {}
                // Every byte is present and they are still not RESP. More
                // bytes will not help, so this is an error, not a wait.
                Err(DecodeError::Invalid(why)) => {
                    return Err(std::io::Error::new(ErrorKind::InvalidData, why));
                }
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&chunk[..n]);
        }
    }
}

/// What a key holds: a string, or a list of strings.
enum Value {
    String(Vec<u8>),
    List(VecDeque<Vec<u8>>),
}

/// One key in the store: the value, and the instant it stops being
/// there.
///
/// The deadline is an `Instant`, not a wall-clock time. It is monotonic,
/// so adjusting the system clock cannot move a key's expiry, and it is
/// process-relative, so a deadline has to become something else on its
/// way to disk — that is step 6's problem.
struct Entry {
    value: Value,
    /// `None` is a key with no deadline: it lives until it is
    /// overwritten or deleted, which is what `SET` leaves behind.
    expires_at: Option<Instant>,
}

/// The store, shared by every connection's task.
///
/// A standard-library `Mutex`, not a Tokio one. Every critical section in
/// `execute` is a couple of hash-map operations with no `.await` inside,
/// so a synchronous lock is both the cheaper choice and the one that
/// cannot be held across a suspension point — a guard that outlives its
/// statement here makes the future non-`Send`, and `tokio::spawn` refuses
/// it at compile time. The failure mode of a long critical section is
/// different: it blocks the worker thread, and every task on it.
type Db = Arc<Mutex<HashMap<Vec<u8>, Entry>>>;

fn new_db() -> Db {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Whether `entry`'s deadline has arrived. A deadline of exactly `now`
/// counts as arrived, the way the real server treats a key whose
/// millisecond timestamp has passed.
fn expired(entry: &Entry, now: Instant) -> bool {
    matches!(entry.expires_at, Some(at) if now >= at)
}

/// Drop `key` if its deadline has passed.
///
/// This is lazy expiry, and the real server does it on every read: a key
/// that has expired is gone whether or not the sweeper has reached it.
/// Captured on redis-server 7.2.4 — `GET` on an expired key answers
/// nil, and a `TTL` afterwards answers -2.
fn expire_if_needed(store: &mut HashMap<Vec<u8>, Entry>, key: &[u8], now: Instant) {
    if let Some(entry) = store.get(key) {
        if expired(entry, now) {
            store.remove(key);
        }
    }
}

/// The seconds left on `entry`, the way `TTL` reports them: the
/// remaining time rounded to the nearest second. `None` is a key with
/// no deadline at all.
///
/// The rounding is the real server's — a key with 100 seconds left
/// answers 100, and one with under half a second left answers 0, which
/// is not "expired": -2 is what that is for. Captured on redis-server
/// 7.2.4.
fn ttl_of(entry: &Entry, now: Instant) -> Option<i64> {
    let at = entry.expires_at?;
    let left = at.saturating_duration_since(now).as_millis() as i64;
    Some((left + 500) / 1000)
}

/// One command, parsed out of its frame. Parsing looks at the frame only;
/// `execute` is the sole place that touches the store.
enum Command {
    Ping,
    Echo(Vec<u8>),
    Set(Vec<u8>, Vec<u8>),
    Get(Vec<u8>),
    Del(Vec<Vec<u8>>),
    LPush(Vec<u8>, Vec<Vec<u8>>),
    LRange(Vec<u8>, i64, i64),
    Expire(Vec<u8>, i64),
    Ttl(Vec<u8>),
}

impl Command {
    /// The command a request frame asks for, or the reply that says why it
    /// is not one.
    fn from_frame(frame: &Frame) -> Result<Command, Frame> {
        let parts = match frame {
            Frame::Array(parts) => parts,
            _ => return Err(Frame::Error("ERR expected an array of bulk strings".to_string())),
        };
        // Every element must be a bulk string — the command name and its
        // arguments alike — because that is what the real server checks
        // before it looks the command up: `FOO :1` answers a protocol
        // error, not an unknown-command error.
        let mut bulks: Vec<&[u8]> = Vec::with_capacity(parts.len());
        for part in parts {
            bulks.push(bulk(part)?);
        }
        let (name, args) = match bulks.split_first() {
            Some(split) => split,
            None => return Err(Frame::Error("ERR expected a command name".to_string())),
        };
        // Upper case for matching only. Every error message quotes the
        // bytes the client actually sent, which is what the real server
        // does: `SET` sent uppercase is answered `'SET'`.
        match name.to_ascii_uppercase().as_slice() {
            b"PING" => match args {
                [] => Ok(Command::Ping),
                [message] => Ok(Command::Echo(message.to_vec())),
                _ => Err(wrong_args(name)),
            },
            b"ECHO" => match args {
                [message] => Ok(Command::Echo(message.to_vec())),
                _ => Err(wrong_args(name)),
            },
            b"SET" => match args {
                [key, value] => Ok(Command::Set(key.to_vec(), value.to_vec())),
                _ => Err(wrong_args(name)),
            },
            b"GET" => match args {
                [key] => Ok(Command::Get(key.to_vec())),
                _ => Err(wrong_args(name)),
            },
            b"DEL" => match args {
                [] => Err(wrong_args(name)),
                keys => Ok(Command::Del(keys.iter().map(|key| key.to_vec()).collect())),
            },
            b"LPUSH" => match args {
                // A key and at least one value: `LPUSH key` on its own is
                // the wrong-arity error, not a push of nothing.
                [key, values @ ..] if !values.is_empty() => Ok(Command::LPush(
                    key.to_vec(),
                    values.iter().map(|value| value.to_vec()).collect(),
                )),
                _ => Err(wrong_args(name)),
            },
            b"LRANGE" => match args {
                [key, start, stop] => {
                    let start = parse_int(start).ok_or_else(not_an_integer)?;
                    let stop = parse_int(stop).ok_or_else(not_an_integer)?;
                    Ok(Command::LRange(key.to_vec(), start, stop))
                }
                _ => Err(wrong_args(name)),
            },
            b"EXPIRE" => match args {
                [key, seconds] => {
                    let seconds = parse_int(seconds).ok_or_else(not_an_integer)?;
                    // The range check belongs to the argument, and it
                    // runs before the key is looked at: the real server
                    // answers the range error for a missing key too,
                    // and refuses both ends — `EXPIRE k -9223372036854775808`
                    // is the error, not a delete. Captured on
                    // redis-server 7.2.4.
                    if seconds.unsigned_abs() > MAX_EXPIRE_SECONDS as u64 {
                        return Err(invalid_expire_time());
                    }
                    Ok(Command::Expire(key.to_vec(), seconds))
                }
                // Extra arguments are a divergence, not an oversight:
                // the real server reads them as NX/XX/GT/LT options and
                // answers `Unsupported option <arg>`. The clone has no
                // SET-style flags, so it answers the arity error.
                // Captured on redis-server 7.2.4.
                _ => Err(wrong_args(name)),
            },
            b"TTL" => match args {
                [key] => Ok(Command::Ttl(key.to_vec())),
                _ => Err(wrong_args(name)),
            },
            _ => Err(unknown_command(name, args)),
        }
    }
}

/// One element of a request array, as bytes. The real server refuses
/// anything else before it looks the command up: a null bulk answers
/// `invalid bulk length`, and every other type byte answers
/// `expected '$', got '<byte>'`. Captured on redis-server 7.2.4.
fn bulk(frame: &Frame) -> Result<&[u8], Frame> {
    match frame {
        Frame::Bulk(bytes) => Ok(bytes),
        Frame::Null => Err(Frame::Error(
            "ERR Protocol error: invalid bulk length".to_string(),
        )),
        Frame::Simple(_) => Err(protocol_error(b'+')),
        Frame::Error(_) => Err(protocol_error(b'-')),
        Frame::Integer(_) => Err(protocol_error(b':')),
        Frame::Array(_) => Err(protocol_error(b'*')),
    }
}

fn protocol_error(byte: u8) -> Frame {
    Frame::Error(format!(
        "ERR Protocol error: expected '$', got '{}'",
        byte as char
    ))
}

/// The unknown-command reply. The real server lists the arguments after
/// the name, quoted and space-separated, capped at 128 bytes so that a
/// typo cannot become a large error: thirty short arguments stop after
/// the twenty-third. Captured on redis-server 7.2.4.
fn unknown_command(name: &[u8], args: &[&[u8]]) -> Frame {
    let mut listed = Vec::new();
    for arg in args {
        if listed.len() >= 128 {
            break;
        }
        let room = 128 - listed.len();
        listed.push(b'\'');
        listed.extend_from_slice(&arg[..arg.len().min(room)]);
        listed.push(b'\'');
        listed.push(b' ');
    }
    Frame::Error(format!(
        "ERR unknown command '{}', with args beginning with: {}",
        String::from_utf8_lossy(name),
        String::from_utf8_lossy(&listed)
    ))
}

fn wrong_args(name: &[u8]) -> Frame {
    Frame::Error(format!(
        "ERR wrong number of arguments for '{}' command",
        String::from_utf8_lossy(name)
    ))
}

fn not_an_integer() -> Frame {
    Frame::Error("ERR value is not an integer or out of range".to_string())
}

/// The largest `EXPIRE` argument the clone takes, in either direction:
/// `i64::MAX / 1000` seconds, the point where `seconds × 1000` stops
/// fitting in an i64. The real server's cutoff is the same idea one
/// layer up — a value whose millisecond form would not fit beside the
/// current time — and it moves with the server's clock: measured here
/// at 9223370243862175 seconds, accepted, with one more refused.
/// Captured on redis-server 7.2.4.
const MAX_EXPIRE_SECONDS: i64 = i64::MAX / 1000;

fn invalid_expire_time() -> Frame {
    Frame::Error("ERR invalid expire time in 'expire' command".to_string())
}

fn wrong_type() -> Frame {
    Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".to_string())
}

/// An i64 the way the real server reads one: an optional minus, then
/// either a single `0` or a first digit of 1-9. `+5`, `007` and `-0` are
/// refused, where `str::parse` would take them, and so is anything out of
/// range. Captured on redis-server 7.2.4.
fn parse_int(text: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(text).ok()?;
    if text == "0" {
        return Some(0);
    }
    let digits = text.strip_prefix('-').unwrap_or(text);
    let first = digits.as_bytes().first()?;
    if !first.is_ascii_digit() || *first == b'0' {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<i64>().ok()
}

/// The slice `LRANGE` reads, after the real server's clamping: a negative
/// index counts from the end, both indices clamp to the list, and a start
/// past the stop — or past the list itself — reads nothing. `None` is the
/// empty answer.
fn range(len: usize, start: i64, stop: i64) -> Option<(usize, usize)> {
    let len = len as i64;
    let start = if start < 0 { (len + start).max(0) } else { start };
    let stop = if stop < 0 { len + stop } else { stop };
    if start > stop || start >= len {
        return None;
    }
    Some((start as usize, stop.min(len - 1) as usize))
}

/// Run one command against the store and return the reply.
///
/// Synchronous on purpose: the lock is taken and dropped inside this
/// function and never crosses an `.await`, so no command can be suspended
/// halfway through a critical section.
fn execute(db: &Db, frame: &Frame) -> Frame {
    let command = match Command::from_frame(frame) {
        Ok(command) => command,
        Err(reply) => return reply,
    };
    match command {
        Command::Ping => Frame::Simple("PONG".to_string()),
        Command::Echo(message) => Frame::Bulk(message),
        Command::Set(key, value) => {
            // SET replaces whatever was there, a list included: the real
            // server answers +OK, not WRONGTYPE. It also drops any
            // deadline the key had — without KEEPTTL, an overwrite is a
            // new value with no time to live. Captured on 7.2.4.
            db.lock().unwrap().insert(
                key,
                Entry {
                    value: Value::String(value),
                    expires_at: None,
                },
            );
            Frame::Simple("OK".to_string())
        }
        Command::Get(key) => {
            let mut store = db.lock().unwrap();
            let now = Instant::now();
            // Lazy expiry: the deadline is checked on the read, so a key
            // that has expired answers nil even if the sweeper has not
            // run yet. The real server does the same.
            expire_if_needed(&mut store, &key, now);
            match store.get(&key) {
                Some(entry) => match &entry.value {
                    Value::String(value) => Frame::Bulk(value.clone()),
                    Value::List(_) => wrong_type(),
                },
                None => Frame::Null,
            }
        }
        Command::Del(keys) => {
            // No lazy expiry here, and the difference is a window: a key
            // whose deadline has passed but which the sweeper has not
            // reached yet is still counted, where the real server's
            // active cycle has usually reclaimed it already. Captured on
            // redis-server 7.2.4 — `DEL` on a key one second past its
            // deadline answered :0 there.
            let mut store = db.lock().unwrap();
            let mut removed = 0i64;
            for key in keys {
                if store.remove(&key).is_some() {
                    removed += 1;
                }
            }
            Frame::Integer(removed)
        }
        Command::LPush(key, values) => {
            let mut store = db.lock().unwrap();
            let now = Instant::now();
            // Lazy expiry here too: pushing onto a key whose deadline
            // has passed starts a new list, exactly as it does on a key
            // that was never there — the real server answers :1, not
            // WRONGTYPE. Captured on redis-server 7.2.4.
            expire_if_needed(&mut store, &key, now);
            let length = match store.entry(key) {
                Slot::Vacant(slot) => {
                    let mut list = VecDeque::new();
                    for value in values {
                        list.push_front(value);
                    }
                    let length = list.len();
                    slot.insert(Entry {
                        value: Value::List(list),
                        expires_at: None,
                    });
                    length
                }
                Slot::Occupied(mut slot) => match &mut slot.get_mut().value {
                    Value::List(list) => {
                        // Each value goes to the head, so the last
                        // argument pushed is the first one read back.
                        for value in values {
                            list.push_front(value);
                        }
                        list.len()
                    }
                    Value::String(_) => return wrong_type(),
                },
            };
            Frame::Integer(length as i64)
        }
        Command::LRange(key, start, stop) => {
            let store = db.lock().unwrap();
            match store.get(&key) {
                Some(entry) => match &entry.value {
                    Value::List(list) => match range(list.len(), start, stop) {
                        Some((first, last)) => Frame::Array(
                            list.iter()
                                .skip(first)
                                .take(last - first + 1)
                                .map(|value| Frame::Bulk(value.clone()))
                                .collect(),
                        ),
                        None => Frame::Array(Vec::new()),
                    },
                    Value::String(_) => wrong_type(),
                },
                None => Frame::Array(Vec::new()),
            }
        }
        Command::Expire(key, seconds) => {
            let now = Instant::now();
            let mut store = db.lock().unwrap();
            // Lazy expiry first, so the :1/:0 answer is about the key as
            // it is now, not as it was when the client asked.
            expire_if_needed(&mut store, &key, now);
            if seconds <= 0 {
                // A non-positive TTL is a delete, not a deadline in the
                // past: the real server answers :1 and the key is gone,
                // and :0 when there was nothing to delete. Captured on
                // redis-server 7.2.4.
                return match store.remove(&key) {
                    Some(_) => Frame::Integer(1),
                    None => Frame::Integer(0),
                };
            }
            // The argument's range check ran in `from_frame`, so this
            // addition cannot overflow the clock.
            let deadline = now + Duration::from_secs(seconds as u64);
            match store.get_mut(&key) {
                Some(entry) => {
                    entry.expires_at = Some(deadline);
                    Frame::Integer(1)
                }
                None => Frame::Integer(0),
            }
        }
        Command::Ttl(key) => {
            let now = Instant::now();
            let mut store = db.lock().unwrap();
            expire_if_needed(&mut store, &key, now);
            match store.get(&key) {
                // -2 is a key that is not there, whether it never was or
                // its deadline has passed; -1 is a key with no deadline.
                // Captured on redis-server 7.2.4.
                None => Frame::Integer(-2),
                Some(entry) => match ttl_of(entry, now) {
                    Some(seconds) => Frame::Integer(seconds),
                    None => Frame::Integer(-1),
                },
            }
        }
    }
}

/// One connection, from the first frame to the hangup. The task that owns
/// it ends when this returns, and the `Db` clone the task holds is
/// dropped with it.
async fn serve(stream: TcpStream, db: Db) -> std::io::Result<()> {
    let mut conn = Connection::new(stream);
    while let Some(frame) = conn.read_frame().await? {
        let mut reply = Vec::new();
        execute(&db, &frame).encode(&mut reply);
        conn.stream.write_all(&reply).await?;
    }
    Ok(())
}

/// How often the sweeper runs. Once a second is coarse next to the real
/// server's active-expiry cycle, which runs ten times a second and
/// samples the keyspace rather than reading all of it.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// One pass of the sweeper: every key whose deadline has passed, out.
/// Returns how many went.
///
/// The lock is taken and dropped inside, like every command's: a guard
/// held across the `interval.tick().await` below would make this future
/// non-`Send`, and `tokio::spawn` refuses those.
fn sweep(db: &Db, now: Instant) -> usize {
    let mut store = db.lock().unwrap();
    let before = store.len();
    store.retain(|_, entry| !expired(entry, now));
    before - store.len()
}

/// The active half of expiry. Lazy expiry on the read path only
/// reclaims keys that somebody reads again; this is the task that
/// reclaims the rest, once a second, for as long as the server runs.
///
/// `tokio::time::interval`'s first tick completes immediately, so the
/// first pass runs at startup — against an empty store, which costs
/// nothing.
async fn sweeper(db: Db) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    loop {
        interval.tick().await;
        let removed = sweep(&db, Instant::now());
        if removed > 0 {
            println!("swept {removed} expired key(s)");
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:6379").await?;
    println!("listening on 127.0.0.1:6379");
    let db = new_db();
    // The sweeper is one more owner of the store, like every connection
    // task: what moves into it is a clone of the `Arc`.
    tokio::spawn(sweeper(db.clone()));
    loop {
        let (stream, _) = listener.accept().await?;
        // One clone per task. What moves is the reference count, not the
        // map: the store stays where it is, and this clone is dropped
        // when the connection ends.
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(err) = serve(stream, db).await {
                eprintln!("connection error: {err}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(frame: &Frame) -> Vec<u8> {
        let mut out = Vec::new();
        frame.encode(&mut out);
        out
    }

    /// A server on an ephemeral port: one task per connection, sharing
    /// `db`. Every socket test drives this one.
    async fn spawn_server(db: Db) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds an ephemeral port");
        let addr = listener.local_addr().expect("has an address");
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accepts");
                let db = db.clone();
                tokio::spawn(async move { serve(stream, db).await.expect("serves") });
            }
        });
        addr
    }

    async fn connect(addr: std::net::SocketAddr) -> Connection {
        Connection::new(TcpStream::connect(addr).await.expect("connects"))
    }

    /// One command, one reply, over a real socket.
    async fn ask(conn: &mut Connection, wire: &[u8]) -> Frame {
        conn.stream.write_all(wire).await.expect("writes");
        conn.read_frame().await.expect("reads").expect("a frame")
    }

    #[test]
    fn every_frame_type_round_trips() {
        let frames = [
            Frame::Simple("OK".to_string()),
            Frame::Error("ERR unknown command 'foo'".to_string()),
            Frame::Integer(-2),
            Frame::Bulk(b"hello\r\nworld".to_vec()),
            Frame::Bulk(vec![0x00, 0xff]),
            Frame::Null,
            Frame::Array(vec![Frame::Integer(1), Frame::Bulk(b"two".to_vec())]),
        ];
        for frame in &frames {
            let wire = bytes(frame);
            let (decoded, used) = decode(&wire).expect("a whole frame decodes");
            assert_eq!(&decoded, frame);
            assert_eq!(used, wire.len(), "the frame used the whole buffer");
        }
    }

    #[test]
    fn the_exact_bytes_the_real_redis_cli_sends_for_ping() {
        // Captured off a real socket: redis-cli 7.2.4 sends the command
        // name lowercase.
        let wire = b"*1\r\n$4\r\nping\r\n";
        let (frame, used) = decode(wire).expect("decodes");
        assert_eq!(frame, Frame::Array(vec![Frame::Bulk(b"ping".to_vec())]));
        assert_eq!(used, wire.len());
        assert_eq!(execute(&new_db(), &frame), Frame::Simple("PONG".to_string()));
    }

    #[test]
    fn a_frame_in_two_pieces_is_incomplete_not_invalid() {
        assert_eq!(decode(b""), Err(DecodeError::Incomplete));
        assert_eq!(decode(b"*1\r\n$4\r\npi"), Err(DecodeError::Incomplete));
        // The payload is all there; the closing CRLF is still in flight.
        assert_eq!(decode(b"*1\r\n$4\r\nping"), Err(DecodeError::Incomplete));
    }

    #[test]
    fn bytes_that_are_not_resp_are_invalid() {
        assert!(matches!(decode(b"?\r\n"), Err(DecodeError::Invalid(_))));
        assert!(matches!(decode(b":soon\r\n"), Err(DecodeError::Invalid(_))));
        assert!(matches!(decode(b"*x\r\n"), Err(DecodeError::Invalid(_))));
        assert!(matches!(decode(b"$4\r\nhiXYab"), Err(DecodeError::Invalid(_))));
    }

    #[test]
    fn one_decode_consumes_exactly_one_frame() {
        let wire = b"*1\r\n$4\r\nping\r\n*1\r\n$4\r\nping\r\n";
        let (frame, used) = decode(wire).expect("decodes");
        assert_eq!(used, 14);
        assert_eq!(execute(&new_db(), &frame), Frame::Simple("PONG".to_string()));
    }

    #[test]
    fn ping_with_an_argument_echoes_it() {
        let wire = b"*2\r\n$4\r\nping\r\n$5\r\nhello\r\n";
        let (frame, _) = decode(wire).expect("decodes");
        assert_eq!(execute(&new_db(), &frame), Frame::Bulk(b"hello".to_vec()));
    }

    #[test]
    fn echo_round_trips_through_the_codec() {
        let wire = b"*2\r\n$4\r\necho\r\n$5\r\nhello\r\n";
        let (frame, used) = decode(wire).expect("decodes");
        assert_eq!(used, wire.len());
        assert_eq!(execute(&new_db(), &frame), Frame::Bulk(b"hello".to_vec()));
        assert_eq!(bytes(&execute(&new_db(), &frame)), b"$5\r\nhello\r\n");
    }

    #[test]
    fn echo_with_the_wrong_number_of_arguments_is_refused() {
        let one = decode(b"*1\r\n$4\r\necho\r\n").expect("decodes").0;
        let three = decode(b"*3\r\n$4\r\necho\r\n$1\r\na\r\n$1\r\nb\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&new_db(), &one), wrong_args(b"echo"));
        assert_eq!(execute(&new_db(), &three), wrong_args(b"echo"));
    }

    #[test]
    fn set_then_get_round_trips() {
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$8\r\ngreeting\r\n$5\r\nhello\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &set), Frame::Simple("OK".to_string()));
        let get = decode(b"*2\r\n$3\r\nget\r\n$8\r\ngreeting\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &get), Frame::Bulk(b"hello".to_vec()));
    }

    #[test]
    fn get_on_a_missing_key_answers_null() {
        let get = decode(b"*2\r\n$3\r\nget\r\n$4\r\nnope\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&new_db(), &get), Frame::Null);
    }

    #[test]
    fn get_on_a_list_is_a_wrongtype_error() {
        let db = new_db();
        let push = decode(b"*3\r\n$5\r\nlpush\r\n$5\r\nqueue\r\n$4\r\njob1\r\n")
            .expect("decodes")
            .0;
        execute(&db, &push);
        let get = decode(b"*2\r\n$3\r\nget\r\n$5\r\nqueue\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &get), wrong_type());
    }

    #[test]
    fn set_replaces_a_list_without_an_error() {
        // The real server answers +OK here, not WRONGTYPE.
        let db = new_db();
        let push = decode(b"*3\r\n$5\r\nlpush\r\n$5\r\nqueue\r\n$4\r\njob1\r\n")
            .expect("decodes")
            .0;
        execute(&db, &push);
        let set = decode(b"*3\r\n$3\r\nset\r\n$5\r\nqueue\r\n$1\r\nx\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &set), Frame::Simple("OK".to_string()));
        let get = decode(b"*2\r\n$3\r\nget\r\n$5\r\nqueue\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &get), Frame::Bulk(b"x".to_vec()));
    }

    #[test]
    fn lpush_on_a_string_key_is_a_wrongtype_error() {
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$8\r\ngreeting\r\n$5\r\nhello\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        let push = decode(b"*3\r\n$5\r\nlpush\r\n$8\r\ngreeting\r\n$1\r\nx\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &push), wrong_type());
    }

    #[test]
    fn del_counts_only_the_keys_that_existed() {
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$1\r\na\r\n$1\r\nv\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        let del = decode(b"*3\r\n$3\r\ndel\r\n$1\r\na\r\n$1\r\nb\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &del), Frame::Integer(1));
        assert_eq!(execute(&db, &del), Frame::Integer(0));
    }

    #[test]
    fn lpush_needs_at_least_one_value() {
        // A key on its own is the wrong-arity error, not a push of
        // nothing: the real server refuses it.
        let push = decode(b"*2\r\n$5\r\nlpush\r\n$5\r\nqueue\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&new_db(), &push), wrong_args(b"lpush"));
    }

    #[test]
    fn lpush_prepends_each_value_in_order() {
        let db = new_db();
        let push = decode(b"*5\r\n$5\r\nlpush\r\n$5\r\nqueue\r\n$4\r\njob1\r\n$4\r\njob2\r\n$4\r\njob3\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &push), Frame::Integer(3));
        let range = decode(b"*4\r\n$6\r\nlrange\r\n$5\r\nqueue\r\n$1\r\n0\r\n$2\r\n-1\r\n")
            .expect("decodes")
            .0;
        assert_eq!(
            execute(&db, &range),
            Frame::Array(vec![
                Frame::Bulk(b"job3".to_vec()),
                Frame::Bulk(b"job2".to_vec()),
                Frame::Bulk(b"job1".to_vec()),
            ])
        );
    }

    #[test]
    fn lrange_reads_a_slice_the_way_the_real_server_clamps_it() {
        let db = new_db();
        let push = decode(b"*5\r\n$5\r\nlpush\r\n$5\r\nqueue\r\n$4\r\njob1\r\n$4\r\njob2\r\n$4\r\njob3\r\n")
            .expect("decodes")
            .0;
        execute(&db, &push);
        let query = |start: &str, stop: &str| {
            let wire = format!(
                "*4\r\n$6\r\nlrange\r\n$5\r\nqueue\r\n${}\r\n{start}\r\n${}\r\n{stop}\r\n",
                start.len(),
                stop.len()
            );
            let frame = decode(wire.as_bytes()).expect("decodes").0;
            execute(&db, &frame)
        };
        // -2 -1 is the last two: job2, job1.
        assert_eq!(
            query("-2", "-1"),
            Frame::Array(vec![Frame::Bulk(b"job2".to_vec()), Frame::Bulk(b"job1".to_vec())])
        );
        // A stop past the end clamps to the last element.
        assert_eq!(
            query("1", "100"),
            Frame::Array(vec![Frame::Bulk(b"job2".to_vec()), Frame::Bulk(b"job1".to_vec())])
        );
        // A start past the end reads nothing, and so does start past stop.
        assert_eq!(query("5", "10"), Frame::Array(Vec::new()));
        assert_eq!(query("2", "1"), Frame::Array(Vec::new()));
        // Both indices negative past the start read nothing.
        assert_eq!(query("0", "-100"), Frame::Array(Vec::new()));
        // Both clamp: -100 100 is the whole list.
        assert_eq!(
            query("-100", "100"),
            Frame::Array(vec![
                Frame::Bulk(b"job3".to_vec()),
                Frame::Bulk(b"job2".to_vec()),
                Frame::Bulk(b"job1".to_vec()),
            ])
        );
    }

    #[test]
    fn lrange_on_a_missing_key_answers_an_empty_array() {
        let range = decode(b"*4\r\n$6\r\nlrange\r\n$4\r\nnope\r\n$1\r\n0\r\n$2\r\n-1\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&new_db(), &range), Frame::Array(Vec::new()));
    }

    #[test]
    fn lrange_on_a_string_key_is_a_wrongtype_error() {
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$8\r\ngreeting\r\n$5\r\nhello\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        let range = decode(b"*4\r\n$6\r\nlrange\r\n$8\r\ngreeting\r\n$1\r\n0\r\n$2\r\n-1\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &range), wrong_type());
    }

    #[test]
    fn lrange_with_a_non_integer_index_is_refused() {
        for text in ["007", "+5", "-0", "", " 5", "0x10", "9999999999999999999999"] {
            let wire = format!(
                "*4\r\n$6\r\nlrange\r\n$4\r\nnope\r\n${}\r\n{text}\r\n$1\r\n0\r\n",
                text.len()
            );
            let frame = decode(wire.as_bytes()).expect("decodes").0;
            assert_eq!(
                execute(&new_db(), &frame),
                not_an_integer(),
                "{text:?} is not an integer the real server would take"
            );
        }
        // The extremes are in range, and read nothing from an empty list.
        for text in ["0", "-9223372036854775808", "9223372036854775807"] {
            let wire = format!(
                "*4\r\n$6\r\nlrange\r\n$4\r\nnope\r\n${}\r\n{text}\r\n$1\r\n0\r\n",
                text.len()
            );
            let frame = decode(wire.as_bytes()).expect("decodes").0;
            assert_eq!(execute(&new_db(), &frame), Frame::Array(Vec::new()));
        }
    }

    #[test]
    fn every_argument_must_be_a_bulk_string() {
        // A non-bulk argument is refused with the type byte that arrived,
        // whatever the command — including one the server does not know.
        // Each reply below was captured off the real server first.
        let integer = decode(b"*2\r\n$3\r\nfoo\r\n:1\r\n").expect("decodes").0;
        assert_eq!(
            execute(&new_db(), &integer),
            Frame::Error("ERR Protocol error: expected '$', got ':'".to_string())
        );
        let null = decode(b"*2\r\n$4\r\nping\r\n$-1\r\n").expect("decodes").0;
        assert_eq!(
            execute(&new_db(), &null),
            Frame::Error("ERR Protocol error: invalid bulk length".to_string())
        );
        let array = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n*1\r\n$1\r\na\r\n")
            .expect("decodes")
            .0;
        assert_eq!(
            execute(&new_db(), &array),
            Frame::Error("ERR Protocol error: expected '$', got '*'".to_string())
        );
        let simple = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n+OK\r\n")
            .expect("decodes")
            .0;
        assert_eq!(
            execute(&new_db(), &simple),
            Frame::Error("ERR Protocol error: expected '$', got '+'".to_string())
        );
    }

    #[test]
    fn an_unknown_command_lists_its_arguments() {
        let frame = decode(b"*3\r\n$3\r\nfoo\r\n$3\r\nbar\r\n$3\r\nbaz\r\n")
            .expect("decodes")
            .0;
        assert_eq!(
            execute(&new_db(), &frame),
            Frame::Error(
                "ERR unknown command 'foo', with args beginning with: 'bar' 'baz' ".to_string()
            )
        );
    }

    #[test]
    fn an_unknown_command_caps_its_argument_list_at_128_bytes() {
        // Thirty short arguments, and the real server's reply stops after
        // the twenty-third — the same cap, byte for byte.
        let mut wire = b"*31\r\n$3\r\nfoo\r\n".to_vec();
        for i in 0..30 {
            let arg = format!("a{i}");
            wire.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
        }
        let frame = decode(&wire).expect("decodes").0;
        let expected_tail = "'a0' 'a1' 'a2' 'a3' 'a4' 'a5' 'a6' 'a7' 'a8' 'a9' 'a10' 'a11' 'a12' 'a13' 'a14' 'a15' 'a16' 'a17' 'a18' 'a19' 'a20' 'a21' 'a22' ";
        assert_eq!(
            execute(&new_db(), &frame),
            Frame::Error(format!(
                "ERR unknown command 'foo', with args beginning with: {expected_tail}"
            ))
        );
    }

    #[test]
    fn error_messages_quote_the_command_name_as_it_was_sent() {
        let upper = decode(b"*1\r\n$3\r\nSET\r\n").expect("decodes").0;
        assert_eq!(
            execute(&new_db(), &upper),
            Frame::Error("ERR wrong number of arguments for 'SET' command".to_string())
        );
        let lower = decode(b"*1\r\n$3\r\nset\r\n").expect("decodes").0;
        assert_eq!(
            execute(&new_db(), &lower),
            Frame::Error("ERR wrong number of arguments for 'set' command".to_string())
        );
    }

    #[test]
    fn an_empty_array_is_not_a_command() {
        let frame = decode(b"*0\r\n").expect("decodes").0;
        assert_eq!(
            execute(&new_db(), &frame),
            Frame::Error("ERR expected a command name".to_string())
        );
    }

    #[tokio::test]
    async fn ping_over_a_real_socket() {
        let addr = spawn_server(new_db()).await;
        let mut conn = connect(addr).await;
        assert_eq!(
            ask(&mut conn, b"*1\r\n$4\r\nping\r\n").await,
            Frame::Simple("PONG".to_string())
        );
    }

    #[tokio::test]
    async fn a_frame_split_across_two_reads_is_reassembled() {
        let addr = spawn_server(new_db()).await;
        let mut conn = connect(addr).await;
        conn.stream
            .write_all(b"*2\r\n$4\r\necho\r\n$5\r\nhel")
            .await
            .expect("writes the first half");
        // The pause makes the split real: the server's first read returns
        // before the second half is written.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(ask(&mut conn, b"lo\r\n").await, Frame::Bulk(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn pipelined_commands_are_all_answered() {
        let addr = spawn_server(new_db()).await;
        let mut conn = connect(addr).await;
        conn.stream
            .write_all(b"*1\r\n$4\r\nping\r\n*2\r\n$4\r\necho\r\n$5\r\nhello\r\n")
            .await
            .expect("writes two frames");
        assert_eq!(
            conn.read_frame().await.expect("reads").expect("a frame"),
            Frame::Simple("PONG".to_string())
        );
        assert_eq!(
            conn.read_frame().await.expect("reads").expect("a frame"),
            Frame::Bulk(b"hello".to_vec())
        );
    }

    #[tokio::test]
    async fn a_buffered_second_frame_needs_no_read() {
        // The buffer starts with two whole frames in it, so the first
        // read_frame decodes without touching the socket at all.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds an ephemeral port");
        let stream = TcpStream::connect(listener.local_addr().expect("has an address"))
            .await
            .expect("connects");
        let mut conn = Connection {
            stream,
            buffer: b"*1\r\n$4\r\nping\r\n*1\r\n$4\r\nping\r\n".to_vec(),
        };
        let first = conn.read_frame().await.expect("reads").expect("a frame");
        assert_eq!(first, Frame::Array(vec![Frame::Bulk(b"ping".to_vec())]));
        assert_eq!(conn.buffer.len(), 14, "the second frame is still buffered");
        let second = conn.read_frame().await.expect("reads").expect("a frame");
        assert_eq!(second, first);
        assert!(conn.buffer.is_empty());
    }

    #[tokio::test]
    async fn a_protocol_error_ends_the_connection_without_a_reply() {
        let addr = spawn_server(new_db()).await;
        let mut conn = connect(addr).await;
        conn.stream
            .write_all(b"?\r\n")
            .await
            .expect("writes a bad type byte");
        // The server's read_frame failed, serve returned, and the task
        // dropped the stream: the client sees a clean EOF.
        let mut reply = Vec::new();
        conn.stream
            .read_to_end(&mut reply)
            .await
            .expect("reads to the end");
        assert!(reply.is_empty(), "and no reply is written");
    }

    #[tokio::test]
    async fn two_connections_share_one_store() {
        let addr = spawn_server(new_db()).await;
        let mut one = connect(addr).await;
        assert_eq!(
            ask(
                &mut one,
                b"*3\r\n$3\r\nset\r\n$8\r\ngreeting\r\n$5\r\nhello\r\n"
            )
            .await,
            Frame::Simple("OK".to_string())
        );
        // A second connection, a second task, and the same store: what
        // the first client wrote, the second one reads.
        let mut two = connect(addr).await;
        assert_eq!(
            ask(&mut two, b"*2\r\n$3\r\nget\r\n$8\r\ngreeting\r\n").await,
            Frame::Bulk(b"hello".to_vec())
        );
    }

    /// A deadline that has already arrived, set directly: the next read
    /// must find the key gone without waiting for the sweeper.
    fn mark_expired(db: &Db, key: &str) {
        db.lock()
            .unwrap()
            .get_mut(key.as_bytes())
            .expect("the key is there")
            .expires_at = Some(Instant::now());
    }

    #[test]
    fn a_key_with_no_deadline_answers_minus_one() {
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nv\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        let ttl = decode(b"*2\r\n$3\r\nttl\r\n$1\r\nk\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &ttl), Frame::Integer(-1));
    }

    #[test]
    fn ttl_on_a_missing_key_answers_minus_two() {
        let ttl = decode(b"*2\r\n$3\r\nttl\r\n$1\r\nk\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&new_db(), &ttl), Frame::Integer(-2));
    }

    #[test]
    fn expire_then_ttl_reports_the_seconds_left() {
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nv\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        let expire = decode(b"*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n$3\r\n100\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &expire), Frame::Integer(1));
        let ttl = decode(b"*2\r\n$3\r\nttl\r\n$1\r\nk\r\n")
            .expect("decodes")
            .0;
        // 100 seconds, give or take the milliseconds the test took.
        match execute(&db, &ttl) {
            Frame::Integer(n) => assert!((99..=100).contains(&n), "ttl was {n}"),
            other => panic!("ttl answered {other:?}"),
        }
    }

    #[test]
    fn expire_replaces_an_earlier_deadline() {
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nv\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        let first = decode(b"*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n$2\r\n50\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &first), Frame::Integer(1));
        let second = decode(b"*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n$3\r\n100\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &second), Frame::Integer(1));
        let ttl = decode(b"*2\r\n$3\r\nttl\r\n$1\r\nk\r\n")
            .expect("decodes")
            .0;
        match execute(&db, &ttl) {
            Frame::Integer(n) => assert!((99..=100).contains(&n), "ttl was {n}"),
            other => panic!("ttl answered {other:?}"),
        }
    }

    #[test]
    fn expire_on_a_missing_key_answers_zero() {
        let expire = decode(b"*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n$3\r\n100\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&new_db(), &expire), Frame::Integer(0));
    }

    #[test]
    fn a_non_positive_ttl_deletes_the_key() {
        for seconds in ["0", "-1"] {
            let db = new_db();
            let set = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nv\r\n")
                .expect("decodes")
                .0;
            execute(&db, &set);
            let wire = format!(
                "*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n${}\r\n{seconds}\r\n",
                seconds.len()
            );
            let expire = decode(wire.as_bytes()).expect("decodes").0;
            // A delete, not a deadline in the past: :1, and the key is
            // gone. Captured on redis-server 7.2.4.
            assert_eq!(execute(&db, &expire), Frame::Integer(1));
            let get = decode(b"*2\r\n$3\r\nget\r\n$1\r\nk\r\n")
                .expect("decodes")
                .0;
            assert_eq!(execute(&db, &get), Frame::Null);
            let ttl = decode(b"*2\r\n$3\r\nttl\r\n$1\r\nk\r\n")
                .expect("decodes")
                .0;
            assert_eq!(execute(&db, &ttl), Frame::Integer(-2));
        }
    }

    #[test]
    fn a_non_positive_ttl_on_a_missing_key_answers_zero() {
        let expire = decode(b"*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n$1\r\n0\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&new_db(), &expire), Frame::Integer(0));
    }

    #[test]
    fn set_clears_the_deadline() {
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nv\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        let expire = decode(b"*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n$3\r\n100\r\n")
            .expect("decodes")
            .0;
        execute(&db, &expire);
        // The overwrite is a new value with no time to live, the same
        // as on the real server. Captured on redis-server 7.2.4.
        let again = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nw\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &again), Frame::Simple("OK".to_string()));
        let ttl = decode(b"*2\r\n$3\r\nttl\r\n$1\r\nk\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &ttl), Frame::Integer(-1));
    }

    #[test]
    fn expire_reads_its_argument_the_way_the_real_server_does() {
        for text in ["abc", "+5", "007", "", " 5", "0x10", "9999999999999999999999"] {
            let wire = format!(
                "*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n${}\r\n{text}\r\n",
                text.len()
            );
            let frame = decode(wire.as_bytes()).expect("decodes").0;
            assert_eq!(
                execute(&new_db(), &frame),
                not_an_integer(),
                "{text:?} is not an integer the real server would take"
            );
        }
    }

    #[test]
    fn an_expire_time_past_the_cutoff_is_refused() {
        // Both ends: the real server refuses a value whose millisecond
        // form would not fit beside the current time, and the clone
        // refuses one whose seconds form would not fit in an i64
        // millisecond count. The replies match at both absurd ends.
        // Captured on redis-server 7.2.4.
        for text in ["9223372036854776", "-9223372036854775808"] {
            let wire = format!(
                "*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n${}\r\n{text}\r\n",
                text.len()
            );
            let frame = decode(wire.as_bytes()).expect("decodes").0;
            assert_eq!(
                execute(&new_db(), &frame),
                invalid_expire_time(),
                "{text:?} is past the clone's cutoff"
            );
        }
        // The one value the two cutoffs disagree about. The real
        // server's line moves with its clock and refused
        // 9223372036854775 here; the clone's is fixed at i64::MAX / 1000
        // and takes it — the key is not there, so the answer is :0.
        let wire = b"*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n$16\r\n9223372036854775\r\n";
        let frame = decode(wire).expect("decodes").0;
        assert_eq!(execute(&new_db(), &frame), Frame::Integer(0));
    }

    #[test]
    fn expire_with_the_wrong_number_of_arguments_is_refused() {
        let none = decode(b"*1\r\n$6\r\nexpire\r\n").expect("decodes").0;
        assert_eq!(execute(&new_db(), &none), wrong_args(b"expire"));
        let one = decode(b"*2\r\n$6\r\nexpire\r\n$1\r\nk\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&new_db(), &one), wrong_args(b"expire"));
        // Extra arguments are the one place the clone and the real
        // server part ways: the real server reads them as NX/XX/GT/LT
        // options and answers `Unsupported option 2`.
        let three = decode(b"*4\r\n$6\r\nexpire\r\n$1\r\nk\r\n$1\r\n1\r\n$1\r\n2\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&new_db(), &three), wrong_args(b"expire"));
    }

    #[test]
    fn ttl_with_the_wrong_number_of_arguments_is_refused() {
        let none = decode(b"*1\r\n$3\r\nttl\r\n").expect("decodes").0;
        assert_eq!(execute(&new_db(), &none), wrong_args(b"ttl"));
        let two = decode(b"*3\r\n$3\r\nttl\r\n$1\r\nk\r\n$1\r\n1\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&new_db(), &two), wrong_args(b"ttl"));
    }

    #[test]
    fn expire_works_on_a_list_key() {
        let db = new_db();
        let push = decode(b"*3\r\n$5\r\nlpush\r\n$1\r\nk\r\n$1\r\nv\r\n")
            .expect("decodes")
            .0;
        execute(&db, &push);
        let expire = decode(b"*3\r\n$6\r\nexpire\r\n$1\r\nk\r\n$3\r\n100\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &expire), Frame::Integer(1));
        let ttl = decode(b"*2\r\n$3\r\nttl\r\n$1\r\nk\r\n")
            .expect("decodes")
            .0;
        match execute(&db, &ttl) {
            Frame::Integer(n) => assert!((99..=100).contains(&n), "ttl was {n}"),
            other => panic!("ttl answered {other:?}"),
        }
    }

    #[test]
    fn a_deadline_that_has_arrived_expires_the_key_on_read() {
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nv\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        mark_expired(&db, "k");
        let get = decode(b"*2\r\n$3\r\nget\r\n$1\r\nk\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &get), Frame::Null);
        let ttl = decode(b"*2\r\n$3\r\nttl\r\n$1\r\nk\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &ttl), Frame::Integer(-2));
    }

    #[test]
    fn lpush_on_an_expired_key_starts_a_new_list() {
        // The real server answers :1 here, not WRONGTYPE: the deadline
        // has passed, so there is no string to push onto. Captured on
        // redis-server 7.2.4.
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nv\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        mark_expired(&db, "k");
        let push = decode(b"*3\r\n$5\r\nlpush\r\n$1\r\nk\r\n$1\r\nx\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&db, &push), Frame::Integer(1));
        let range = decode(b"*4\r\n$6\r\nlrange\r\n$1\r\nk\r\n$1\r\n0\r\n$2\r\n-1\r\n")
            .expect("decodes")
            .0;
        assert_eq!(
            execute(&db, &range),
            Frame::Array(vec![Frame::Bulk(b"x".to_vec())])
        );
    }

    #[test]
    fn ttl_rounds_to_the_nearest_second() {
        // Under half a second left answers 0, and 100 seconds answers
        // 100 — the real server's own rounding. Captured on
        // redis-server 7.2.4.
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nv\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        let ttl = decode(b"*2\r\n$3\r\nttl\r\n$1\r\nk\r\n")
            .expect("decodes")
            .0;
        for (millis, expected) in [(400, 0), (600, 1)] {
            db.lock()
                .unwrap()
                .get_mut(b"k".as_slice())
                .expect("the key is there")
                .expires_at = Some(Instant::now() + Duration::from_millis(millis));
            assert_eq!(execute(&db, &ttl), Frame::Integer(expected));
        }
        db.lock()
            .unwrap()
            .get_mut(b"k".as_slice())
            .expect("the key is there")
            .expires_at = Some(Instant::now() + Duration::from_secs(100));
        assert_eq!(execute(&db, &ttl), Frame::Integer(100));
    }

    #[test]
    fn the_sweeper_removes_only_the_keys_whose_deadline_has_passed() {
        let now = Instant::now();
        let db = new_db();
        {
            let mut store = db.lock().unwrap();
            let value = |expires_at| Entry {
                value: Value::String(b"v".to_vec()),
                expires_at,
            };
            store.insert(b"forever".to_vec(), value(None));
            store.insert(b"session".to_vec(), value(Some(now + Duration::from_secs(100))));
            store.insert(b"cache".to_vec(), value(Some(now + Duration::from_secs(1))));
            store.insert(b"flash".to_vec(), value(Some(now + Duration::from_millis(400))));
            store.insert(b"stale".to_vec(), value(Some(now - Duration::from_secs(1))));
        }
        // One pass, taken a second later than the deadlines above:
        // cache and flash have passed, stale passed long ago.
        let removed = sweep(&db, now + Duration::from_secs(2));
        assert_eq!(removed, 3);
        let mut remaining: Vec<String> = db
            .lock()
            .unwrap()
            .keys()
            .map(|key| String::from_utf8_lossy(key).into_owned())
            .collect();
        remaining.sort();
        assert_eq!(
            remaining,
            vec!["forever".to_string(), "session".to_string()]
        );
    }

    #[tokio::test]
    async fn the_sweeper_task_reclaims_an_expired_key_on_its_own() {
        let db = new_db();
        let set = decode(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nv\r\n")
            .expect("decodes")
            .0;
        execute(&db, &set);
        mark_expired(&db, "k");
        // The interval's first tick is immediate, so the pass runs
        // without waiting a second for it.
        tokio::spawn(sweeper(db.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(db.lock().unwrap().is_empty(), "the sweeper reclaimed it");
    }

    #[tokio::test]
    async fn a_half_sent_command_does_not_block_another_connection() {
        let addr = spawn_server(new_db()).await;
        // Half a PING, and then nothing: this connection stays open with
        // a partial frame in the server's buffer. On the step-3 server,
        // every other client would now wait behind it.
        let mut slow = connect(addr).await;
        slow.stream
            .write_all(b"*1\r\n$4\r\npi")
            .await
            .expect("writes half a PING");
        // The other client runs to completion while that frame is open.
        let mut other = connect(addr).await;
        assert_eq!(
            ask(
                &mut other,
                b"*3\r\n$3\r\nset\r\n$8\r\ngreeting\r\n$5\r\nhello\r\n"
            )
            .await,
            Frame::Simple("OK".to_string())
        );
        assert_eq!(
            ask(&mut other, b"*2\r\n$3\r\nget\r\n$8\r\ngreeting\r\n").await,
            Frame::Bulk(b"hello".to_vec())
        );
        // And the slow client's PING completes afterwards.
        assert_eq!(
            ask(&mut slow, b"ng\r\n").await,
            Frame::Simple("PONG".to_string())
        );
    }
}
