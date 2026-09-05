//! Minimal Source RCON client (CS2 dedicated server, `-usercon`).
//!
//! Implements only what the agent needs: authenticate, execute one command,
//! collect the (possibly multi-packet) response.
//!
//! Termination cannot use the classic SRCDS sentinel trick (an empty
//! `SERVERDATA_RESPONSE_VALUE` echoed back under a distinct id). CS2 tags
//! every response packet with the id of the LAST request it saw and sends
//! no distinct empty terminator, so the command output arrives under the
//! sentinel's id and is indistinguishable from it by id alone — the reason
//! a real host was needed to shake this out. Instead we follow the command
//! with a bogus command carrying a unique token; the server answers
//! `Unknown command '<token>'`, an end marker no real output can contain.

use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SERVERDATA_AUTH: i32 = 3;
const SERVERDATA_EXECCOMMAND: i32 = 2;
const SERVERDATA_RESPONSE_VALUE: i32 = 0;

const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Body limit per packet is 4096; responses larger than 1 MiB are nonsense.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

struct Packet {
    id: i32,
    ptype: i32,
    body: Vec<u8>,
}

async fn write_packet(stream: &mut TcpStream, id: i32, ptype: i32, body: &str) -> Result<()> {
    let body_bytes = body.as_bytes();
    let size = 4 + 4 + body_bytes.len() + 2;
    let mut buf = Vec::with_capacity(4 + size);
    buf.extend_from_slice(
        &i32::try_from(size)
            .context("packet too large")?
            .to_le_bytes(),
    );
    buf.extend_from_slice(&id.to_le_bytes());
    buf.extend_from_slice(&ptype.to_le_bytes());
    buf.extend_from_slice(body_bytes);
    buf.extend_from_slice(&[0, 0]);
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(&buf))
        .await
        .context("rcon write timeout")??;
    Ok(())
}

async fn read_packet(stream: &mut TcpStream) -> Result<Packet> {
    let mut size_buf = [0u8; 4];
    tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut size_buf))
        .await
        .context("rcon read timeout")??;
    let size = i32::from_le_bytes(size_buf);
    if !(10..=4110).contains(&size) {
        bail!("implausible rcon packet size: {size}");
    }
    #[allow(clippy::cast_sign_loss)]
    let mut payload = vec![0u8; size as usize];
    tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut payload))
        .await
        .context("rcon read timeout")??;
    let id = i32::from_le_bytes(payload[0..4].try_into().unwrap());
    let ptype = i32::from_le_bytes(payload[4..8].try_into().unwrap());
    let body = payload[8..payload.len().saturating_sub(2)].to_vec();
    Ok(Packet { id, ptype, body })
}

/// Connect, authenticate, run `command`, return its full text response.
///
/// One connection per command: SRCDS handles this fine at our cadence and
/// it sidesteps stale-connection handling entirely.
pub async fn exec(addr: &str, password: &str, command: &str) -> Result<String> {
    let mut stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(addr))
        .await
        .with_context(|| format!("connecting to rcon at {addr}"))??;
    stream.set_nodelay(true).ok();

    // Authenticate. The server may send an empty RESPONSE_VALUE before the
    // AUTH_RESPONSE; an AUTH_RESPONSE with id -1 means bad password.
    write_packet(&mut stream, 1, SERVERDATA_AUTH, password).await?;
    loop {
        let packet = read_packet(&mut stream).await?;
        if packet.ptype == SERVERDATA_EXECCOMMAND {
            // SERVERDATA_AUTH_RESPONSE shares the value 2 with EXECCOMMAND.
            if packet.id == -1 {
                bail!("rcon authentication failed (bad password)");
            }
            break;
        }
    }

    // Execute, then a unique end marker (see the module docs). We ignore
    // packet ids entirely and read until the marker's echo appears.
    let marker = format!("cgm_end_{:032x}", rand::random::<u128>());
    write_packet(&mut stream, 10, SERVERDATA_EXECCOMMAND, command).await?;
    write_packet(&mut stream, 11, SERVERDATA_EXECCOMMAND, &marker).await?;

    let mut buf: Vec<u8> = Vec::new();
    let needle = marker.as_bytes();
    loop {
        let packet = read_packet(&mut stream).await?;
        if packet.ptype != SERVERDATA_RESPONSE_VALUE {
            continue;
        }
        buf.extend_from_slice(&packet.body);
        if buf.len() > MAX_RESPONSE_BYTES {
            bail!("rcon response exceeded {MAX_RESPONSE_BYTES} bytes");
        }
        if contains_subslice(&buf, needle) {
            break;
        }
    }

    Ok(strip_marker(&String::from_utf8_lossy(&buf), &marker))
}

/// Whether `needle` occurs in `haystack`. Inputs are a few KiB at most.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Drop the end marker (and the `Unknown command '` wrapper the server puts
/// around it) plus any trailing whitespace, leaving the real output.
fn strip_marker(text: &str, marker: &str) -> String {
    match text.find(marker) {
        Some(idx) => {
            const WRAPPER: &str = "Unknown command '";
            let cut = if text[..idx].ends_with(WRAPPER) {
                idx - WRAPPER.len()
            } else {
                idx
            };
            text[..cut].trim_end().to_string()
        }
        None => text.trim_end().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{contains_subslice, strip_marker};

    #[test]
    fn strips_the_marker_and_its_wrapper() {
        let m = "cgm_end_abc";
        let out = format!("map     : de_mirage\n#end\nUnknown command '{m}'!\n");
        assert_eq!(strip_marker(&out, m), "map     : de_mirage\n#end");
    }

    #[test]
    fn strips_a_bare_marker_without_a_wrapper() {
        assert_eq!(
            strip_marker("output here\ncgm_end_x tail", "cgm_end_x"),
            "output here"
        );
    }

    #[test]
    fn without_the_marker_only_trailing_space_is_trimmed() {
        assert_eq!(
            strip_marker("plain output\n\n", "cgm_end_x"),
            "plain output"
        );
    }

    #[test]
    fn subslice_search_matches_across_a_split() {
        assert!(contains_subslice(b"aaMARKERbb", b"MARKER"));
        assert!(!contains_subslice(b"aaaa", b"MARKER"));
        assert!(!contains_subslice(b"x", b""));
    }
}
