//! Newline-delimited JSON framing over any `Read`/`Write` — used by both
//! the daemon (server side) and the `libra-governor` CLI (client side)
//! so the framing logic exists exactly once.
//!
//! One message per line: [`serde_json::to_writer`] never emits an
//! embedded newline for these message shapes, so a single `\n` is a
//! sufficient and simple frame delimiter for the one-request/one-response
//! exchange this protocol uses (see crate docs on transport shape).

use std::io::{self, BufRead, Write};

use serde::{de::DeserializeOwned, Serialize};

/// Errors from reading or writing a framed message.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("malformed json message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("connection closed before a message was received")]
    ConnectionClosed,
}

/// Serializes `value` as one JSON line and writes it (plus a trailing
/// newline) to `writer`, flushing so the peer observes it promptly.
pub fn write_message<W: Write, T: Serialize>(mut writer: W, value: &T) -> Result<(), WireError> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}

/// Reads one newline-delimited JSON message from `reader` and
/// deserializes it as `T`. Returns [`WireError::ConnectionClosed`] if the
/// peer closed the connection without sending a complete line.
pub fn read_message<R: BufRead, T: DeserializeOwned>(mut reader: R) -> Result<T, WireError> {
    let mut line = String::new();
    let bytes_read = reader.read_line(&mut line)?;
    if bytes_read == 0 {
        return Err(WireError::ConnectionClosed);
    }
    let value = serde_json::from_str(line.trim_end())?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn write_then_read_round_trips() {
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, &("hello", 42)).unwrap();
        assert_eq!(buf.last(), Some(&b'\n'));

        let (s, n): (String, i32) = read_message(BufReader::new(buf.as_slice())).unwrap();
        assert_eq!(s, "hello");
        assert_eq!(n, 42);
    }

    #[test]
    fn reading_from_empty_stream_reports_connection_closed() {
        let buf: &[u8] = &[];
        let result: Result<serde_json::Value, _> = read_message(BufReader::new(buf));
        assert!(matches!(result, Err(WireError::ConnectionClosed)));
    }

    #[test]
    fn reading_malformed_json_reports_json_error() {
        let buf = b"not json at all\n";
        let result: Result<serde_json::Value, _> = read_message(BufReader::new(buf.as_slice()));
        assert!(matches!(result, Err(WireError::Json(_))));
    }
}
