//! Bounded interactive stdin line reading for the agent CLI loop.
//!
//! Extracted from `loop_.rs` so the 1 MiB input cap and drain semantics are
//! not interleaved with turn assembly.

pub(crate) const MAX_INTERACTIVE_INPUT_BYTES: usize = 1024 * 1024; // 1 MiB

/// Result of [`read_capped_line`].
#[derive(Debug)]
pub(crate) enum CappedLine {
    /// A full line under the cap, with the trailing `\n` stripped.
    Line(String),
    /// The physical line exceeded `cap`. The remainder has been
    /// drained to the next `\n` or EOF, so the caller must treat this
    /// as a discarded line and must not feed it into the model path.
    Truncated,
    /// EOF with no bytes read.
    Eof,
}

pub(crate) fn read_capped_line<R: std::io::BufRead>(
    reader: R,
    cap: usize,
) -> std::io::Result<CappedLine> {
    let mut raw = Vec::new();
    // +1 headroom so the cap detection is unambiguous: a buffer that
    // reaches exactly `cap` bytes without a `\n` was truncated; a
    // buffer shorter than `cap` has the full line.
    let mut limited = reader.take((cap + 1) as u64);
    std::io::BufRead::read_until(&mut limited, b'\n', &mut raw)?;
    let truncated = raw.len() > cap;
    if truncated {
        // Drain the rest of the physical line without accumulating it
        // in memory; `read_until` into a `Vec` would re-introduce the
        // original OOM vector.
        let mut inner = limited.into_inner();
        discard_until_newline(&mut inner)?;
        return Ok(CappedLine::Truncated);
    } else if raw.last() == Some(&b'\n') {
        // Strip the trailing `\n` that `read_until` leaves behind. The
        // lossy decode runs after the strip so the result has no
        // trailing newline regardless of the cap path.
        raw.pop();
    }
    if raw.is_empty() {
        return Ok(CappedLine::Eof);
    }
    Ok(CappedLine::Line(String::from_utf8_lossy(&raw).into_owned()))
}

fn discard_until_newline<R: std::io::BufRead>(reader: &mut R) -> std::io::Result<()> {
    loop {
        let buf = reader.fill_buf()?;
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            reader.consume(pos + 1);
            return Ok(());
        }
        let len = buf.len();
        if len == 0 {
            return Ok(());
        }
        reader.consume(len);
    }
}
