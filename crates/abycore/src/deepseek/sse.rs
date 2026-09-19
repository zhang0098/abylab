use crate::{Error, Result};

pub(super) struct Frame {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE framing, including split UTF-8 and CRLF boundaries.
pub(super) struct Parser {
    line: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    size: usize,
    limit: usize,
    skip_lf: bool,
    first_line: bool,
}

impl Parser {
    pub fn new(limit: usize) -> Self {
        Self {
            line: vec![],
            event: None,
            data: vec![],
            size: 0,
            limit,
            skip_lf: false,
            first_line: true,
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Frame>> {
        let mut frames = vec![];
        for byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if *byte == b'\n' {
                    continue;
                }
            }
            if *byte == b'\r' || *byte == b'\n' {
                self.skip_lf = *byte == b'\r';
                self.finish_line(&mut frames)?;
            } else {
                self.line.push(*byte);
                if self.line.len().saturating_add(self.size) > self.limit {
                    return Err(Error::protocol("SSE event size limit exceeded"));
                }
            }
        }
        Ok(frames)
    }
    fn finish_line(&mut self, frames: &mut Vec<Frame>) -> Result<()> {
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes)
            .map_err(|_| Error::protocol("SSE contains invalid UTF-8"))?;
        let line = if std::mem::replace(&mut self.first_line, false) {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        if line.is_empty() {
            if !self.data.is_empty() {
                frames.push(Frame {
                    event: self.event.take(),
                    data: self.data.join("\n"),
                });
            }
            self.event = None;
            self.data.clear();
            self.size = 0;
        } else if !line.starts_with(':') {
            self.size = self.size.saturating_add(line.len() + 1);
            if self.size > self.limit {
                return Err(Error::protocol("SSE event size limit exceeded"));
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.event = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_byte_boundary_and_line_ending() {
        let bytes = "\u{feff}: ping\r\nevent: x\rdata: 中\ndata: 文\r\n\r\n".as_bytes();
        for split in 0..=bytes.len() {
            let mut p = Parser::new(100);
            let mut f = p.feed(&bytes[..split]).unwrap();
            f.extend(p.feed(&bytes[split..]).unwrap());
            assert_eq!(f.len(), 1);
            assert_eq!(f[0].event.as_deref(), Some("x"));
            assert_eq!(f[0].data, "中\n文");
        }
    }
    #[test]
    fn bounded_and_utf8_checked() {
        assert!(Parser::new(4).feed(b"data: hi").is_err());
        assert!(Parser::new(100).feed(b"data: \xff\n\n").is_err());
    }
}
