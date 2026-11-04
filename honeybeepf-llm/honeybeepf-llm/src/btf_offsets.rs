//! Load-time kernel struct field offsets, read from `/sys/kernel/btf/vmlinux`.
//! aya has no program-side CO-RE (aya#349), so plaintext capture resolves the kernel
//! fields it reads here and injects the byte offsets into the eBPF program as globals.

use std::{fmt, fs, io, path::Path};

const BTF_MAGIC: u16 = 0xeb9f;

const KIND_PTR: u32 = 2;
const KIND_ARRAY: u32 = 3;
const KIND_STRUCT: u32 = 4;
const KIND_UNION: u32 = 5;
const KIND_ENUM: u32 = 6;
const KIND_TYPEDEF: u32 = 8;
const KIND_VOLATILE: u32 = 9;
const KIND_CONST: u32 = 10;
const KIND_RESTRICT: u32 = 11;
const KIND_FUNC_PROTO: u32 = 13;
const KIND_VAR: u32 = 14;
const KIND_DATASEC: u32 = 15;
const KIND_DECL_TAG: u32 = 17;
const KIND_ENUM64: u32 = 19;

const INT_KIND: u32 = 1;
const FLOAT_KIND: u32 = 16;

/// Why a field offset could not be resolved; the caller logs it and disables plaintext capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtfError {
    /// `/sys/kernel/btf/vmlinux` is absent or unreadable (kernel built without `CONFIG_DEBUG_INFO_BTF`).
    NoVmlinux(String),
    /// The blob is not little-endian BTF we understand.
    BadBlob(&'static str),
    /// No struct/union with this name exists in the kernel BTF.
    NoStruct(String),
    /// The struct exists but the field path does not resolve (layout changed, field renamed/removed).
    NoField { root: String, path: String },
    /// The field is a bitfield; we only inject byte-aligned offsets.
    NotByteAligned { root: String, path: String },
    /// No enum anywhere in the kernel BTF defines this variant (e.g. `ITER_UBUF` pre-6.7).
    NoEnumValue(String),
}

impl fmt::Display for BtfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoVmlinux(e) => write!(f, "kernel BTF unavailable: {e}"),
            Self::BadBlob(e) => write!(f, "unusable BTF blob: {e}"),
            Self::NoStruct(s) => write!(f, "struct `{s}` not in kernel BTF"),
            Self::NoField { root, path } => write!(f, "field `{root}.{path}` not in kernel BTF"),
            Self::NotByteAligned { root, path } => {
                write!(f, "field `{root}.{path}` is a bitfield")
            }
            Self::NoEnumValue(v) => write!(f, "enum variant `{v}` not in kernel BTF"),
        }
    }
}

impl std::error::Error for BtfError {}

/// A parsed kernel BTF blob, enough to resolve embedded field byte offsets.
pub struct Btf {
    types: Vec<Type>,
    strings: Vec<u8>,
}

struct Type {
    name_off: u32,
    kind: u32,
    /// `size` for INT/STRUCT/UNION/ENUM, else the referenced type id.
    size_or_type: u32,
    members: Vec<Member>,
    /// (name_off, value) for ENUM/ENUM64, empty otherwise.
    enum_vals: Vec<(u32, i64)>,
}

struct Member {
    name_off: u32,
    type_id: u32,
    /// Raw `btf_member.offset`; for a kind_flag struct the low 24 bits are the bit offset.
    offset: u32,
}

impl Btf {
    /// Parse the running kernel's BTF.
    pub fn from_sys_fs() -> Result<Self, BtfError> {
        Self::from_path("/sys/kernel/btf/vmlinux")
    }

    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, BtfError> {
        let raw = fs::read(path).map_err(|e| BtfError::NoVmlinux(describe_io(&e)))?;
        Self::parse(&raw)
    }

    /// Parse a raw little-endian BTF blob (the `.BTF` section / vmlinux BTF).
    pub fn parse(raw: &[u8]) -> Result<Self, BtfError> {
        if raw.len() < 24 {
            return Err(BtfError::BadBlob("shorter than a BTF header"));
        }
        let magic = u16::from_le_bytes([raw[0], raw[1]]);
        if magic != BTF_MAGIC {
            // A big-endian kernel (e.g. s390x) writes the magic byte-swapped; unsupported here.
            return Err(BtfError::BadBlob("not little-endian BTF"));
        }
        let hdr_len = rd32(raw, 4)? as usize;
        let type_off = rd32(raw, 8)? as usize;
        let type_len = rd32(raw, 12)? as usize;
        let str_off = rd32(raw, 16)? as usize;
        let str_len = rd32(raw, 20)? as usize;

        let type_start = hdr_len
            .checked_add(type_off)
            .ok_or(BtfError::BadBlob("type section overflow"))?;
        let type_end = type_start
            .checked_add(type_len)
            .ok_or(BtfError::BadBlob("type section overflow"))?;
        let str_start = hdr_len
            .checked_add(str_off)
            .ok_or(BtfError::BadBlob("string section overflow"))?;
        let str_end = str_start
            .checked_add(str_len)
            .ok_or(BtfError::BadBlob("string section overflow"))?;
        if type_end > raw.len() || str_end > raw.len() {
            return Err(BtfError::BadBlob("section runs past the blob"));
        }

        let strings = raw[str_start..str_end].to_vec();
        let types = parse_types(&raw[type_start..type_end])?;
        Ok(Self { types, strings })
    }

    /// Byte offset of an embedded field reached by `path` from struct/union `root`.
    /// Anonymous struct/union members are descended transparently; pointers are never
    /// dereferenced, so every path component must be embedded, not behind a `*`.
    pub fn offset_of(&self, root: &str, path: &[&str]) -> Result<u32, BtfError> {
        let mut cur = self
            .struct_id_by_name(root)
            .ok_or_else(|| BtfError::NoStruct(root.to_string()))?;
        let mut bits: u32 = 0;
        for comp in path {
            let (add, next) = self
                .find_member(cur, comp)
                .ok_or_else(|| BtfError::NoField {
                    root: root.to_string(),
                    path: path.join("."),
                })?;
            bits = bits.saturating_add(add);
            cur = next;
        }
        if !bits.is_multiple_of(8) {
            return Err(BtfError::NotByteAligned {
                root: root.to_string(),
                path: path.join("."),
            });
        }
        Ok(bits / 8)
    }

    /// True when `root` exists and every `path` resolves.
    pub fn has(&self, root: &str, path: &[&str]) -> bool {
        self.offset_of(root, path).is_ok()
    }

    /// Value of an enum variant (searched across all enums), e.g. `ITER_UBUF`.
    /// `iter_type` enumerators were renumbered at 6.7, so capture reads these, not constants.
    pub fn enum_value(&self, variant: &str) -> Result<i64, BtfError> {
        self.types
            .iter()
            .filter(|t| t.kind == KIND_ENUM || t.kind == KIND_ENUM64)
            .flat_map(|t| t.enum_vals.iter())
            .find(|(name_off, _)| self.str_at(*name_off) == variant)
            .map(|(_, val)| *val)
            .ok_or_else(|| BtfError::NoEnumValue(variant.to_string()))
    }

    /// Bitmask of the iterator-kind bits: the OR of every value in the enum that defines
    /// `ITER_IOVEC`. ANDing the kind field with it drops the direction bit packed into the
    /// pre-5.14 `iov_iter.type`, and is a no-op on the dedicated `iter_type` field.
    pub fn iter_kind_mask(&self) -> Result<i64, BtfError> {
        self.types
            .iter()
            .filter(|t| t.kind == KIND_ENUM || t.kind == KIND_ENUM64)
            .find(|t| {
                t.enum_vals
                    .iter()
                    .any(|(n, _)| self.str_at(*n) == "ITER_IOVEC")
            })
            .map(|t| t.enum_vals.iter().fold(0i64, |m, (_, v)| m | v))
            .ok_or_else(|| BtfError::NoEnumValue("ITER_IOVEC".to_string()))
    }

    /// Resolve every field and enum value a tcp_sendmsg/recvmsg plaintext capture needs.
    pub fn plaintext_support(&self) -> PlaintextSupport {
        let mut offsets = Vec::new();
        let mut missing = Vec::new();
        for (root, path) in REQUIRED_FIELDS {
            match self.offset_of(root, path) {
                Ok(off) => offsets.push((format!("{root}.{}", path.join(".")), off)),
                Err(_) => missing.push(format!("{root}.{}", path.join("."))),
            }
        }
        // The kind field is `iter_type` since 5.14 and `type` before it; either is fine.
        let iter_kind = ITER_KIND_ALTS
            .iter()
            .find_map(|(r, p)| self.offset_of(r, p).ok().map(|o| (p.join("."), o)));
        if iter_kind.is_none() {
            missing.push("iov_iter kind field (iter_type/type)".into());
        }
        let payload = PAYLOAD_PTR_ALTS
            .iter()
            .find_map(|(r, p)| self.offset_of(r, p).ok().map(|o| (p.join("."), o)));
        if payload.is_none() {
            missing.push("iov_iter payload pointer (ubuf/__iov/iov)".into());
        }
        // ITER_IOVEC exists on every supported kernel; ITER_UBUF only from 6.7.
        let iovec = self.enum_value("ITER_IOVEC");
        if iovec.is_err() {
            missing.push("enum ITER_IOVEC".into());
        }
        let kind_mask = self.iter_kind_mask();
        if kind_mask.is_err() {
            missing.push("iov_iter kind mask".into());
        }
        let ubuf = self.enum_value("ITER_UBUF").ok();

        PlaintextSupport {
            offsets,
            iter_kind,
            iter_kind_mask: kind_mask.ok(),
            payload,
            iter_iovec: iovec.ok(),
            iter_ubuf: ubuf,
            missing,
        }
    }

    fn str_at(&self, off: u32) -> &str {
        let off = off as usize;
        if off >= self.strings.len() {
            return "";
        }
        let tail = &self.strings[off..];
        let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        std::str::from_utf8(&tail[..end]).unwrap_or("")
    }

    fn type_at(&self, id: u32) -> Option<&Type> {
        // id 0 is void; stored types start at id 1.
        if id == 0 {
            return None;
        }
        self.types.get((id - 1) as usize)
    }

    fn struct_id_by_name(&self, name: &str) -> Option<u32> {
        self.types.iter().enumerate().find_map(|(i, t)| {
            ((t.kind == KIND_STRUCT || t.kind == KIND_UNION) && self.str_at(t.name_off) == name)
                .then_some(i as u32 + 1)
        })
    }

    /// Resolve typedef/const/volatile/restrict down to the underlying type id.
    fn strip_modifiers(&self, mut id: u32) -> u32 {
        while let Some(t) = self.type_at(id) {
            match t.kind {
                KIND_TYPEDEF | KIND_CONST | KIND_VOLATILE | KIND_RESTRICT => id = t.size_or_type,
                _ => break,
            }
        }
        id
    }

    /// Bit offset of `name` within struct/union `id`, descending anonymous members.
    /// Returns the accumulated bit offset and the field's type id.
    fn find_member(&self, id: u32, name: &str) -> Option<(u32, u32)> {
        let t = self.type_at(self.strip_modifiers(id))?;
        if t.kind != KIND_STRUCT && t.kind != KIND_UNION {
            return None;
        }
        for m in &t.members {
            let mname = self.str_at(m.name_off);
            // The top byte carries a bitfield size on kind_flag structs; a byte-aligned field never sets it.
            let bit = m.offset & 0x00ff_ffff;
            if mname == name {
                return Some((bit, m.type_id));
            }
            if mname.is_empty()
                && let Some((sub, tid)) = self.find_member(m.type_id, name)
            {
                return Some((bit.saturating_add(sub), tid));
            }
        }
        None
    }
}

/// Kernel fields plaintext capture reads; shared so startup, the smoke gate and the
/// capture path agree on one list. Each is reached from the probe's argument structs.
pub const REQUIRED_FIELDS: &[(&str, &[&str])] = &[
    ("sock", &["__sk_common", "skc_family"]),
    ("sock", &["__sk_common", "skc_dport"]),
    ("sock", &["__sk_common", "skc_num"]),
    ("msghdr", &["msg_iter"]),
    ("iov_iter", &["iov_offset"]),
    ("iov_iter", &["count"]),
];

/// The iterator-kind field: `iter_type` since 5.14, `type` before it (5.10, RHEL 8.10's 4.18),
/// where the read/write direction is packed into the low bit — capture masks it off with
/// `iter_kind_mask` before comparing to the `ITER_*` enum values.
pub const ITER_KIND_ALTS: &[(&str, &[&str])] =
    &[("iov_iter", &["iter_type"]), ("iov_iter", &["type"])];

/// The payload pointer's union member was respelled at 6.4 (`iov`→`__iov`) and 6.7 (`ubuf`);
/// capture uses whichever the running kernel has.
pub const PAYLOAD_PTR_ALTS: &[(&str, &[&str])] = &[
    ("iov_iter", &["ubuf"]),
    ("iov_iter", &["__ubuf_iovec"]),
    ("iov_iter", &["__iov"]),
    ("iov_iter", &["iov"]),
];

/// What a kernel can offer plaintext capture, resolved once at load time.
pub struct PlaintextSupport {
    pub offsets: Vec<(String, u32)>,
    /// The iterator-kind field actually present: `("iter_type", off)` or `("type", off)`.
    pub iter_kind: Option<(String, u32)>,
    /// Mask applied to the kind field to drop the pre-5.14 packed direction bit.
    pub iter_kind_mask: Option<i64>,
    pub payload: Option<(String, u32)>,
    pub iter_iovec: Option<i64>,
    pub iter_ubuf: Option<i64>,
    pub missing: Vec<String>,
}

impl PlaintextSupport {
    /// Capture can run only when nothing required is missing.
    pub fn available(&self) -> bool {
        self.missing.is_empty()
    }

    /// One log line; the `supported`/`unsupported` word is what the CI smoke gate greps.
    pub fn summary(&self) -> String {
        if self.available() {
            let ubuf = self
                .iter_ubuf
                .map(|v| format!(", ITER_UBUF={v}"))
                .unwrap_or_else(|| " (no ITER_UBUF; iovec-only kernel)".into());
            let payload = self
                .payload
                .as_ref()
                .map(|(n, o)| format!("{n}@{o}"))
                .unwrap_or_default();
            let kind = self
                .iter_kind
                .as_ref()
                .map(|(n, o)| format!("{n}@{o}"))
                .unwrap_or_default();
            format!(
                "[plaintext] kernel BTF supported: {} fields, kind {kind} mask {:#x}, payload {payload}, ITER_IOVEC={}{ubuf}",
                self.offsets.len(),
                self.iter_kind_mask.unwrap_or(0),
                self.iter_iovec.unwrap_or(-1),
            )
        } else {
            format!(
                "[plaintext] kernel BTF unsupported: missing {}",
                self.missing.join(", ")
            )
        }
    }
}

/// Detect plaintext-capture support on the running kernel (reads `/sys/kernel/btf/vmlinux`).
pub fn detect_plaintext_support() -> PlaintextSupport {
    match Btf::from_sys_fs() {
        Ok(btf) => btf.plaintext_support(),
        Err(e) => PlaintextSupport {
            offsets: Vec::new(),
            iter_kind: None,
            iter_kind_mask: None,
            payload: None,
            iter_iovec: None,
            iter_ubuf: None,
            missing: vec![e.to_string()],
        },
    }
}

fn parse_types(mut data: &[u8]) -> Result<Vec<Type>, BtfError> {
    let mut types = Vec::new();
    while !data.is_empty() {
        if data.len() < 12 {
            return Err(BtfError::BadBlob("truncated btf_type"));
        }
        let name_off = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let info = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let size_or_type = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        let vlen = (info & 0xffff) as usize;
        let kind = (info >> 24) & 0x1f;
        data = &data[12..];

        let trailing = kind_trailing_len(kind, vlen);
        if data.len() < trailing {
            return Err(BtfError::BadBlob("truncated btf_type trailing data"));
        }

        let mut members = Vec::new();
        let mut enum_vals = Vec::new();
        if kind == KIND_STRUCT || kind == KIND_UNION {
            members.reserve(vlen);
            for i in 0..vlen {
                let b = &data[i * 12..i * 12 + 12];
                members.push(Member {
                    name_off: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                    type_id: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                    offset: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
                });
            }
        } else if kind == KIND_ENUM {
            for i in 0..vlen {
                let b = &data[i * 8..i * 8 + 8];
                let name = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                let val = i32::from_le_bytes([b[4], b[5], b[6], b[7]]) as i64;
                enum_vals.push((name, val));
            }
        } else if kind == KIND_ENUM64 {
            for i in 0..vlen {
                let b = &data[i * 12..i * 12 + 12];
                let name = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                let lo = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as u64;
                let hi = u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as u64;
                enum_vals.push((name, ((hi << 32) | lo) as i64));
            }
        }

        types.push(Type {
            name_off,
            kind,
            size_or_type,
            members,
            enum_vals,
        });
        data = &data[trailing..];
    }
    Ok(types)
}

/// Bytes of per-kind data that follow the 12-byte `btf_type` common header.
fn kind_trailing_len(kind: u32, vlen: usize) -> usize {
    match kind {
        INT_KIND => 4,
        KIND_ARRAY => 12,
        KIND_STRUCT | KIND_UNION => vlen * 12,
        KIND_ENUM => vlen * 8,
        KIND_FUNC_PROTO => vlen * 8,
        KIND_VAR => 4,
        KIND_DATASEC => vlen * 12,
        KIND_DECL_TAG => 4,
        KIND_ENUM64 => vlen * 12,
        // PTR, FWD, TYPEDEF, VOLATILE, CONST, RESTRICT, FUNC, FLOAT, TYPE_TAG carry no trailing data.
        KIND_PTR | FLOAT_KIND => 0,
        _ => 0,
    }
}

fn rd32(raw: &[u8], at: usize) -> Result<u32, BtfError> {
    raw.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(BtfError::BadBlob("header truncated"))
}

fn describe_io(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => "no /sys/kernel/btf/vmlinux (CONFIG_DEBUG_INFO_BTF off)".into(),
        io::ErrorKind::PermissionDenied => "permission denied reading kernel BTF".into(),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_btf() -> Option<Btf> {
        if !Path::new("/sys/kernel/btf/vmlinux").exists() {
            return None;
        }
        Some(Btf::from_sys_fs().expect("host kernel BTF should parse"))
    }

    #[test]
    fn resolves_destination_port_through_nested_anon_members() {
        let Some(btf) = host_btf() else {
            eprintln!("skip: no kernel BTF on this host");
            return;
        };
        // skc_dport lives in sock_common inside an anonymous union+struct; sock embeds it at 0.
        let dport = btf
            .offset_of("sock", &["__sk_common", "skc_dport"])
            .expect("skc_dport resolves");
        let num = btf
            .offset_of("sock", &["__sk_common", "skc_num"])
            .expect("skc_num resolves");
        // skc_num is the local port, two bytes past the big-endian destination port.
        assert_eq!(num, dport + 2, "skc_num should follow skc_dport");
    }

    #[test]
    fn resolves_iov_iter_layout() {
        let Some(btf) = host_btf() else {
            return;
        };
        assert_eq!(
            btf.offset_of("iov_iter", &["iter_type"])
                .expect("iter_type resolves"),
            0
        );
        // iov_offset sits after the three leading bytes plus padding on every post-3.19 layout.
        let iov_offset = btf
            .offset_of("iov_iter", &["iov_offset"])
            .expect("iov_offset resolves");
        let count = btf
            .offset_of("iov_iter", &["count"])
            .expect("count resolves");
        assert!(count > iov_offset, "count follows iov_offset");
    }

    #[test]
    fn missing_struct_and_field_are_reported_not_panicked() {
        let Some(btf) = host_btf() else {
            return;
        };
        assert!(matches!(
            btf.offset_of("not_a_real_struct_xyz", &["x"]),
            Err(BtfError::NoStruct(_))
        ));
        assert!(matches!(
            btf.offset_of("sock", &["no_such_field_xyz"]),
            Err(BtfError::NoField { .. })
        ));
    }

    #[test]
    fn has_gates_on_real_fields() {
        let Some(btf) = host_btf() else {
            return;
        };
        assert!(btf.has("sock", &["__sk_common", "skc_dport"]));
        assert!(!btf.has("sock", &["__sk_common", "nope"]));
    }

    #[test]
    fn bad_blob_errors_cleanly() {
        assert!(matches!(Btf::parse(&[0u8; 4]), Err(BtfError::BadBlob(_))));
        assert!(matches!(Btf::parse(&[]), Err(BtfError::BadBlob(_))));
    }

    /// The whole point of load-time resolution: a kernel with the pre-6.4 `iov_iter`
    /// (payload member named `iov`, no anonymous union) parses to different offsets, and
    /// the post-6.4 spelling `__iov` is correctly absent. Proven without that kernel.
    #[test]
    fn parses_a_pre_6_4_iov_iter_layout() {
        let blob = build_pre64_iov_iter_btf();
        let btf = Btf::parse(&blob).expect("synthetic BTF should parse");
        let off = |p: &str| btf.offset_of("iov_iter", &[p]).expect("field resolves");
        assert_eq!(off("iter_type"), 0);
        assert_eq!(off("iov_offset"), 8);
        assert_eq!(off("iov"), 16);
        assert_eq!(off("count"), 24);
        // The modern spelling must be reported missing, so capture picks `iov` here.
        assert!(matches!(
            btf.offset_of("iov_iter", &["__iov"]),
            Err(BtfError::NoField { .. })
        ));
    }

    #[test]
    fn reads_iter_type_enum_values_from_the_kernel() {
        let Some(btf) = host_btf() else {
            return;
        };
        // ITER_IOVEC exists on every supported kernel; its number differs pre/post-6.7.
        assert!(btf.enum_value("ITER_IOVEC").is_ok());
        assert!(matches!(
            btf.enum_value("ITER_DEFINITELY_NOT_A_REAL_ONE"),
            Err(BtfError::NoEnumValue(_))
        ));
    }

    #[test]
    fn enum_values_come_from_btf_not_a_hardcoded_table() {
        // A kernel that numbers ITER_IOVEC=0 and has no ITER_UBUF (pre-6.7 shape).
        let blob = build_enum_btf(&[("ITER_IOVEC", 0), ("ITER_KVEC", 1), ("ITER_BVEC", 2)]);
        let btf = Btf::parse(&blob).expect("synthetic BTF should parse");
        assert_eq!(btf.enum_value("ITER_IOVEC").expect("ITER_IOVEC present"), 0);
        assert_eq!(btf.enum_value("ITER_BVEC").expect("ITER_BVEC present"), 2);
        assert!(matches!(
            btf.enum_value("ITER_UBUF"),
            Err(BtfError::NoEnumValue(_))
        ));
    }

    /// Minimal BTF with a single named enum `iter_type`.
    fn build_enum_btf(variants: &[(&str, i32)]) -> Vec<u8> {
        let mut strings = vec![0u8];
        let intern = |s: &str, strings: &mut Vec<u8>| {
            let off = strings.len() as u32;
            strings.extend_from_slice(s.as_bytes());
            strings.push(0);
            off
        };
        let n_enum = intern("iter_type", &mut strings);
        let var_offs: Vec<u32> = variants
            .iter()
            .map(|(n, _)| intern(n, &mut strings))
            .collect();

        let mut types = Vec::new();
        let info = (KIND_ENUM << 24) | (variants.len() as u32 & 0xffff);
        types.extend_from_slice(&n_enum.to_le_bytes());
        types.extend_from_slice(&info.to_le_bytes());
        types.extend_from_slice(&4u32.to_le_bytes()); // size
        for (off, (_, val)) in var_offs.iter().zip(variants) {
            types.extend_from_slice(&off.to_le_bytes());
            types.extend_from_slice(&val.to_le_bytes());
        }

        let mut blob = Vec::new();
        blob.extend_from_slice(&BTF_MAGIC.to_le_bytes());
        blob.push(1);
        blob.push(0);
        blob.extend_from_slice(&24u32.to_le_bytes());
        blob.extend_from_slice(&0u32.to_le_bytes());
        blob.extend_from_slice(&(types.len() as u32).to_le_bytes());
        blob.extend_from_slice(&(types.len() as u32).to_le_bytes());
        blob.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        blob.extend_from_slice(&types);
        blob.extend_from_slice(&strings);
        blob
    }

    #[test]
    fn plaintext_support_is_available_on_this_host() {
        let Some(btf) = host_btf() else {
            return;
        };
        let s = btf.plaintext_support();
        assert!(s.available(), "missing: {:?}", s.missing);
        assert!(s.summary().contains("supported"));
    }

    /// Minimal little-endian vmlinux-style BTF: one int, one flat `struct iov_iter`
    /// with the pre-6.4 members at byte offsets 0/8/16/24.
    fn build_pre64_iov_iter_btf() -> Vec<u8> {
        let mut strings = vec![0u8]; // index 0 is the empty string
        let intern = |s: &str, strings: &mut Vec<u8>| {
            let off = strings.len() as u32;
            strings.extend_from_slice(s.as_bytes());
            strings.push(0);
            off
        };
        let n_iov_iter = intern("iov_iter", &mut strings);
        let n_iter_type = intern("iter_type", &mut strings);
        let n_iov_offset = intern("iov_offset", &mut strings);
        let n_iov = intern("iov", &mut strings);
        let n_count = intern("count", &mut strings);
        let n_nr_segs = intern("nr_segs", &mut strings);

        let mut types = Vec::new();
        let btf_type = |name_off: u32, kind: u32, vlen: u32, size_or_type: u32| {
            let info = (kind << 24) | (vlen & 0xffff);
            [
                name_off.to_le_bytes(),
                info.to_le_bytes(),
                size_or_type.to_le_bytes(),
            ]
            .concat()
        };
        // id 1: INT, size 8 (the member types; their id is all that matters here).
        types.extend_from_slice(&btf_type(0, INT_KIND, 0, 8));
        types.extend_from_slice(&0u32.to_le_bytes()); // int encoding word
        // id 2: struct iov_iter, 5 members, size 40.
        types.extend_from_slice(&btf_type(n_iov_iter, KIND_STRUCT, 5, 40));
        let member = |name_off: u32, bit_off: u32| {
            [
                name_off.to_le_bytes(),
                1u32.to_le_bytes(),
                bit_off.to_le_bytes(),
            ]
            .concat()
        };
        types.extend_from_slice(&member(n_iter_type, 0));
        types.extend_from_slice(&member(n_iov_offset, 64));
        types.extend_from_slice(&member(n_iov, 128));
        types.extend_from_slice(&member(n_count, 192));
        types.extend_from_slice(&member(n_nr_segs, 256));

        let mut blob = Vec::new();
        blob.extend_from_slice(&BTF_MAGIC.to_le_bytes());
        blob.push(1); // version
        blob.push(0); // flags
        blob.extend_from_slice(&24u32.to_le_bytes()); // hdr_len
        blob.extend_from_slice(&0u32.to_le_bytes()); // type_off
        blob.extend_from_slice(&(types.len() as u32).to_le_bytes()); // type_len
        blob.extend_from_slice(&(types.len() as u32).to_le_bytes()); // str_off
        blob.extend_from_slice(&(strings.len() as u32).to_le_bytes()); // str_len
        blob.extend_from_slice(&types);
        blob.extend_from_slice(&strings);
        blob
    }

    /// A pre-5.14 kernel (5.10, RHEL 8.10's 4.18): the kind field is `type`, not `iter_type`,
    /// with the read/write direction packed into the low bit, and `ITER_*` are an anonymous
    /// enum of flavour bits. Capture must fall back to `type` and mask the direction off.
    #[test]
    fn resolves_pre_5_14_type_field_and_masks_direction() {
        let blob = build_pre514_iov_iter_btf();
        let btf = Btf::parse(&blob).expect("synthetic BTF should parse");
        // No dedicated iter_type field on this kernel; capture falls back to `type` at 0.
        assert!(matches!(
            btf.offset_of("iov_iter", &["iter_type"]),
            Err(BtfError::NoField { .. })
        ));
        let s = btf.plaintext_support();
        assert_eq!(s.iter_kind, Some(("type".to_string(), 0)));
        // Mask is the OR of the enum's values (2|4|8, IOVEC=0), leaving bit 0 (direction) clear.
        let mask = s.iter_kind_mask.expect("kind mask from the ITER_* enum");
        assert_eq!(mask, 2 | 4 | 8);
        assert_eq!(btf.enum_value("ITER_IOVEC").expect("ITER_IOVEC present"), 0);
        // The stored field is `flavour | WRITE(1)`; masking the direction bit recovers the flavour.
        assert_eq!(1 & mask, 0, "write IOVEC (0|1) resolves to ITER_IOVEC");
        assert_eq!(5 & mask, 4, "write BVEC (4|1) resolves to ITER_BVEC");
    }

    /// Minimal BTF: `struct iov_iter { int type; size_t iov_offset; void *iov; size_t count; }`
    /// plus an anonymous flavour enum, matching the pre-5.14 layout.
    fn build_pre514_iov_iter_btf() -> Vec<u8> {
        let mut strings = vec![0u8];
        let intern = |s: &str, strings: &mut Vec<u8>| {
            let off = strings.len() as u32;
            strings.extend_from_slice(s.as_bytes());
            strings.push(0);
            off
        };
        let n_iov_iter = intern("iov_iter", &mut strings);
        let n_type = intern("type", &mut strings);
        let n_iov_offset = intern("iov_offset", &mut strings);
        let n_iov = intern("iov", &mut strings);
        let n_count = intern("count", &mut strings);
        let variants: [(&str, i32); 4] = [
            ("ITER_IOVEC", 0),
            ("ITER_KVEC", 2),
            ("ITER_BVEC", 4),
            ("ITER_PIPE", 8),
        ];
        let var_offs: Vec<u32> = variants
            .iter()
            .map(|(n, _)| intern(n, &mut strings))
            .collect();

        let mut types = Vec::new();
        let btf_type = |name_off: u32, kind: u32, vlen: u32, size_or_type: u32| {
            let info = (kind << 24) | (vlen & 0xffff);
            [
                name_off.to_le_bytes(),
                info.to_le_bytes(),
                size_or_type.to_le_bytes(),
            ]
            .concat()
        };
        // id 1: INT, size 8 (member element type).
        types.extend_from_slice(&btf_type(0, INT_KIND, 0, 8));
        types.extend_from_slice(&0u32.to_le_bytes());
        // id 2: struct iov_iter, 4 members at 0/8/16/24.
        types.extend_from_slice(&btf_type(n_iov_iter, KIND_STRUCT, 4, 32));
        let member = |name_off: u32, bit_off: u32| {
            [
                name_off.to_le_bytes(),
                1u32.to_le_bytes(),
                bit_off.to_le_bytes(),
            ]
            .concat()
        };
        types.extend_from_slice(&member(n_type, 0));
        types.extend_from_slice(&member(n_iov_offset, 64));
        types.extend_from_slice(&member(n_iov, 128));
        types.extend_from_slice(&member(n_count, 192));
        // id 3: anonymous flavour enum.
        types.extend_from_slice(&btf_type(0, KIND_ENUM, variants.len() as u32, 4));
        for (off, (_, val)) in var_offs.iter().zip(variants) {
            types.extend_from_slice(&off.to_le_bytes());
            types.extend_from_slice(&val.to_le_bytes());
        }

        let mut blob = Vec::new();
        blob.extend_from_slice(&BTF_MAGIC.to_le_bytes());
        blob.push(1);
        blob.push(0);
        blob.extend_from_slice(&24u32.to_le_bytes());
        blob.extend_from_slice(&0u32.to_le_bytes());
        blob.extend_from_slice(&(types.len() as u32).to_le_bytes());
        blob.extend_from_slice(&(types.len() as u32).to_le_bytes());
        blob.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        blob.extend_from_slice(&types);
        blob.extend_from_slice(&strings);
        blob
    }
}
