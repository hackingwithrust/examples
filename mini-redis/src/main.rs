//! mini-redis, step 2: the RESP2 codec written by hand, and a server that
//! answers `PING`.
//!
//! Start state: nothing — this step creates the project. End state: the tag
//! `mini-ch02`, a server that binds 127.0.0.1:6379 and speaks one command.

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
    /// the frame may still be on the wire. Step 3 is this variant's whole
    /// reason for existing.
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

fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:6379")?;
    println!("listening on 127.0.0.1:6379");
    for stream in listener.incoming() {
        let mut stream = stream?;
        if let Err(err) = serve(&mut stream) {
            eprintln!("connection error: {err}");
        }
    }
    Ok(())
}

/// One connection: read, decode, answer, repeat. The read is a single
/// `read` call over one chunk, so a frame split across two packets ends the
/// connection, and a second frame in the same chunk is dropped. Both gaps
/// are the same missing buffer, and step 3 is that buffer.
fn serve(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(()); // the client hung up
        }
        // `used` is ignored on purpose: whatever follows the frame waits in
        // a buffer that does not exist yet.
        let (frame, _used) = decode(&chunk[..n]).map_err(|err| match err {
            DecodeError::Incomplete => {
                std::io::Error::new(ErrorKind::UnexpectedEof, "a frame arrived in pieces")
            }
            DecodeError::Invalid(why) => std::io::Error::new(ErrorKind::InvalidData, why),
        })?;
        let mut reply = Vec::new();
        execute(&frame).encode(&mut reply);
        stream.write_all(&reply)?;
    }
}

/// The command table, which is one command deep. Names are
/// case-insensitive: the redis-cli on this machine sends `ping` lowercase,
/// captured off the wire while this step was written.
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
            2 => match &parts[1] {
                Frame::Bulk(message) => Frame::Bulk(message.clone()),
                _ => wrong_args("ping"),
            },
            _ => wrong_args("ping"),
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
            let (mut stream, _) = listener.accept().expect("accepts one connection");
            serve(&mut stream).expect("serves until the client hangs up");
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
}
