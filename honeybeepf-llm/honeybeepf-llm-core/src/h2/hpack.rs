//! HPACK (RFC 7541) header block decoding, with a dynamic table that can be forgotten after lost bytes.

use std::collections::VecDeque;

use super::huffman;

/// Why a header block could not be decoded.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HpackError {
    /// The block ended inside an integer or a string.
    Truncated,
    /// An integer longer than five continuation bytes or past `u32`.
    IntegerOverflow,
    /// Huffman data with EOS, or padding that is longer than 7 bits or not all ones.
    Huffman,
    /// Index 0, or an index past a table the decoder has seen whole.
    BadIndex,
    /// A table size update over the receiver's limit, or after the block's first field.
    TableSize,
}

/// Largest dynamic table kept in memory; dropping entries past it makes the table count as forgotten.
pub const MAX_TABLE_SIZE: usize = 64 * 1024;
/// SETTINGS_HEADER_TABLE_SIZE until the receiver's SETTINGS say otherwise.
pub const DEFAULT_TABLE_SIZE: usize = 4096;
/// Per-entry overhead counted in a table's size (RFC 7541 §4.1).
const ENTRY_OVERHEAD: usize = 32;

/// The static table (RFC 7541 Appendix A); index 1 is `STATIC_TABLE[0]`.
#[rustfmt::skip]
pub static STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""), (":method", "GET"), (":method", "POST"), (":path", "/"), (":path", "/index.html"),
    (":scheme", "http"), (":scheme", "https"), (":status", "200"), (":status", "204"), (":status", "206"),
    (":status", "304"), (":status", "400"), (":status", "404"), (":status", "500"), ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"), ("accept-language", ""), ("accept-ranges", ""), ("accept", ""),
    ("access-control-allow-origin", ""), ("age", ""), ("allow", ""), ("authorization", ""), ("cache-control", ""),
    ("content-disposition", ""), ("content-encoding", ""), ("content-language", ""), ("content-length", ""),
    ("content-location", ""), ("content-range", ""), ("content-type", ""), ("cookie", ""), ("date", ""),
    ("etag", ""), ("expect", ""), ("expires", ""), ("from", ""), ("host", ""), ("if-match", ""),
    ("if-modified-since", ""), ("if-none-match", ""), ("if-range", ""), ("if-unmodified-since", ""),
    ("last-modified", ""), ("link", ""), ("location", ""), ("max-forwards", ""), ("proxy-authenticate", ""),
    ("proxy-authorization", ""), ("range", ""), ("referer", ""), ("refresh", ""), ("retry-after", ""),
    ("server", ""), ("set-cookie", ""), ("strict-transport-security", ""), ("transfer-encoding", ""),
    ("user-agent", ""), ("vary", ""), ("via", ""), ("www-authenticate", ""),
];

/// Decodes an integer with a `prefix`-bit prefix, 1 to 8 (RFC 7541 §5.1); returns it and the bytes it took.
pub fn decode_int(input: &[u8], prefix: u8) -> Result<(u32, usize), HpackError> {
    debug_assert!((1..=8).contains(&prefix), "HPACK prefixes are 1 to 8 bits");
    let first = *input.first().ok_or(HpackError::Truncated)?;
    let max = (1u64 << prefix) - 1;
    let mut value = u64::from(first) & max;
    if value < max {
        return Ok((value as u32, 1));
    }
    // Five continuation bytes add at most 35 bits, so the sum cannot overflow u64.
    for (i, &byte) in input.iter().enumerate().skip(1).take(5) {
        value += u64::from(byte & 0x7f) << (7 * (i - 1));
        if byte & 0x80 == 0 {
            let value = u32::try_from(value).map_err(|_| HpackError::IntegerOverflow)?;
            return Ok((value, i + 1));
        }
    }
    if input.len() > 5 {
        Err(HpackError::IntegerOverflow)
    } else {
        Err(HpackError::Truncated)
    }
}

/// Appends the string literal at the start of `input` (RFC 7541 §5.2) to `out`, which an error leaves as it was; returns the bytes it took.
pub fn decode_string(input: &[u8], out: &mut Vec<u8>) -> Result<usize, HpackError> {
    let huffman = input.first().is_some_and(|b| b & 0x80 != 0);
    let (len, used) = decode_int(input, 7)?;
    let end = used
        .checked_add(len as usize)
        .filter(|&end| end <= input.len())
        .ok_or(HpackError::Truncated)?;
    let bytes = &input[used..end];
    if huffman {
        let start = out.len();
        if let Err(e) = huffman::decode(bytes, out) {
            out.truncate(start);
            return Err(e);
        }
    } else {
        out.extend_from_slice(bytes);
    }
    Ok(end)
}

/// A header name or value, or `Unknown` when it sits in the part of the table the decoder never saw.
// A closed pair, so no `#[non_exhaustive]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Text<'a> {
    Known(&'a [u8]),
    Unknown,
}

/// One decoded header field.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field<'a> {
    pub name: Text<'a>,
    pub value: Text<'a>,
}

#[derive(Clone, Debug)]
struct Entry {
    /// None when the entry took its name from an unknown entry.
    name: Option<Vec<u8>>,
    value: Vec<u8>,
}

impl Entry {
    /// An unknown name counts as empty, so the table is never thought fuller than it is.
    fn size(&self) -> usize {
        self.name.as_ref().map_or(0, Vec::len) + self.value.len() + ENTRY_OVERHEAD
    }
}

/// The decoder of one direction of one connection, holding the dynamic table its encoder keeps.
#[derive(Clone, Debug)]
pub struct Decoder {
    /// Newest first: index 62 is `entries[0]`.
    entries: VecDeque<Entry>,
    size: usize,
    /// The size the encoder last set, at most `limit`.
    max_size: usize,
    /// SETTINGS_HEADER_TABLE_SIZE announced by the receiver of this direction.
    limit: usize,
    /// Older entries exist that were never seen; an index past `entries` names one of them.
    forgotten: bool,
    /// The literal being decoded, reused across fields; header blocks are at most 64 KiB (frame.rs).
    name: Vec<u8>,
    value: Vec<u8>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder that sees its direction from the first header block on.
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            size: 0,
            max_size: DEFAULT_TABLE_SIZE,
            limit: DEFAULT_TABLE_SIZE,
            forgotten: false,
            name: Vec::new(),
            value: Vec::new(),
        }
    }

    /// Takes the SETTINGS_HEADER_TABLE_SIZE the receiver of this direction announced.
    pub fn set_limit(&mut self, size: u32) {
        self.limit = size as usize;
        self.max_size = self.max_size.min(self.limit);
        self.evict();
    }

    /// Drops the table after lost bytes: indexes past the entries added from now on decode as `Unknown`.
    pub fn forget(&mut self) {
        self.entries.clear();
        self.size = 0;
        self.forgotten = true;
        // A lost size update may have grown the table; assuming the largest allowed only keeps entries longer.
        self.max_size = self.limit;
    }

    /// Whether some entries were never seen, so lookups can give `Unknown`.
    pub fn forgotten(&self) -> bool {
        self.forgotten
    }

    /// Size of the entries the decoder holds (RFC 7541 §4.1).
    pub fn table_size(&self) -> usize {
        self.size
    }

    /// Decodes one whole header block, passing each field to `on`; on an error, which leaves the table
    /// forgotten, the fields already passed belong to a block that could not be read and should be dropped.
    pub fn decode(
        &mut self,
        block: &[u8],
        mut on: impl FnMut(Field<'_>),
    ) -> Result<(), HpackError> {
        let decoded = self.decode_fields(block, &mut on);
        if decoded.is_err() {
            self.forget();
        }
        decoded
    }

    fn decode_fields(
        &mut self,
        block: &[u8],
        on: &mut impl FnMut(Field<'_>),
    ) -> Result<(), HpackError> {
        let (mut pos, mut fields) = (0, 0);
        while let Some(&first) = block.get(pos) {
            if first & 0x80 != 0 {
                let (index, used) = decode_int(&block[pos..], 7)?;
                pos += used;
                let (name, value) = lookup(&self.entries, self.forgotten, index)?;
                on(Field { name, value });
            } else if first & 0xe0 == 0x20 {
                let (size, used) = decode_int(&block[pos..], 5)?;
                pos += used;
                let size = size as usize;
                if fields > 0 || size > self.limit {
                    return Err(HpackError::TableSize);
                }
                self.max_size = size;
                if size == 0 {
                    self.empty();
                }
                self.evict();
                continue;
            } else {
                pos += self.literal(&block[pos..], on)?;
            }
            fields += 1;
        }
        Ok(())
    }

    /// Decodes the literal field at the start of `input` (RFC 7541 §6.2); returns the bytes it took.
    fn literal(
        &mut self,
        input: &[u8],
        on: &mut impl FnMut(Field<'_>),
    ) -> Result<usize, HpackError> {
        // 01: with incremental indexing (6-bit index); 0000 and 0001: without (4-bit).
        let indexing = input[0] & 0x40 != 0;
        let (index, mut pos) = decode_int(input, if indexing { 6 } else { 4 })?;
        self.name.clear();
        self.value.clear();
        let name_known = if index == 0 {
            pos += decode_string(&input[pos..], &mut self.name)?;
            true
        } else {
            match lookup(&self.entries, self.forgotten, index)?.0 {
                Text::Known(name) => {
                    self.name.extend_from_slice(name);
                    true
                }
                Text::Unknown => false,
            }
        };
        pos += decode_string(&input[pos..], &mut self.value)?;
        let name = if name_known {
            Text::Known(&self.name)
        } else {
            Text::Unknown
        };
        on(Field {
            name,
            value: Text::Known(&self.value),
        });
        if indexing {
            // Copied before inserting: the insert may evict the entry the name came from (RFC 7541 §4.4).
            let name = name_known.then(|| self.name.clone());
            self.insert(Entry {
                name,
                value: self.value.clone(),
            });
        }
        Ok(pos)
    }

    /// Empties the table on a change that empties the encoder's too, so every later index is known.
    fn empty(&mut self) {
        (self.entries, self.size, self.forgotten) = (VecDeque::new(), 0, false);
    }

    fn insert(&mut self, entry: Entry) {
        let size = entry.size();
        if size > self.max_size {
            // Larger than the table, so the encoder empties it too (RFC 7541 §4.4). After `forget()`
            // `max_size` is the limit, which the encoder's table can't exceed, so this stays certain.
            self.empty();
            return;
        }
        self.entries.push_front(entry);
        self.size += size;
        self.evict();
    }

    fn evict(&mut self) {
        while self.size > self.max_size {
            self.drop_oldest();
        }
        while self.size > MAX_TABLE_SIZE {
            self.drop_oldest();
            self.forgotten = true;
        }
    }

    fn drop_oldest(&mut self) {
        if let Some(old) = self.entries.pop_back() {
            self.size -= old.size();
        }
    }
}

/// The field at `index` (RFC 7541 §2.3.3): static table first, then the dynamic table newest first.
fn lookup(
    entries: &VecDeque<Entry>,
    forgotten: bool,
    index: u32,
) -> Result<(Text<'_>, Text<'_>), HpackError> {
    let index = (index as usize)
        .checked_sub(1)
        .ok_or(HpackError::BadIndex)?;
    if let Some((name, value)) = STATIC_TABLE.get(index) {
        return Ok((Text::Known(name.as_bytes()), Text::Known(value.as_bytes())));
    }
    match entries.get(index - STATIC_TABLE.len()) {
        Some(e) => Ok((
            e.name.as_deref().map_or(Text::Unknown, Text::Known),
            Text::Known(&e.value),
        )),
        None if forgotten => Ok((Text::Unknown, Text::Unknown)),
        None => Err(HpackError::BadIndex),
    }
}
