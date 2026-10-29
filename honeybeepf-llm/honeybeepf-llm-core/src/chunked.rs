//! HTTP/1.1 chunked transfer coding, walked a step at a time so a caller can drop data it has seen.

/// How strictly a walk reads size lines, the bytes after chunk data and line lengths.
#[derive(Clone, Copy, Debug)]
pub struct ChunkRules {
    /// Skip `;name=value` after the size; otherwise such a line is not a size.
    pub extensions: bool,
    /// Require the whole size line, extensions too, to be UTF-8; otherwise only the size.
    pub utf8_line: bool,
    /// Read the CRLF after chunk data as a line that must be empty; otherwise skip two bytes unread.
    pub data_crlf: bool,
    /// Largest size taken as a chunk; a larger one breaks the walk.
    pub max_size: usize,
    /// Break on a LF without a CR before it; otherwise lines end only at CRLF.
    pub bare_lf_breaks: bool,
    /// Longest line followed.
    pub max_line: usize,
}

impl ChunkRules {
    /// Plain sizes only, the bytes after chunk data skipped unread, and no limits.
    pub const BASIC: Self = Self {
        extensions: false,
        utf8_line: false,
        data_crlf: false,
        max_size: usize::MAX,
        bare_lf_breaks: false,
        max_line: usize::MAX,
    };
}

impl Default for ChunkRules {
    fn default() -> Self {
        Self::BASIC
    }
}

/// Where the walk stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChunkState {
    #[default]
    Size,
    /// Chunk data bytes still due.
    Data(usize),
    /// Bytes after chunk data still to skip unread.
    Skip(usize),
    /// The line after chunk data.
    DataEnd,
    /// Past the last chunk, in the trailer section.
    Trailer,
    Ended,
    Broken,
}

enum Line {
    /// No line end yet.
    Cut,
    /// A bare LF or a line too long, under the rules.
    Bad,
    /// The index of the LF after the CR.
    EndsAt(usize),
}

/// A walk through one chunked body.
#[derive(Clone, Debug, Default)]
pub struct ChunkWalk {
    rules: ChunkRules,
    state: ChunkState,
}

impl ChunkWalk {
    pub fn new(rules: ChunkRules) -> Self {
        Self {
            rules,
            state: ChunkState::Size,
        }
    }

    pub fn state(&self) -> ChunkState {
        self.state
    }

    /// Walks `input` as far as its whole lines go, passing chunk data to `data`; returns the bytes used.
    pub fn walk(&mut self, input: &[u8], mut data: impl FnMut(&[u8])) -> usize {
        let mut used = 0;
        loop {
            let rest = &input[used..];
            match self.state {
                ChunkState::Ended | ChunkState::Broken => return used,
                ChunkState::Data(_) | ChunkState::Skip(_) if rest.is_empty() => return used,
                ChunkState::Data(left) => {
                    let n = left.min(rest.len());
                    data(&rest[..n]);
                    used += n;
                    self.state = if n < left {
                        ChunkState::Data(left - n)
                    } else if self.rules.data_crlf {
                        ChunkState::DataEnd
                    } else {
                        ChunkState::Skip(2)
                    };
                }
                ChunkState::Skip(left) => {
                    let n = left.min(rest.len());
                    used += n;
                    self.state = if n < left {
                        ChunkState::Skip(left - n)
                    } else {
                        ChunkState::Size
                    };
                }
                ChunkState::Size | ChunkState::DataEnd | ChunkState::Trailer => {
                    match self.line(rest) {
                        Line::Cut => return used,
                        Line::Bad => self.state = ChunkState::Broken,
                        Line::EndsAt(lf) => {
                            used += lf + 1;
                            self.state = self.after_line(&rest[..lf - 1]);
                        }
                    }
                }
            }
        }
    }

    /// How the line at the start of `rest` ends.
    fn line(&self, rest: &[u8]) -> Line {
        let mut from = 0;
        let lf = loop {
            match rest[from..].iter().position(|&b| b == b'\n') {
                None if rest.len() > self.rules.max_line => return Line::Bad,
                None => return Line::Cut,
                Some(i) if from + i > 0 && rest[from + i - 1] == b'\r' => break from + i,
                Some(_) if self.rules.bare_lf_breaks => return Line::Bad,
                Some(i) => from += i + 1,
            }
        };
        if lf > self.rules.max_line {
            Line::Bad
        } else {
            Line::EndsAt(lf)
        }
    }

    /// State after a whole line, without its CRLF, in a line state.
    fn after_line(&self, line: &[u8]) -> ChunkState {
        match self.state {
            ChunkState::DataEnd if line.is_empty() => ChunkState::Size,
            ChunkState::Trailer if line.is_empty() => ChunkState::Ended,
            ChunkState::Trailer => ChunkState::Trailer,
            ChunkState::Size if self.rules.utf8_line && std::str::from_utf8(line).is_err() => {
                ChunkState::Broken
            }
            ChunkState::Size => {
                let size = if self.rules.extensions {
                    line.split(|&b| b == b';').next().unwrap_or_default()
                } else {
                    line
                };
                let size = std::str::from_utf8(size)
                    .ok()
                    .and_then(|s| usize::from_str_radix(s.trim(), 16).ok());
                match size {
                    Some(0) => ChunkState::Trailer,
                    Some(n) if n <= self.rules.max_size => ChunkState::Data(n),
                    _ => ChunkState::Broken,
                }
            }
            _ => ChunkState::Broken,
        }
    }
}
