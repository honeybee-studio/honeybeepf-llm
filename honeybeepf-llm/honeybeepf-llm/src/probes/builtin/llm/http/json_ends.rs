//! JSON responses past the response buffer limit: followed to their framed end, only both ends kept.

use honeybeepf_llm_core::types::UsageInfo;
use serde_json::{Map, Value};

use super::body::parse_response_json_value;
use super::h1::{self, Framing};

/// Body bytes kept from the start, where chat, messages and Responses bodies put `model`.
const HEAD_KEEP: usize = 64 * 1024;
/// Body bytes kept from the end, where every built-in provider puts usage.
const TAIL_KEEP: usize = 64 * 1024;
/// Longest chunk-size or trailer line followed; real ones are a few bytes.
const MAX_LINE: usize = 4096;

/// A JSON response too large to buffer, with its first and last body bytes and where its body ends.
pub struct JsonEnds {
    walk: BodyWalk,
    head: Vec<u8>,
    tail: Vec<u8>,
    /// Read once when the body ends.
    pub usage: Option<UsageInfo>,
}

impl JsonEnds {
    /// Takes over a buffered HTTP/1.1 response whose body is a JSON object with a framed end.
    pub fn start(buffer: &[u8]) -> Option<Self> {
        let (framing, body) = h1::plain_body(buffer)?;
        let state = match framing {
            Framing::Length(len) => Walk::Body(len),
            Framing::Chunked => Walk::Size,
            Framing::UntilClose => return None,
        };
        let mut ends = Self {
            walk: BodyWalk {
                state,
                line: Vec::new(),
            },
            head: Vec::with_capacity(HEAD_KEEP),
            tail: Vec::with_capacity(2 * TAIL_KEEP),
            usage: None,
        };
        // A JSON array (Gemini streamed without alt=sse) has no single usage at its end.
        let object = ends.feed(body) && ends.head.trim_ascii_start().first() == Some(&b'{');
        object.then_some(ends)
    }

    /// Follows more body bytes; false once the framing broke or the body ended without usage at its ends.
    pub fn feed(&mut self, data: &[u8]) -> bool {
        let Self {
            walk, head, tail, ..
        } = self;
        let followed = walk.feed(data, &mut |data| {
            let room = HEAD_KEEP.saturating_sub(head.len());
            head.extend_from_slice(&data[..room.min(data.len())]);
            // Old bytes go whenever new ones are cut, so the tail stays one piece of the body.
            let cut = data.len().saturating_sub(TAIL_KEEP);
            if cut > 0 {
                tail.clear();
            } else if tail.len() + data.len() > 2 * TAIL_KEEP {
                tail.drain(..tail.len() - TAIL_KEEP);
            }
            tail.extend_from_slice(&data[cut..]);
        });
        if self.walk.state == Walk::Ended && self.usage.is_none() {
            self.usage = self.read_usage();
            return self.usage.is_some();
        }
        followed
    }

    /// Usage from the root members whole in the kept ends; the end wins a key both have.
    fn read_usage(&self) -> Option<UsageInfo> {
        let tail = &self.tail[self.tail.len().saturating_sub(TAIL_KEEP)..];
        let mut root = trailing_members(tail).unwrap_or_default();
        for (key, value) in leading_members(&self.head).unwrap_or_default() {
            root.entry(key).or_insert(value);
        }
        parse_response_json_value(&Value::Object(root))
    }
}

/// Follows the body framing as the bytes stream past, so the end is known without keeping the body.
#[derive(Default)]
struct BodyWalk {
    state: Walk,
    line: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Walk {
    /// Content-Length bytes still due.
    Body(usize),
    #[default]
    Size,
    /// Chunk data bytes still due; at 0, the CRLF closing them.
    Data(usize),
    Trailer,
    Ended,
    Broken,
}

impl BodyWalk {
    fn feed(&mut self, mut data: &[u8], keep: &mut impl FnMut(&[u8])) -> bool {
        while !data.is_empty() {
            match self.state {
                Walk::Body(left) | Walk::Data(left) if left > 0 => {
                    let n = left.min(data.len());
                    keep(&data[..n]);
                    data = &data[n..];
                    self.state = match self.state {
                        Walk::Body(_) if n == left => Walk::Ended,
                        Walk::Body(_) => Walk::Body(left - n),
                        _ => Walk::Data(left - n),
                    };
                }
                Walk::Ended | Walk::Broken => break,
                _ => {
                    let line_end = data.iter().position(|&b| b == b'\n');
                    self.line
                        .extend_from_slice(&data[..line_end.unwrap_or(data.len())]);
                    data = &data[line_end.map_or(data.len(), |i| i + 1)..];
                    if self.line.len() > MAX_LINE {
                        self.state = Walk::Broken;
                    } else if line_end.is_some() {
                        self.state = self.after_line();
                        self.line.clear();
                    }
                }
            }
        }
        self.state != Walk::Broken
    }

    /// State after a whole line in a line state.
    fn after_line(&self) -> Walk {
        let size =
            |s: &str| usize::from_str_radix(s.split(';').next().unwrap_or_default().trim(), 16);
        let line = self.line.strip_suffix(b"\r").map(std::str::from_utf8);
        match (self.state, line) {
            (Walk::Data(_), Some(Ok(""))) => Walk::Size,
            (Walk::Trailer, Some(Ok(""))) => Walk::Ended,
            (Walk::Trailer, Some(_)) => Walk::Trailer,
            (Walk::Size, Some(Ok(line))) => match size(line) {
                Ok(0) => Walk::Trailer,
                Ok(n) => Walk::Data(n),
                Err(_) => Walk::Broken,
            },
            _ => Walk::Broken,
        }
    }
}

/// The root object's first members that are whole in `head`, as an object.
fn leading_members(head: &[u8]) -> Option<Map<String, Value>> {
    let start = head.iter().position(|b| !b.is_ascii_whitespace())?;
    let (mut depth, mut in_str, mut escaped, mut cut) = (0usize, false, false, None);
    for (i, &b) in head.iter().enumerate().skip(start + 1) {
        match b {
            _ if escaped => escaped = false,
            b'\\' if in_str => escaped = true,
            b'"' => in_str = !in_str,
            _ if in_str => {}
            b'{' | b'[' => depth += 1,
            b'}' | b']' if depth > 0 => depth -= 1,
            b',' if depth == 0 => cut = Some(i),
            b'}' | b']' => {
                cut = Some(i);
                break;
            }
            _ => {}
        }
    }
    let mut members = head[start..cut?].to_vec();
    members.push(b'}');
    object_lossy(&members)
}

/// Like the whole-body parse, invalid UTF-8 in a string does not lose the object.
fn object_lossy(json: &[u8]) -> Option<Map<String, Value>> {
    serde_json::from_str(&String::from_utf8_lossy(json)).ok()
}

/// The root object's last members that are whole in `tail`, as an object.
fn trailing_members(tail: &[u8]) -> Option<Map<String, Value>> {
    let end = tail.iter().rposition(|b| !b.is_ascii_whitespace())?;
    let (mut depth, mut in_str, mut cut) = (0usize, false, None);
    for i in (0..end).rev() {
        match tail[i] {
            b'"' => {
                // Read back from the known end, a quote is escaped iff an odd run of backslashes precedes it.
                let slashes = tail[..i].iter().rev().take_while(|&&c| c == b'\\').count();
                if slashes == i {
                    break; // the run may go on before the kept bytes
                }
                in_str ^= slashes % 2 == 0;
            }
            _ if in_str => {}
            b'}' | b']' => depth += 1,
            b'{' | b'[' if depth > 0 => depth -= 1,
            b',' if depth == 0 => cut = Some(i),
            b'{' | b'[' => {
                cut = Some(i);
                break;
            }
            _ => {}
        }
    }
    let mut members = vec![b'{'];
    members.extend_from_slice(&tail[cut? + 1..=end]);
    object_lossy(&members)
}

#[cfg(test)]
mod tests {
    use super::super::body::parse_response_json;
    use super::super::chunk_cases::{Walked, chunk_cases};
    use super::*;

    #[test]
    fn chunk_walk_keeps_its_answers_at_every_split() {
        for case in chunk_cases() {
            let text = String::from_utf8_lossy(case.body);
            for split in 0..=case.body.len() {
                let (mut walk, mut kept) = (BodyWalk::default(), Vec::new());
                walk.feed(&case.body[..split], &mut |d| kept.extend_from_slice(d));
                walk.feed(&case.body[split..], &mut |d| kept.extend_from_slice(d));
                let walked = match walk.state {
                    Walk::Ended => Walked::Ended,
                    Walk::Broken => Walked::Broken,
                    _ => Walked::Pending,
                };
                assert_eq!((&kept[..], walked), case.walked, "split {split} {text:?}");
            }
        }
    }

    /// Escaped quotes and backslashes, and text that looks like JSON members or usage.
    const TRICKY: &str =
        r#"\"a\"b\\\",c} {\"usage\":{\"prompt_tokens\":99,\"completion_tokens\":99}} \\ ,"#;

    /// Built-in provider bodies in the order the APIs send them; `PAD` goes into the generated text.
    const BODIES: [&str; 5] = [
        r#"{"id":"c1","object":"chat.completion","created":1,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"PAD"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":4,"total_tokens":15},"service_tier":"default","system_fingerprint":"fp"}"#,
        r#"{"id":"r1","object":"response","created_at":1,"status":"completed","error":null,"model":"gpt-6","output":[{"type":"message","content":[{"type":"output_text","text":"PAD"}]}],"usage":{"input_tokens":30,"input_tokens_details":{"cached_tokens":0},"output_tokens":9,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":39},"user":null,"metadata":{}}"#,
        r#"{"id":"m1","type":"message","role":"assistant","model":"claude-test","content":[{"type":"text","text":"PAD"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":14,"cache_read_input_tokens":100,"output_tokens":36}}"#,
        r#"{"candidates":[{"content":{"parts":[{"text":"PAD"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"thoughtsTokenCount":2},"modelVersion":"gemini-test","responseId":"x"}"#,
        r#"{"output":{"message":{"role":"assistant","content":[{"text":"PAD"}]}},"stopReason":"end_turn","usage":{"inputTokens":30,"outputTokens":628,"totalTokens":658},"metrics":{"latencyMs":1}}"#,
    ];

    /// Usage and kept capacity after following `body` padded to `pad` bytes, in 16 KB reads.
    fn follow(body: &str, pad: usize) -> (Option<UsageInfo>, usize) {
        let body = body.replace("PAD", &TRICKY.repeat(pad / TRICKY.len()));
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let (first, rest) = resp.as_bytes().split_at(resp.len() / 10);
        let mut ends = JsonEnds::start(first).expect("JSON object");
        let reads = rest.chunks(16384).collect::<Vec<_>>();
        let (last, most) = reads.split_last().expect("reads");
        assert!(most.iter().all(|read| ends.feed(read)));
        assert_eq!(ends.feed(last), ends.usage.is_some());
        assert_eq!(ends.usage, parse_response_json(&body));
        (ends.usage, ends.head.capacity() + ends.tail.capacity())
    }

    #[test]
    fn usage_from_both_ends_matches_the_whole_body() {
        for body in BODIES {
            assert!(
                follow(body, 3 * (HEAD_KEEP + TAIL_KEEP)).0.is_some(),
                "{body}"
            );
        }
    }

    #[test]
    fn kept_bytes_do_not_grow_with_the_body() {
        let kept = [20 << 20, 40 << 20].map(|size| follow(BODIES[0], size).1);
        assert_eq!(kept, [HEAD_KEEP + 2 * TAIL_KEEP; 2]);
    }

    #[test]
    fn tail_stays_one_piece_after_a_short_read() {
        let mut body = BODIES[0]
            .replace("PAD", &"é".repeat(TAIL_KEEP))
            .into_bytes();
        body.insert(body.len() - 3, 0xff);
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
        for short in 1..4 {
            let first = [head.as_bytes(), &body[..short]].concat();
            let mut ends = JsonEnds::start(&first).expect("JSON object");
            assert!(ends.feed(&body[short..]), "{short}");
            assert!(body.ends_with(&ends.tail), "{short}");
        }
    }

    #[test]
    fn members_at_every_cut_are_members_of_the_body() {
        let body = format!(
            r#"{{"model":"m","x":"{TRICKY}","usage":{{"prompt_tokens":1}},"y":"{TRICKY}"}}"#
        );
        let whole: Map<String, Value> = serde_json::from_str(&body).expect("JSON");
        let usage_at = body.find(r#","usage""#).expect("usage");
        for cut in 0..=body.len() {
            let head = leading_members(&body.as_bytes()[..cut]).unwrap_or_default();
            let tail = trailing_members(&body.as_bytes()[cut..]).unwrap_or_default();
            assert!(
                head.iter()
                    .chain(&tail)
                    .all(|(k, v)| whole.get(k) == Some(v)),
                "{cut}"
            );
            assert_eq!(head.contains_key("x"), cut > usage_at, "{cut}");
            assert_eq!(tail.contains_key("usage"), cut <= usage_at, "{cut}");
        }
    }

    #[test]
    fn chunk_walk_ends_only_at_the_last_chunk() {
        let stream = b"5\r\n0\r\n\r\n\r\n3;e=1\r\n0\r\n\r\n0\r\nT: 1\r\n\r\n";
        for split in 0..stream.len() {
            let (mut walk, mut kept) = (BodyWalk::default(), Vec::new());
            assert!(walk.feed(&stream[..split], &mut |d| kept.extend_from_slice(d)));
            assert!(walk.state != Walk::Ended, "{split}");
            assert!(walk.feed(&stream[split..], &mut |d| kept.extend_from_slice(d)));
            assert!(walk.state == Walk::Ended, "{split}");
            assert_eq!(kept, b"0\r\n\r\n0\r\n");
        }
        let broken: [&[u8]; 4] = [b"zz\r\n", b"2\r\nabcd\r\n", b"2\nab\r\n", &[b'1'; 5000]];
        for bytes in broken {
            assert!(!BodyWalk::default().feed(bytes, &mut |_| {}), "{bytes:?}");
        }
    }
}
