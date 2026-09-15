//! Binary codec for a sorted list of CLOCK facts.
//!
//! One format serves three tiers of the cache: the per-file artifact
//! (layer 1), each of the 256 path-hash buckets, and the global root
//! (layer 2). A tier is nothing more than a k-way merge of the tier
//! below, so one codec and one merge routine cover all of them.
//!
//! # Why fixed-width records
//!
//! The consumer is [`crate::org::clock_overlap`]'s sweep, which reads
//! two `i64`s per step. Anything that has to allocate a node per
//! record before the sweep can start — JSON, a serde tree, the
//! `orgize` syntax tree — was measured to load *slower* than simply
//! re-parsing the source. A flat array of `Record`s is the only shape
//! where "load" is materially cheaper than "recompute".
//!
//! # Layout
//!
//! ```text
//! Header   64 bytes   magic, ABI versions, counts, body digest
//! Records  N × 40     sorted by (start, end, path_id, byte_offset)
//! Strings  var        length-prefixed UTF-8, deduplicated
//! ```
//!
//! The raw CLOCK line is deliberately **not** stored here: the sweep
//! does not need it, and it would triple the record size for the
//! benefit of rendering alone. Rendering fetches it from the layer-1
//! artifact by `(path_id, byte_offset)`.
//!
//! # Invalidation is structural
//!
//! Every ABI that could change the meaning of a byte is in the header
//! and folded into [`Blob::digest`]. A parser upgrade, a projection
//! change, or a sort-key change therefore yields a different digest,
//! and the layer-2 keys that embed it stop matching. There is no
//! "wrong version silently decodes" failure mode.

use std::cmp::Ordering;
use std::collections::HashMap;

use sha2::{Digest, Sha256};

/// Four bytes at offset 0 of every blob.
pub const MAGIC: &[u8; 4] = b"OCLK";

/// Bump when the byte layout of header, record, or string table
/// changes.
pub const FORMAT_VERSION: u16 = 2;

/// Bump when the sort key changes. Consumers assume records are
/// ordered by this key and will misbehave on a different order.
pub const SORT_ABI: u16 = 1;

const HEADER_LEN: usize = 64;
/// `i64 start, i64 end, u32 path, u32 heading, u32 offset, u32 line,
/// i16 tz, u16 flags, u32 tag` — exactly 40 bytes, no padding on the wire.
const RECORD_LEN: usize = 40;

/// Sentinel for "no heading" in [`Record::heading_id`].
pub const NO_HEADING: u32 = u32::MAX;

/// Sentinel for "no customer tag" in [`Record::tag_id`].
pub const NO_TAG: u32 = u32::MAX;

/// Bit in [`Record::flags`]: the source line carried seconds.
pub const FLAG_HAS_SECONDS: u16 = 1 << 0;

/// Bit in [`Record::flags`]: the source line uses the `FILED:` keyword.
pub const FLAG_FILED: u16 = 1 << 1;

/// One CLOCK or FILED fact, reduced to what a sorted global list needs.
///
/// `path_id`, `heading_id`, and `tag_id` index the blob's string table; they
/// are only meaningful together with the [`Blob`] they came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Record {
    /// Start instant, UTC seconds since the chrono era (matches
    /// [`crate::org::clock::ClockLine::start_utc_seconds`]).
    pub start: i64,
    /// End instant, same epoch. Never less than `start`.
    pub end: i64,
    /// Index of the canonical file path in the string table.
    pub path_id: u32,
    /// Index of the enclosing heading title, or [`NO_HEADING`].
    pub heading_id: u32,
    /// Byte offset of the source line's first byte within the file.
    pub byte_offset: u32,
    /// 1-based line number, for human-facing reports only.
    pub line: u32,
    /// Numerical UTC offset in minutes, as written on the line.
    pub tz_minutes: i16,
    /// Bit flags; see `FLAG_*`.
    pub flags: u16,
    /// Index of the FILED customer tag, or [`NO_TAG`] for CLOCK records.
    pub tag_id: u32,
}

impl Record {
    /// The total order every blob is sorted by.
    ///
    /// `path_id` participates so that ties on instants are broken
    /// deterministically and the blob's digest does not depend on the
    /// order files happened to be visited.
    pub fn sort_key(&self) -> (i64, i64, u32, u32) {
        (self.start, self.end, self.path_id, self.byte_offset)
    }
}

/// Header, record array, and string table of one sorted list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    /// Projection ABI the records were produced under. Owned by the
    /// projection module; the codec only carries it.
    pub projection_abi: u16,
    /// Parser ABI (the `orgize` fork revision) the records were
    /// produced under. Owned by the projection module.
    pub parser_abi: u16,
    /// Records, sorted by [`Record::sort_key`].
    pub records: Vec<Record>,
    /// Interned strings referenced by `path_id`, `heading_id`, or `tag_id`.
    pub strings: Vec<String>,
}

/// Why a byte slice was refused as a [`Blob`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than a header.
    TooShort,
    /// First four bytes are not [`MAGIC`].
    BadMagic,
    /// `format_version` is not [`FORMAT_VERSION`].
    UnsupportedFormat(u16),
    /// `sort_abi` is not [`SORT_ABI`].
    UnsupportedSort(u16),
    /// Declared counts point past the end of the input.
    Truncated,
    /// Body digest in the header does not match the body bytes.
    DigestMismatch,
    /// A string table entry is not valid UTF-8.
    BadString,
    /// A record references a string index the table does not have.
    DanglingString,
    /// Records are not in [`Record::sort_key`] order.
    Unsorted,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "blob shorter than header"),
            Self::BadMagic => write!(f, "not a clock blob (bad magic)"),
            Self::UnsupportedFormat(v) => write!(f, "unsupported format version {}", v),
            Self::UnsupportedSort(v) => write!(f, "unsupported sort ABI {}", v),
            Self::Truncated => write!(f, "blob truncated"),
            Self::DigestMismatch => write!(f, "blob body digest mismatch"),
            Self::BadString => write!(f, "string table entry is not UTF-8"),
            Self::DanglingString => write!(f, "record references missing string"),
            Self::Unsorted => write!(f, "records are not sorted"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Why a [`Blob`] could not be encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// `records` is not sorted by [`Record::sort_key`].
    Unsorted,
    /// A record references a string index outside `strings`.
    DanglingString,
    /// More records or strings than the `u32` header fields can hold.
    TooLarge,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsorted => write!(f, "records must be sorted before encoding"),
            Self::DanglingString => write!(f, "record references missing string"),
            Self::TooLarge => write!(f, "too many records or strings for the header"),
        }
    }
}

impl std::error::Error for EncodeError {}

impl Blob {
    /// An empty list under the given ABIs. Still a valid, encodable
    /// blob — an empty file result is cached exactly like a full one.
    pub fn empty(projection_abi: u16, parser_abi: u16) -> Self {
        Self {
            projection_abi,
            parser_abi,
            records: Vec::new(),
            strings: Vec::new(),
        }
    }

    fn is_sorted(&self) -> bool {
        self.records
            .windows(2)
            .all(|w| w[0].sort_key() <= w[1].sort_key())
    }

    fn strings_resolve(&self) -> bool {
        let n = self.strings.len();
        self.records.iter().all(|r| {
            (r.path_id as usize) < n
                && (r.heading_id == NO_HEADING || (r.heading_id as usize) < n)
                && (r.tag_id == NO_TAG || (r.tag_id as usize) < n)
        })
    }

    /// Serialise to bytes. Refuses unsorted input rather than sorting
    /// it: the caller owns the sort, and a silent re-sort here would
    /// let an out-of-order producer go unnoticed.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        if !self.is_sorted() {
            return Err(EncodeError::Unsorted);
        }
        if !self.strings_resolve() {
            return Err(EncodeError::DanglingString);
        }
        let record_count = u32::try_from(self.records.len()).map_err(|_| EncodeError::TooLarge)?;
        let string_count = u32::try_from(self.strings.len()).map_err(|_| EncodeError::TooLarge)?;

        let mut body = Vec::with_capacity(self.records.len() * RECORD_LEN + 64);
        for r in &self.records {
            body.extend_from_slice(&r.start.to_le_bytes());
            body.extend_from_slice(&r.end.to_le_bytes());
            body.extend_from_slice(&r.path_id.to_le_bytes());
            body.extend_from_slice(&r.heading_id.to_le_bytes());
            body.extend_from_slice(&r.byte_offset.to_le_bytes());
            body.extend_from_slice(&r.line.to_le_bytes());
            body.extend_from_slice(&r.tz_minutes.to_le_bytes());
            body.extend_from_slice(&r.flags.to_le_bytes());
            body.extend_from_slice(&r.tag_id.to_le_bytes());
        }
        let string_table_offset =
            u32::try_from(HEADER_LEN + body.len()).map_err(|_| EncodeError::TooLarge)?;
        for s in &self.strings {
            let len = u32::try_from(s.len()).map_err(|_| EncodeError::TooLarge)?;
            body.extend_from_slice(&len.to_le_bytes());
            body.extend_from_slice(s.as_bytes());
        }

        let digest = Sha256::digest(&body);

        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.parser_abi.to_le_bytes());
        out.extend_from_slice(&self.projection_abi.to_le_bytes());
        out.extend_from_slice(&SORT_ABI.to_le_bytes());
        out.extend_from_slice(&record_count.to_le_bytes());
        out.extend_from_slice(&string_table_offset.to_le_bytes());
        out.extend_from_slice(&string_count.to_le_bytes());
        out.extend_from_slice(&digest);
        debug_assert_eq!(out.len(), 4 + 2 * 4 + 4 * 3 + 32);
        out.resize(HEADER_LEN, 0);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Parse bytes produced by [`Blob::encode`]. Every structural
    /// invariant is checked; a blob that passes is safe to hand to the
    /// sweep without further validation.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < HEADER_LEN {
            return Err(DecodeError::TooShort);
        }
        if &bytes[0..4] != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let u32_at =
            |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);

        let format = u16_at(4);
        if format != FORMAT_VERSION {
            return Err(DecodeError::UnsupportedFormat(format));
        }
        let parser_abi = u16_at(6);
        let projection_abi = u16_at(8);
        let sort_abi = u16_at(10);
        if sort_abi != SORT_ABI {
            return Err(DecodeError::UnsupportedSort(sort_abi));
        }
        let record_count = u32_at(12) as usize;
        let string_table_offset = u32_at(16) as usize;
        let string_count = u32_at(20) as usize;
        let header_digest = &bytes[24..56];

        let body = &bytes[HEADER_LEN..];
        let records_len = record_count
            .checked_mul(RECORD_LEN)
            .ok_or(DecodeError::Truncated)?;
        if string_table_offset != HEADER_LEN + records_len || body.len() < records_len {
            return Err(DecodeError::Truncated);
        }
        if Sha256::digest(body).as_slice() != header_digest {
            return Err(DecodeError::DigestMismatch);
        }

        let mut records = Vec::with_capacity(record_count);
        for i in 0..record_count {
            let r = &body[i * RECORD_LEN..(i + 1) * RECORD_LEN];
            let i64_at = |o: usize| i64::from_le_bytes(r[o..o + 8].try_into().expect("8 bytes"));
            let u32_r = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().expect("4 bytes"));
            records.push(Record {
                start: i64_at(0),
                end: i64_at(8),
                path_id: u32_r(16),
                heading_id: u32_r(20),
                byte_offset: u32_r(24),
                line: u32_r(28),
                tz_minutes: i16::from_le_bytes([r[32], r[33]]),
                flags: u16::from_le_bytes([r[34], r[35]]),
                tag_id: u32_r(36),
            });
        }

        let mut strings = Vec::with_capacity(string_count);
        let mut pos = records_len;
        for _ in 0..string_count {
            if pos + 4 > body.len() {
                return Err(DecodeError::Truncated);
            }
            let len = u32::from_le_bytes(body[pos..pos + 4].try_into().expect("4 bytes")) as usize;
            pos += 4;
            if pos + len > body.len() {
                return Err(DecodeError::Truncated);
            }
            let s =
                std::str::from_utf8(&body[pos..pos + len]).map_err(|_| DecodeError::BadString)?;
            strings.push(s.to_owned());
            pos += len;
        }

        let blob = Self {
            projection_abi,
            parser_abi,
            records,
            strings,
        };
        if !blob.strings_resolve() {
            return Err(DecodeError::DanglingString);
        }
        if !blob.is_sorted() {
            return Err(DecodeError::Unsorted);
        }
        Ok(blob)
    }

    /// SHA-256 of the encoded bytes. This is the blob's identity for
    /// layer-2 composition: a parent digest is built from its
    /// children's digests, never by re-hashing their contents.
    pub fn digest(&self) -> Result<[u8; 32], EncodeError> {
        Ok(Sha256::digest(self.encode()?).into())
    }

    /// Merge already-sorted blobs into one sorted blob.
    ///
    /// Strings are re-interned across inputs, so a path that appears
    /// in several children is stored once in the parent. Records are
    /// re-keyed to the parent's table before the k-way merge, because
    /// `path_id` is part of the sort key and must be comparable across
    /// inputs.
    ///
    /// All inputs must share the same ABIs; mixing is a programming
    /// error, not a data condition, so it panics in debug builds.
    pub fn merge(inputs: &[&Blob]) -> Blob {
        let (projection_abi, parser_abi) = match inputs.first() {
            Some(b) => (b.projection_abi, b.parser_abi),
            None => return Blob::empty(0, 0),
        };
        debug_assert!(
            inputs
                .iter()
                .all(|b| b.projection_abi == projection_abi && b.parser_abi == parser_abi),
            "merging blobs of different ABIs"
        );

        let mut interner = Interner::default();
        // Re-key each input's records into the shared string space.
        let mut streams: Vec<Vec<Record>> = inputs
            .iter()
            .map(|b| {
                let map: Vec<u32> = b.strings.iter().map(|s| interner.intern(s)).collect();
                let mut recs: Vec<Record> = b
                    .records
                    .iter()
                    .map(|r| Record {
                        path_id: map[r.path_id as usize],
                        heading_id: if r.heading_id == NO_HEADING {
                            NO_HEADING
                        } else {
                            map[r.heading_id as usize]
                        },
                        tag_id: if r.tag_id == NO_TAG {
                            NO_TAG
                        } else {
                            map[r.tag_id as usize]
                        },
                        ..*r
                    })
                    .collect();
                // Re-keying can reorder ties on path_id; restore the
                // invariant per stream before merging.
                recs.sort_by_key(Record::sort_key);
                recs
            })
            .collect();

        let total: usize = streams.iter().map(Vec::len).sum();
        let mut out = Vec::with_capacity(total);
        let mut heads = vec![0usize; streams.len()];
        loop {
            let mut best: Option<usize> = None;
            for (i, s) in streams.iter().enumerate() {
                if heads[i] >= s.len() {
                    continue;
                }
                match best {
                    None => best = Some(i),
                    Some(b) => {
                        if s[heads[i]].sort_key().cmp(&streams[b][heads[b]].sort_key())
                            == Ordering::Less
                        {
                            best = Some(i);
                        }
                    }
                }
            }
            let Some(i) = best else { break };
            out.push(streams[i][heads[i]]);
            heads[i] += 1;
        }
        // Drop the now-consumed streams before moving `out` so the
        // borrow checker is satisfied and memory is released early.
        streams.clear();

        Blob {
            projection_abi,
            parser_abi,
            records: out,
            strings: interner.into_strings(),
        }
    }
}

/// Deduplicating string table builder.
#[derive(Default)]
pub struct Interner {
    index: HashMap<String, u32>,
    strings: Vec<String>,
}

impl Interner {
    /// Return the id for `s`, inserting it if unseen.
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.index.get(s) {
            return id;
        }
        let id = u32::try_from(self.strings.len()).expect("string table overflow");
        self.strings.push(s.to_owned());
        self.index.insert(s.to_owned(), id);
        id
    }

    /// Consume into the ordered table.
    pub fn into_strings(self) -> Vec<String> {
        self.strings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(start: i64, end: i64, path_id: u32, off: u32) -> Record {
        Record {
            start,
            end,
            path_id,
            heading_id: NO_HEADING,
            byte_offset: off,
            line: 1,
            tz_minutes: 60,
            flags: 0,
            tag_id: NO_TAG,
        }
    }

    fn sample() -> Blob {
        Blob {
            projection_abi: 3,
            parser_abi: 7,
            records: vec![
                rec(100, 200, 0, 10),
                rec(100, 200, 1, 5),
                rec(150, 250, 0, 40),
                Record {
                    heading_id: 2,
                    flags: FLAG_HAS_SECONDS,
                    tz_minutes: -300,
                    ..rec(300, 300, 1, 99)
                },
            ],
            strings: vec!["/a.org".into(), "/b.org".into(), "Some heading".into()],
        }
    }

    // C1
    #[test]
    fn round_trip_exact() {
        let b = sample();
        let bytes = b.encode().unwrap();
        let back = Blob::decode(&bytes).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn round_trip_preserves_filed_tag_reference() {
        let mut b = sample();
        b.strings.push("lok".into());
        b.records[0].flags |= FLAG_FILED;
        b.records[0].tag_id = 3;
        assert_eq!(Blob::decode(&b.encode().unwrap()).unwrap(), b);
    }

    // C1 (empty is a first-class value)
    #[test]
    fn round_trip_empty() {
        let b = Blob::empty(1, 1);
        let back = Blob::decode(&b.encode().unwrap()).unwrap();
        assert_eq!(back, b);
        assert!(back.records.is_empty());
    }

    // C2
    #[test]
    fn digest_is_deterministic() {
        assert_eq!(sample().digest().unwrap(), sample().digest().unwrap());
        let mut other = sample();
        other.records[0].line += 1;
        assert_ne!(sample().digest().unwrap(), other.digest().unwrap());
    }

    // C3
    #[test]
    fn rejects_foreign_header() {
        let mut bytes = sample().encode().unwrap();
        bytes[0] = b'X';
        assert_eq!(Blob::decode(&bytes), Err(DecodeError::BadMagic));

        let mut bytes = sample().encode().unwrap();
        bytes[4] = 0xFF; // format_version low byte
        assert!(matches!(
            Blob::decode(&bytes),
            Err(DecodeError::UnsupportedFormat(_))
        ));

        let mut bytes = sample().encode().unwrap();
        bytes[10] = 0xFF; // sort_abi low byte
        assert!(matches!(
            Blob::decode(&bytes),
            Err(DecodeError::UnsupportedSort(_))
        ));
    }

    #[test]
    fn rejects_previous_format_version() {
        let mut bytes = sample().encode().unwrap();
        bytes[4..6].copy_from_slice(&(FORMAT_VERSION - 1).to_le_bytes());
        assert_eq!(
            Blob::decode(&bytes),
            Err(DecodeError::UnsupportedFormat(FORMAT_VERSION - 1))
        );
    }

    // C3 (payload tamper is caught by the digest, not by luck)
    #[test]
    fn rejects_tampered_body() {
        let mut bytes = sample().encode().unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert_eq!(Blob::decode(&bytes), Err(DecodeError::DigestMismatch));
    }

    // C4
    #[test]
    fn rejects_truncated() {
        let bytes = sample().encode().unwrap();
        assert_eq!(Blob::decode(&bytes[..10]), Err(DecodeError::TooShort));
        // Cut inside the record array: length check fires before digest.
        assert_eq!(
            Blob::decode(&bytes[..HEADER_LEN + RECORD_LEN + 3]),
            Err(DecodeError::Truncated)
        );
    }

    // C5
    #[test]
    fn rejects_unsorted() {
        let mut b = sample();
        b.records.swap(0, 2);
        assert_eq!(b.encode(), Err(EncodeError::Unsorted));
    }

    #[test]
    fn rejects_dangling_string_on_encode() {
        let mut b = sample();
        // `heading_id` is not part of the sort key, so dangling it
        // exercises the string check in isolation from the sort check.
        b.records[0].heading_id = 42;
        assert_eq!(b.encode(), Err(EncodeError::DanglingString));
    }

    // C6
    #[test]
    fn merge_preserves_order_and_count() {
        let a = Blob {
            projection_abi: 1,
            parser_abi: 1,
            records: vec![rec(10, 20, 0, 0), rec(30, 40, 0, 1)],
            strings: vec!["/x.org".into()],
        };
        let b = Blob {
            projection_abi: 1,
            parser_abi: 1,
            records: vec![rec(5, 15, 0, 0), rec(30, 35, 0, 1)],
            strings: vec!["/y.org".into()],
        };
        let m = Blob::merge(&[&a, &b]);
        assert_eq!(m.records.len(), 4);
        assert!(
            m.records
                .windows(2)
                .all(|w| w[0].sort_key() <= w[1].sort_key()),
            "merged output must be sorted: {:?}",
            m.records
        );
        let starts: Vec<i64> = m.records.iter().map(|r| r.start).collect();
        assert_eq!(starts, vec![5, 10, 30, 30]);
        // Merged output is itself encodable (sorted + strings resolve).
        m.encode().unwrap();
    }

    // C6: shared strings are interned once in the parent
    #[test]
    fn merge_dedupes_shared_strings() {
        let a = Blob {
            projection_abi: 1,
            parser_abi: 1,
            records: vec![rec(1, 2, 0, 0)],
            strings: vec!["/shared.org".into()],
        };
        let b = Blob {
            projection_abi: 1,
            parser_abi: 1,
            records: vec![rec(3, 4, 0, 0)],
            strings: vec!["/shared.org".into()],
        };
        let m = Blob::merge(&[&a, &b]);
        assert_eq!(m.strings, vec!["/shared.org".to_string()]);
        assert!(m.records.iter().all(|r| r.path_id == 0));
    }

    #[test]
    fn merge_rekeys_filed_tag_ids() {
        let a = Blob {
            projection_abi: 1,
            parser_abi: 1,
            records: vec![Record {
                flags: FLAG_FILED,
                tag_id: 1,
                ..rec(1, 2, 0, 0)
            }],
            strings: vec!["/a.org".into(), "lok".into()],
        };
        let b = Blob {
            projection_abi: 1,
            parser_abi: 1,
            records: vec![Record {
                flags: FLAG_FILED,
                tag_id: 1,
                ..rec(3, 4, 0, 0)
            }],
            strings: vec!["/b.org".into(), "acme".into()],
        };
        let merged = Blob::merge(&[&a, &b]);
        let tags: Vec<&str> = merged
            .records
            .iter()
            .map(|r| merged.strings[r.tag_id as usize].as_str())
            .collect();
        assert_eq!(tags, vec!["lok", "acme"]);
    }

    #[test]
    fn merge_of_nothing_is_empty() {
        let m = Blob::merge(&[]);
        assert!(m.records.is_empty());
    }

    // Merging is associative in content: (a+b)+c == a+(b+c) == a+b+c.
    // This is what lets buckets be rebuilt independently of the root.
    #[test]
    fn merge_is_associative() {
        let mk = |s: i64, p: &str| Blob {
            projection_abi: 1,
            parser_abi: 1,
            records: vec![rec(s, s + 10, 0, 0)],
            strings: vec![p.into()],
        };
        let (a, b, c) = (mk(3, "/a"), mk(1, "/b"), mk(2, "/c"));
        let left = Blob::merge(&[&Blob::merge(&[&a, &b]), &c]);
        let right = Blob::merge(&[&a, &Blob::merge(&[&b, &c])]);
        let flat = Blob::merge(&[&a, &b, &c]);
        let keyed = |m: &Blob| -> Vec<(i64, String)> {
            m.records
                .iter()
                .map(|r| (r.start, m.strings[r.path_id as usize].clone()))
                .collect()
        };
        assert_eq!(keyed(&left), keyed(&right));
        assert_eq!(keyed(&left), keyed(&flat));
    }
}
