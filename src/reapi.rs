//! Bazel Remote Execution API (REAPI) cache interop.
//!
//! bazel-remote and friends expose an HTTP cache with two endpoints — `/ac/<h>`
//! for action results and `/cas/<h>` for content blobs — keyed by the **SHA-256**
//! hex of their contents, where an action result is a serialized
//! `build.bazel.remote.execution.v2.ActionResult` protobuf.
//!
//! This module provides exactly that: SHA-256 hashing and a small,
//! dependency-free encoder/decoder for the slice of `ActionResult` yatr needs
//! (output files + their digests, exit code, raw stdout). It does **not** pull
//! in `prost`/`protoc`; the wire format is hand-written and covered by a
//! known-bytes test against the protobuf spec.

// Protobuf field numbers, wire types, sizes and exit codes are all bounded by
// the spec; the casts below can't meaningfully truncate or wrap.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of `data` (the REAPI digest hash).
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(data) {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// One captured output file in an [`ActionResult`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutputFile {
    pub path: String,
    /// SHA-256 hex of the file contents (its CAS key).
    pub digest: String,
    pub size: u64,
    pub executable: bool,
}

/// One captured output symlink in an [`ActionResult`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutputSymlink {
    pub path: String,
    pub target: String,
}

/// The slice of `build.bazel.remote.execution.v2.ActionResult` yatr uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActionResult {
    pub output_files: Vec<OutputFile>,
    pub output_symlinks: Vec<OutputSymlink>,
    pub exit_code: i32,
    pub stdout: Vec<u8>,
}

// --- protobuf wire helpers -------------------------------------------------

fn put_varint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let byte = u8::try_from(n & 0x7f).unwrap_or(0);
        n >>= 7;
        if n == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

fn put_tag(field: u32, wire: u32, out: &mut Vec<u8>) {
    put_varint(u64::from((field << 3) | wire), out);
}

fn put_len_delimited(field: u32, data: &[u8], out: &mut Vec<u8>) {
    put_tag(field, 2, out);
    put_varint(data.len() as u64, out);
    out.extend_from_slice(data);
}

fn put_field_varint(field: u32, value: u64, out: &mut Vec<u8>) {
    put_tag(field, 0, out);
    put_varint(value, out);
}

fn encode_digest(hash: &str, size: u64) -> Vec<u8> {
    let mut d = Vec::new();
    put_len_delimited(1, hash.as_bytes(), &mut d); // Digest.hash
    if size != 0 {
        put_field_varint(2, size, &mut d); // Digest.size_bytes
    }
    d
}

fn encode_output_file(f: &OutputFile) -> Vec<u8> {
    let mut buf = Vec::new();
    put_len_delimited(1, f.path.as_bytes(), &mut buf); // OutputFile.path
    put_len_delimited(2, &encode_digest(&f.digest, f.size), &mut buf); // OutputFile.digest
    if f.executable {
        put_field_varint(4, 1, &mut buf); // OutputFile.is_executable
    }
    buf
}

fn encode_output_symlink(l: &OutputSymlink) -> Vec<u8> {
    let mut buf = Vec::new();
    put_len_delimited(1, l.path.as_bytes(), &mut buf); // OutputSymlink.path
    put_len_delimited(2, l.target.as_bytes(), &mut buf); // OutputSymlink.target
    buf
}

/// Encode an [`ActionResult`] to its REAPI protobuf bytes.
#[must_use]
pub fn encode_action_result(ar: &ActionResult) -> Vec<u8> {
    let mut buf = Vec::new();
    for f in &ar.output_files {
        put_len_delimited(2, &encode_output_file(f), &mut buf); // ActionResult.output_files
    }
    if ar.exit_code != 0 {
        put_field_varint(4, ar.exit_code as u64, &mut buf); // ActionResult.exit_code
    }
    if !ar.stdout.is_empty() {
        put_len_delimited(5, &ar.stdout, &mut buf); // ActionResult.stdout_raw
    }
    for l in &ar.output_symlinks {
        put_len_delimited(12, &encode_output_symlink(l), &mut buf); // ActionResult.output_symlinks
    }
    buf
}

// --- protobuf decode -------------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.varint()?).ok()?;
        let end = self.pos.checked_add(len)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    /// Skip a field of the given wire type. Returns `false` on malformed input.
    fn skip(&mut self, wire: u32) -> bool {
        match wire {
            0 => self.varint().is_some(),
            2 => self.bytes().is_some(),
            5 => self.advance(4),
            1 => self.advance(8),
            _ => false,
        }
    }

    const fn advance(&mut self, n: usize) -> bool {
        match self.pos.checked_add(n) {
            Some(end) if end <= self.buf.len() => {
                self.pos = end;
                true
            }
            _ => false,
        }
    }

    const fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }
}

fn decode_digest(bytes: &[u8]) -> (String, u64) {
    let mut r = Reader::new(bytes);
    let mut hash = String::new();
    let mut size = 0u64;
    while !r.done() {
        let Some(tag) = r.varint() else { break };
        let (field, wire) = ((tag >> 3) as u32, (tag & 7) as u32);
        match (field, wire) {
            (1, 2) => {
                hash = r
                    .bytes()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
            }
            (2, 0) => size = r.varint().unwrap_or(0),
            _ => {
                if !r.skip(wire) {
                    break;
                }
            }
        }
    }
    (hash, size)
}

fn decode_output_file(bytes: &[u8]) -> OutputFile {
    let mut r = Reader::new(bytes);
    let mut f = OutputFile::default();
    while !r.done() {
        let Some(tag) = r.varint() else { break };
        let (field, wire) = ((tag >> 3) as u32, (tag & 7) as u32);
        match (field, wire) {
            (1, 2) => {
                f.path = r
                    .bytes()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
            }
            (2, 2) => {
                if let Some(d) = r.bytes() {
                    let (hash, size) = decode_digest(d);
                    f.digest = hash;
                    f.size = size;
                }
            }
            (4, 0) => f.executable = r.varint().unwrap_or(0) != 0,
            _ => {
                if !r.skip(wire) {
                    break;
                }
            }
        }
    }
    f
}

fn decode_output_symlink(bytes: &[u8]) -> OutputSymlink {
    let mut r = Reader::new(bytes);
    let mut l = OutputSymlink::default();
    while !r.done() {
        let Some(tag) = r.varint() else { break };
        let (field, wire) = ((tag >> 3) as u32, (tag & 7) as u32);
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        match (field, wire) {
            (1, 2) => l.path = r.bytes().map(text).unwrap_or_default(),
            (2, 2) => l.target = r.bytes().map(text).unwrap_or_default(),
            _ => {
                if !r.skip(wire) {
                    break;
                }
            }
        }
    }
    l
}

/// Decode REAPI protobuf bytes into an [`ActionResult`]. Unknown fields are
/// skipped, so it tolerates results written by Bazel or other REAPI clients.
#[must_use]
pub fn decode_action_result(bytes: &[u8]) -> Option<ActionResult> {
    let mut r = Reader::new(bytes);
    let mut ar = ActionResult::default();
    while !r.done() {
        let tag = r.varint()?;
        let (field, wire) = ((tag >> 3) as u32, (tag & 7) as u32);
        match (field, wire) {
            (2, 2) => ar.output_files.push(decode_output_file(r.bytes()?)),
            (4, 0) => ar.exit_code = i32::try_from(r.varint()?).unwrap_or(0),
            (5, 2) => ar.stdout = r.bytes()?.to_vec(),
            // 10 and 11 are the deprecated file/directory symlink lists older clients write.
            (10..=12, 2) => ar.output_symlinks.push(decode_output_symlink(r.bytes()?)),
            _ => {
                if !r.skip(wire) {
                    return None;
                }
            }
        }
    }
    Some(ar)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256("abc") — a standard test vector.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn action_result_round_trips() {
        let ar = ActionResult {
            output_files: vec![
                OutputFile {
                    path: "bin/app".into(),
                    digest: "deadbeef".into(),
                    size: 42,
                    executable: true,
                },
                OutputFile {
                    path: "dist/x.js".into(),
                    digest: "cafe".into(),
                    size: 7,
                    executable: false,
                },
            ],
            output_symlinks: vec![OutputSymlink {
                path: "bin/app-latest".into(),
                target: "app".into(),
            }],
            exit_code: 0,
            stdout: b"built ok\n".to_vec(),
        };
        let bytes = encode_action_result(&ar);
        assert_eq!(decode_action_result(&bytes), Some(ar));
    }

    #[test]
    fn encodes_exact_protobuf_bytes() {
        // One output file {path:"a", digest{hash:"ab", size:1}}, stdout "hi".
        // Hand-derived from the protobuf wire spec.
        let ar = ActionResult {
            output_files: vec![OutputFile {
                path: "a".into(),
                digest: "ab".into(),
                size: 1,
                executable: false,
            }],
            output_symlinks: vec![],
            exit_code: 0,
            stdout: b"hi".to_vec(),
        };
        let expected = vec![
            0x12, 0x0B, // ActionResult.output_files, len 11
            0x0A, 0x01, b'a', // OutputFile.path = "a"
            0x12, 0x06, // OutputFile.digest, len 6
            0x0A, 0x02, b'a', b'b', // Digest.hash = "ab"
            0x10, 0x01, // Digest.size_bytes = 1
            0x2A, 0x02, b'h', b'i', // ActionResult.stdout_raw = "hi"
        ];
        assert_eq!(encode_action_result(&ar), expected);
    }

    #[test]
    fn decode_tolerates_unknown_fields() {
        // exit_code (field 4) then an unknown field 9 (varint) — must be skipped.
        let mut bytes = encode_action_result(&ActionResult {
            output_files: vec![],
            output_symlinks: vec![],
            exit_code: 3,
            stdout: vec![],
        });
        put_field_varint(9, 123, &mut bytes); // unknown ExecutedActionMetadata-ish field
        let ar = decode_action_result(&bytes).unwrap();
        assert_eq!(ar.exit_code, 3);
    }
}
