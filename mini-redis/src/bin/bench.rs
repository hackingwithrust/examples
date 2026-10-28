//! A load generator for the server: sequential and pipelined SET/GET
//! against a running instance. `tests/client.rs` cannot be imported
//! by a binary, so this file repeats the small amount of protocol
//! work it needs and nothing else.

use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// One encoded command, ready to write.
fn encode(cmd: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", cmd.len()).into_bytes();
    for arg in cmd {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Read exactly one reply, whatever its length. The first byte says
/// which kind it is; only bulk strings need a second read.
async fn read_reply(r: &mut BufReader<TcpStream>) -> std::io::Result<()> {
    let mut line = Vec::new();
    r.read_until(b'\n', &mut line).await?;
    match line[0] {
        b'+' | b'-' | b':' | b',' | b'_' | b'%' | b'>' | b'~' => Ok(()),
        b'$' => {
            let n: i64 = std::str::from_utf8(&line[1..line.len() - 2])
                .unwrap_or("")
                .trim()
                .parse()
                .unwrap_or(0);
            if n > 0 {
                let mut rest = vec![0u8; n as usize + 2]; // payload + CRLF
                r.read_exact(&mut rest).await?;
            }
            Ok(())
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a reply",
        )),
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Set,
    Get,
}

/// One connection's worth of work: `ops` commands, `pipe` at a time.
async fn worker(addr: String, mode: Mode, id: usize, ops: usize, pipe: usize) -> std::io::Result<()> {
    let mut r = BufReader::new(TcpStream::connect(&addr).await?);
    let key = format!("k{id}");
    let value = vec![b'x'; 64];
    let req = match mode {
        Mode::Set => encode(&[b"SET", key.as_bytes(), &value]),
        Mode::Get => encode(&[b"GET", key.as_bytes()]),
    };
    if pipe <= 1 {
        for _ in 0..ops {
            r.get_mut().write_all(&req).await?;
            read_reply(&mut r).await?;
        }
    } else {
        let mut done = 0;
        while done < ops {
            let batch = pipe.min(ops - done);
            for _ in 0..batch {
                r.get_mut().write_all(&req).await?;
            }
            for _ in 0..batch {
                read_reply(&mut r).await?;
            }
            done += batch;
        }
    }
    Ok(())
}

async fn run_phase(addr: &str, mode: Mode, conns: usize, ops: usize, pipe: usize) -> std::io::Result<Duration> {
    let start = Instant::now();
    let mut handles = Vec::new();
    for id in 0..conns {
        handles.push(tokio::spawn(worker(addr.to_string(), mode, id, ops, pipe)));
    }
    for h in handles {
        h.await.unwrap()?;
    }
    Ok(start.elapsed())
}

fn report(name: &str, ops: usize, t: Duration, pipe: usize) {
    let secs = t.as_secs_f64();
    println!(
        "{name:<5} pipeline={pipe:<3} {ops:>7} ops in {secs:7.3}s  {:>10.0} ops/s",
        ops as f64 / secs
    );
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:6379".to_string());
    let conns: usize = args.next().map(|v| v.parse().expect("connections")).unwrap_or(4);
    let ops: usize = args.next().map(|v| v.parse().expect("ops per connection")).unwrap_or(10_000);
    let pipe: usize = args.next().map(|v| v.parse().expect("pipeline depth")).unwrap_or(16);

    // One PING first: if the server is not up, say so now, with the
    // address in the message, instead of failing halfway through a run.
    let mut probe = BufReader::new(TcpStream::connect(&addr).await.map_err(|e| {
        std::io::Error::new(e.kind(), format!("cannot reach {addr}: {e}"))
    })?);
    probe.get_mut().write_all(b"*1\r\n$4\r\nPING\r\n").await?;
    let mut pong = Vec::new();
    probe.read_until(b'\n', &mut pong).await?;
    println!("ping -> {}", String::from_utf8_lossy(&pong).trim_end());

    println!("{addr}  connections={conns}  ops-per-connection={ops}");
    let total = conns * ops;
    let t = run_phase(&addr, Mode::Set, conns, ops, 1).await?;
    report("SET", total, t, 1);
    let t = run_phase(&addr, Mode::Get, conns, ops, 1).await?;
    report("GET", total, t, 1);
    let t = run_phase(&addr, Mode::Set, conns, ops, pipe).await?;
    report("SET", total, t, pipe);
    let t = run_phase(&addr, Mode::Get, conns, ops, pipe).await?;
    report("GET", total, t, pipe);
    Ok(())
}
