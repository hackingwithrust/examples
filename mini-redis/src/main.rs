//! mini-redis, step 3: a connection with a buffer, a decoder that survives
//! a split packet, and `ECHO`.
//!
//! Start state: the tag `mini-ch02` — a server that decodes one frame per
//! read and answers `PING`. End state: the tag `mini-ch03` — the same
//! server, keeping the bytes `serve` used to throw away.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};

/// One RESP2 value: the six things a frame can be.
#[derive(Debug, PartialEq)]
enum Frame {
    /// `+OK\r\n` — a short status line, no length prefix.
    Simple(String),
    /// `-ERR ...\r\n` — an error, the same shape as a simple string.
    Error(String),
    /// `:42\r\n` — signed, because `TTL` answers -1 and -2 (step 5).
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
    fn read_frame(&mut self) -> std::io::Result<Option<Frame>> {
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
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&chunk[..n]);
        }
    }
}

fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:6379")?;
    println!("listening on 127.0.0.1:6379");
    for stream in listener.incoming() {
        let stream = stream?;
        if let Err(err) = serve(stream) {
            eprintln!("connection error: {err}");
        }
    }
    Ok(())
}

/// One connection, from the first frame to the hangup. The buffer does the
/// waiting: a frame that arrives in pieces is reassembled, and a second
/// frame that came in the same read is answered next time around.
fn serve(stream: TcpStream) -> std::io::Result<()> {
    let mut conn = Connection::new(stream);
    while let Some(frame) = conn.read_frame()? {
        let mut reply = Vec::new();
        execute(&frame).encode(&mut reply);
        conn.stream.write_all(&reply)?;
    }
    Ok(())
}

/// The one argument a two-part command takes, as bytes. `None` when the
/// frame is not `COMMAND <bulk>`: wrong arity, or an argument that is not
/// a bulk string.
fn argument(parts: &[Frame]) -> Option<&[u8]> {
    match parts {
        [_, Frame::Bulk(message)] => Some(message.as_slice()),
        _ => None,
    }
}

/// The command table. Names fold to upper case first, because the
/// redis-cli on this machine sends them lowercase (captured in step 2).
fn execute(frame: &Frame) -> Frame {
    let parts = match frame {
        Frame::Array(parts) => parts,
        _ => return Frame::Error("ERR expected an array of bulk strings".to_string()),
    };
    let name: Vec<u8> = match parts.first() {
        Some(Frame::Bulk(name)) => name.to_ascii_uppercase(),
        _ => return Frame::Error("ERR expected a command name".to_string()),
    };
    if name == b"PING" {
        match parts.len() {
            1 => Frame::Simple("PONG".to_string()),
            _ => match argument(parts) {
                Some(message) => Frame::Bulk(message.to_vec()),
                None => wrong_args("ping"),
            },
        }
    } else if name == b"ECHO" {
        // `ECHO hello` answers `$5\r\nhello\r\n` off a real
        // redis-server 7.2.4 — the argument back, as a bulk string.
        match argument(parts) {
            Some(message) => Frame::Bulk(message.to_vec()),
            None => wrong_args("echo"),
        }
    } else {
        Frame::Error(format!(
            "ERR unknown command '{}'",
            String::from_utf8_lossy(&name).to_ascii_lowercase()
        ))
    }
}

fn wrong_args(command: &str) -> Frame {
    Frame::Error(format!(
        "ERR wrong number of arguments for '{command}' command"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Shutdown;

    fn bytes(frame: &Frame) -> Vec<u8> {
        let mut out = Vec::new();
        frame.encode(&mut out);
        out
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
        assert_eq!(execute(&frame), Frame::Simple("PONG".to_string()));
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
        assert_eq!(execute(&frame), Frame::Simple("PONG".to_string()));
    }

    #[test]
    fn ping_with_an_argument_echoes_it() {
        let wire = b"*2\r\n$4\r\nping\r\n$5\r\nhello\r\n";
        let (frame, _) = decode(wire).expect("decodes");
        assert_eq!(execute(&frame), Frame::Bulk(b"hello".to_vec()));
    }

    #[test]
    fn ping_over_a_real_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds an ephemeral port");
        let addr = listener.local_addr().expect("has an address");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accepts one connection");
            serve(stream).expect("serves until the client hangs up");
        });
        let mut client = TcpStream::connect(addr).expect("connects");
        client
            .write_all(b"*1\r\n$4\r\nping\r\n")
            .expect("writes a frame");
        client.shutdown(Shutdown::Write).expect("half-closes");
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).expect("reads the reply");
        server.join().expect("the server thread did not panic");
        assert_eq!(reply, b"+PONG\r\n");
    }

    #[test]
    fn echo_round_trips_through_the_codec() {
        let wire = b"*2\r\n$4\r\necho\r\n$5\r\nhello\r\n";
        let (frame, used) = decode(wire).expect("decodes");
        assert_eq!(used, wire.len());
        assert_eq!(execute(&frame), Frame::Bulk(b"hello".to_vec()));
        assert_eq!(bytes(&execute(&frame)), b"$5\r\nhello\r\n");
    }

    #[test]
    fn echo_with_the_wrong_number_of_arguments_is_refused() {
        let one = decode(b"*1\r\n$4\r\necho\r\n").expect("decodes").0;
        let three = decode(b"*3\r\n$4\r\necho\r\n$1\r\na\r\n$1\r\nb\r\n")
            .expect("decodes")
            .0;
        assert_eq!(execute(&one), wrong_args("echo"));
        assert_eq!(execute(&three), wrong_args("echo"));
    }

    #[test]
    fn a_frame_split_across_two_reads_is_reassembled() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds an ephemeral port");
        let addr = listener.local_addr().expect("has an address");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accepts one connection");
            serve(stream).expect("serves until the client hangs up");
        });
        let mut client = TcpStream::connect(addr).expect("connects");
        // The ECHO request, cut inside the payload.
        client
            .write_all(b"*2\r\n$4\r\necho\r\n$5\r\nhel")
            .expect("writes the first half");
        // The pause makes the split real: the server's first read returns
        // before the second half is written.
        std::thread::sleep(std::time::Duration::from_millis(100));
        client
            .write_all(b"lo\r\n")
            .expect("writes the second half");
        client.shutdown(Shutdown::Write).expect("half-closes");
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).expect("reads the reply");
        server.join().expect("the server thread did not panic");
        assert_eq!(reply, b"$5\r\nhello\r\n");
    }

    #[test]
    fn pipelined_commands_are_all_answered() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds an ephemeral port");
        let addr = listener.local_addr().expect("has an address");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accepts one connection");
            serve(stream).expect("serves until the client hangs up");
        });
        let mut client = TcpStream::connect(addr).expect("connects");
        // Two frames in one write — the second one is the one step 2
        // dropped on the floor.
        client
            .write_all(b"*1\r\n$4\r\nping\r\n*2\r\n$4\r\necho\r\n$5\r\nhello\r\n")
            .expect("writes two frames");
        client.shutdown(Shutdown::Write).expect("half-closes");
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).expect("reads the replies");
        server.join().expect("the server thread did not panic");
        assert_eq!(reply, b"+PONG\r\n$5\r\nhello\r\n");
    }

    #[test]
    fn a_buffered_second_frame_needs_no_read() {
        // The buffer starts with two whole frames in it, so the first
        // read_frame decodes without touching the socket at all.
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds an ephemeral port");
        let stream = TcpStream::connect(listener.local_addr().expect("has an address"))
            .expect("connects");
        let mut conn = Connection {
            stream,
            buffer: b"*1\r\n$4\r\nping\r\n*1\r\n$4\r\nping\r\n".to_vec(),
        };
        let first = conn.read_frame().expect("reads").expect("a frame");
        assert_eq!(first, Frame::Array(vec![Frame::Bulk(b"ping".to_vec())]));
        assert_eq!(conn.buffer.len(), 14, "the second frame is still buffered");
        let second = conn.read_frame().expect("reads").expect("a frame");
        assert_eq!(second, first);
        assert!(conn.buffer.is_empty());
    }

    #[test]
    fn a_protocol_error_ends_the_connection_without_a_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds an ephemeral port");
        let addr = listener.local_addr().expect("has an address");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accepts one connection");
            serve(stream)
        });
        let mut client = TcpStream::connect(addr).expect("connects");
        client.write_all(b"?\r\n").expect("writes a bad type byte");
        client.shutdown(Shutdown::Write).expect("half-closes");
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).expect("reads the reply");
        let served = server.join().expect("the server thread did not panic");
        assert!(served.is_err(), "a protocol error fails the connection");
        assert!(reply.is_empty(), "and no reply is written");
    }
}
