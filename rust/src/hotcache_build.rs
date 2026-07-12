//! Native writer for the `RRHC` catalog hotcache — the build-side mirror of
//! [`crate::hotcache`], emitting the byte layout in `HOTCACHE.md`.
//!
//! Tier 1 (the manifest bundle): given the already-built member files' boot regions,
//! emit one `.rrhc` that inlines the small boots (≤ `inline_threshold`) and references the
//! rest by `(boot_off, boot_len)` in their own data files. The inline-vs-reference choice
//! is the RRTI inline-rare-postings instinct one level up: give away the small boot for
//! free in the single GET, spend a later parallel fetch only on the big boots (the RRVI
//! centroids).
//!
//! Tier 2 (`write_split`: concatenate every member body into one `.rrsplit` with a FOOTER
//! hotcache + trailer, offsets rebased per member) is **deferred**.

use crate::hotcache::MemberTag;
use std::io::{self, Write};

/// `RRHC` magic.
const MAGIC: &[u8; 4] = b"RRHC";
/// Format version written into the header.
const VERSION: u16 = 1;
/// Manifest-entry flag bit marking an inlined member (mirrors the reader's `FLAG_INLINED`).
const FLAG_INLINED: u16 = 1;

/// One member of a composition to write into the manifest. Carries the member's boot
/// bytes so the writer can decide inline-vs-reference, plus the boot region's location in
/// the member's own data file (`boot_off`, `boot_len`) for per-query reads and range-
/// referenced fetches.
pub struct MemberSpec {
    /// The member's format type.
    pub tag: MemberTag,
    /// The data-file name (or URL) the member's per-query reads go to.
    pub data_file: String,
    /// Boot region offset within the data file.
    pub boot_off: u64,
    /// Boot region length in bytes (must equal `boot_bytes.len()`).
    pub boot_len: u32,
    /// The actual boot bytes (so the writer can inline them or measure their size).
    pub boot_bytes: Vec<u8>,
}

/// Writes a Tier-1 `.rrhc` over `members` to `w`. A member whose `boot_len` is
/// `<= inline_threshold` is **inlined** (its flag set and its bytes copied into the
/// inlined-boot blob); the rest are **referenced** by `(boot_off, boot_len)` in their own
/// data files. Emits `[header][manifest entries][string blob][inlined-boot blob]` per
/// `HOTCACHE.md`. All integers little-endian.
pub fn write_hotcache<W: Write>(
    mut w: W,
    members: &[MemberSpec],
    inline_threshold: u32,
) -> io::Result<()> {
    // 1. Lay out the string blob (UTF-8 data-file names) and the inlined-boot blob,
    // recording each member's `(name_off, name_len)` and — when inlined —
    // `(inline_off, inline_len)`. A member is inlined iff its boot fits the threshold.
    let mut string_blob: Vec<u8> = Vec::new();
    let mut inline_blob: Vec<u8> = Vec::new();
    // Per member: (name_off, name_len, inlined, inline_off).
    let mut placements: Vec<(u32, u16, bool, u64)> = Vec::with_capacity(members.len());
    for m in members {
        if m.boot_bytes.len() as u64 != m.boot_len as u64 {
            return Err(io::Error::other(format!(
                "member {:?}: boot_len {} disagrees with boot_bytes.len() {}",
                m.data_file,
                m.boot_len,
                m.boot_bytes.len()
            )));
        }
        let name_bytes = m.data_file.as_bytes();
        let name_off: u32 = string_blob
            .len()
            .try_into()
            .map_err(|_| io::Error::other("RRHC string blob exceeds the 32-bit limit"))?;
        let name_len: u16 = name_bytes
            .len()
            .try_into()
            .map_err(|_| io::Error::other("data-file name exceeds the 16-bit length limit"))?;
        string_blob.extend_from_slice(name_bytes);

        let inlined = m.boot_len <= inline_threshold;
        let inline_off = inline_blob.len() as u64;
        if inlined {
            inline_blob.extend_from_slice(&m.boot_bytes);
        }
        placements.push((name_off, name_len, inlined, inline_off));
    }

    let member_count: u32 = members
        .len()
        .try_into()
        .map_err(|_| io::Error::other("member count exceeds the 32-bit limit"))?;
    let str_bytes: u32 = string_blob
        .len()
        .try_into()
        .map_err(|_| io::Error::other("RRHC string blob exceeds the 32-bit limit"))?;

    // 2. Header (32 B).
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&0u16.to_le_bytes())?; // flags (reserved; bit0 = split-footer in Tier 2)
    w.write_all(&member_count.to_le_bytes())?;
    w.write_all(&str_bytes.to_le_bytes())?;
    w.write_all(&(inline_blob.len() as u64).to_le_bytes())?;
    w.write_all(&[0u8; 8])?; // reserved — pads the header to 32 B

    // 3. Manifest entries (40 B each), in member order.
    for (m, &(name_off, name_len, inlined, inline_off)) in members.iter().zip(&placements) {
        let flags = if inlined { FLAG_INLINED } else { 0 };
        let inline_len = if inlined { m.boot_len } else { 0 };
        let inline_off = if inlined { inline_off } else { 0 };
        w.write_all(&m.tag.to_u16().to_le_bytes())?;
        w.write_all(&flags.to_le_bytes())?;
        w.write_all(&name_off.to_le_bytes())?;
        w.write_all(&name_len.to_le_bytes())?;
        w.write_all(&0u16.to_le_bytes())?; // pad
        w.write_all(&m.boot_off.to_le_bytes())?;
        w.write_all(&m.boot_len.to_le_bytes())?;
        w.write_all(&inline_off.to_le_bytes())?;
        w.write_all(&inline_len.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?; // reserved
    }

    // 4. String blob, then the inlined-boot blob.
    w.write_all(&string_blob)?;
    w.write_all(&inline_blob)?;
    Ok(())
}
