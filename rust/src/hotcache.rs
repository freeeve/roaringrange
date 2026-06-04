//! The `RRHC` catalog-hotcache reader — a cross-format **boot accelerator**.
//!
//! Not another index, but a small artifact that front-loads the boot regions of a
//! whole *composition* (trigram `RRS` + term `RRTI` + facet `RRSF` + vector `RRVI` +
//! record `RRSR` + lookup `RRIL` + embedder `RRM2`) so booting the composition costs
//! **one** ranged read instead of N separate cold opens. It replaces nothing: the
//! per-query data files are untouched and per-query reads are unchanged. See
//! `HOTCACHE.md` for the frozen byte layout and `tasks/006_catalog_hotcache.md` for
//! the design.
//!
//! Layout (`HOTCACHE.md`): `[header][manifest entries][string blob][inlined-boot blob]`.
//! Each manifest entry names one member: its type tag, its data-file name (resolved
//! against the string blob), its boot byte-range **within that data file**, and an
//! inlined-here-vs-fetch-by-range flag. The small boots (headers, sparse indexes, FSTs,
//! facet tables, record offsets, `.dict`, lookup map) are copied into the inlined-boot
//! blob and come back free with the single GET; the few large boots (the RRVI centroids)
//! are referenced by `(bootOff, bootLen)` and fetched from the member's own data file in a
//! later parallel wave.
//!
//! [`Hotcache::open`] does **one** ranged read of the whole `.rrhc` and parses it
//! resident. It is the only fetch the hotcache itself ever issues; range-referenced large
//! boots are fetched by the caller (a future `Catalog::open_hotcache`) from each member's
//! data file, not from the `.rrhc`.

use crate::fetch::RangeFetch;
use crate::index::{read_u16, read_u32, read_u64, IndexError};

/// `RRHC` magic.
const MAGIC: &[u8; 4] = b"RRHC";
/// Header size in bytes: magic[4] + version[2] + flags[2] + memberCount[4] +
/// strBytes[4] + inlineBytes[8] + reserved[8]. Kept in sync with the builder.
const HEADER_SIZE: usize = 32;
/// Manifest entry size in bytes: tag[2] + flags[2] + nameOff[4] + nameLen[2] + pad[2] +
/// bootOff[8] + bootLen[4] + inlineOff[8] + inlineLen[4] + reserved[4]. Kept in sync
/// with the builder.
const ENTRY_SIZE: usize = 40;
/// Format version written into / accepted from the header.
const VERSION: u16 = 1;
/// Manifest-entry flag bit marking a member whose boot bytes are inlined in the `.rrhc`
/// (else the boot is fetched by range from the member's data file).
const FLAG_INLINED: u16 = 1;

/// The type tag of a hotcache member — which roaringrange format the member is. Matches
/// the `RRHC` v1 tag numbers (`HOTCACHE.md` §manifest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberTag {
    /// Trigram index (`RRS` / `RRSI`).
    Rrs,
    /// Term-level inverted index (`RRTI`).
    Rrti,
    /// Facet sidecar (`RRSF`).
    Rrsf,
    /// Vector similarity index (`RRVI`).
    Rrvi,
    /// Record store offset index (`RRSR` `.idx`).
    RrsrIdx,
    /// Record store value blob (`RRSR` `.bin`).
    RrsrBin,
    /// Record store shared zstd dictionary (`RRSR` `.dict`).
    RrsrDict,
    /// Doc-ID lookup map (`RRIL`).
    Rril,
    /// In-browser model2vec embedder (`RRM2`).
    Rrm2,
}

impl MemberTag {
    /// Maps the on-disk `u16` tag to a [`MemberTag`], or `None` for an unknown tag.
    fn from_u16(tag: u16) -> Option<Self> {
        match tag {
            1 => Some(MemberTag::Rrs),
            2 => Some(MemberTag::Rrti),
            3 => Some(MemberTag::Rrsf),
            4 => Some(MemberTag::Rrvi),
            5 => Some(MemberTag::RrsrIdx),
            6 => Some(MemberTag::RrsrBin),
            7 => Some(MemberTag::RrsrDict),
            8 => Some(MemberTag::Rril),
            9 => Some(MemberTag::Rrm2),
            _ => None,
        }
    }

    /// The on-disk `u16` tag for this member type — the builder's encoding.
    pub fn to_u16(self) -> u16 {
        match self {
            MemberTag::Rrs => 1,
            MemberTag::Rrti => 2,
            MemberTag::Rrsf => 3,
            MemberTag::Rrvi => 4,
            MemberTag::RrsrIdx => 5,
            MemberTag::RrsrBin => 6,
            MemberTag::RrsrDict => 7,
            MemberTag::Rril => 8,
            MemberTag::Rrm2 => 9,
        }
    }
}

/// One member of a composition, as named by the manifest. Records the member's type, the
/// data file its per-query reads still go to, and where its boot region lives — both
/// within the data file (`boot_off`, `boot_len`, used as the per-query base and for a
/// range-referenced fetch) and, if inlined, within the `.rrhc`'s inlined-boot blob.
#[derive(Debug, Clone)]
pub struct Member {
    /// The member's format type.
    pub tag: MemberTag,
    /// The data-file name (or URL) the member's per-query reads go to.
    pub data_file: String,
    /// Boot region offset **within the data file** (per-query base / range-fetch start).
    pub boot_off: u64,
    /// Boot region length in bytes.
    pub boot_len: u32,
    /// Whether the boot bytes are inlined in the `.rrhc` (else fetch by range from the
    /// data file at `(boot_off, boot_len)`).
    pub inlined: bool,
    /// If inlined, the boot bytes' offset into the inlined-boot blob.
    inline_off: u64,
    /// If inlined, the boot bytes' length in the inlined-boot blob (always `== boot_len`).
    inline_len: u32,
}

/// A parsed `RRHC` manifest. Holds the members and the resident inlined-boot blob in
/// memory; the whole artifact was read with one ranged read by [`Hotcache::open`].
#[derive(Debug, Clone)]
pub struct Hotcache {
    /// Members of the composition, in manifest order.
    members: Vec<Member>,
    /// The concatenated inlined boot bodies; sliced per member by `(inline_off, inline_len)`.
    inline_blob: Vec<u8>,
}

impl Hotcache {
    /// Boots from one GET of the `.rrhc`: header + manifest + string blob + inlined-boot
    /// blob, all resident immediately. The only fetch the hotcache issues — range-
    /// referenced large boots are fetched by the caller from each member's data file.
    pub async fn open<F: RangeFetch>(rrhc: F) -> Result<Hotcache, IndexError> {
        // One ranged read of the header pins the artifact's section sizes; a second
        // ranged read of exactly the remaining bytes (manifest + strings + inline blob)
        // makes the whole `.rrhc` resident.
        let header = rrhc.read(0, HEADER_SIZE).await?;
        if header.len() < HEADER_SIZE {
            return Err(IndexError::Malformed("short RRHC header"));
        }
        if &header[0..4] != MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&header[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = read_u16(&header, 4);
        if version != VERSION {
            return Err(IndexError::BadVersion(version));
        }
        let member_count = read_u32(&header, 8) as usize;
        let str_bytes = read_u32(&header, 12) as usize;
        let inline_bytes = read_u64(&header, 16) as usize;

        // The body holds the manifest, the string blob, then the inlined-boot blob.
        let manifest_bytes = member_count
            .checked_mul(ENTRY_SIZE)
            .ok_or(IndexError::Malformed("RRHC manifest size overflow"))?;
        let body_len = manifest_bytes
            .checked_add(str_bytes)
            .and_then(|n| n.checked_add(inline_bytes))
            .ok_or(IndexError::Malformed("RRHC body size overflow"))?;
        let body = rrhc.read(HEADER_SIZE as u64, body_len).await?;
        if body.len() < body_len {
            return Err(IndexError::Malformed("short RRHC body"));
        }

        // Section offsets within `body` (which starts at file offset HEADER_SIZE).
        let str_start = manifest_bytes;
        let inline_start = str_start + str_bytes;
        let string_blob = &body[str_start..str_start + str_bytes];
        let inline_blob = body[inline_start..inline_start + inline_bytes].to_vec();

        let mut members = Vec::with_capacity(member_count);
        for i in 0..member_count {
            let base = i * ENTRY_SIZE;
            let tag = MemberTag::from_u16(read_u16(&body, base))
                .ok_or(IndexError::Malformed("RRHC unknown member tag"))?;
            let flags = read_u16(&body, base + 2);
            let name_off = read_u32(&body, base + 4) as usize;
            let name_len = read_u16(&body, base + 8) as usize;
            // base + 10..12 is `pad` (reserved 0).
            let boot_off = read_u64(&body, base + 12);
            let boot_len = read_u32(&body, base + 20);
            let inline_off = read_u64(&body, base + 24);
            let inline_len = read_u32(&body, base + 32);
            // base + 36..40 is `reserved` (0).

            let name_end = name_off
                .checked_add(name_len)
                .ok_or(IndexError::Malformed("RRHC name range overflow"))?;
            if name_end > string_blob.len() {
                return Err(IndexError::Malformed("RRHC name out of string blob"));
            }
            let data_file = String::from_utf8(string_blob[name_off..name_end].to_vec())
                .map_err(|_| IndexError::Malformed("RRHC non-UTF-8 data-file name"))?;

            let inlined = flags & FLAG_INLINED != 0;
            if inlined {
                let end = inline_off
                    .checked_add(inline_len as u64)
                    .ok_or(IndexError::Malformed("RRHC inline range overflow"))?;
                if end > inline_blob.len() as u64 {
                    return Err(IndexError::Malformed("RRHC inline out of inlined blob"));
                }
            }
            members.push(Member {
                tag,
                data_file,
                boot_off,
                boot_len,
                inlined,
                inline_off,
                inline_len,
            });
        }

        Ok(Hotcache {
            members,
            inline_blob,
        })
    }

    /// The members present, in manifest order.
    pub fn members(&self) -> &[Member] {
        &self.members
    }

    /// The resident boot bytes for `m` if it was inlined, else `None` — meaning the
    /// caller must fetch the boot by range from `m`'s data file at `(m.boot_off,
    /// m.boot_len)`. The returned slice is exactly `m.boot_len` bytes.
    pub fn inlined(&self, m: &Member) -> Option<&[u8]> {
        if !m.inlined {
            return None;
        }
        let start = m.inline_off as usize;
        let end = start + m.inline_len as usize;
        Some(&self.inline_blob[start..end])
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::fetch::MemoryFetch;
    use crate::hotcache_build::{write_hotcache, MemberSpec};
    use futures::executor::block_on;

    /// Builds an in-memory `.rrhc` over `specs` at `inline_threshold` and opens it.
    fn build(specs: &[MemberSpec], inline_threshold: u32) -> Hotcache {
        let mut buf = Vec::new();
        write_hotcache(&mut buf, specs, inline_threshold).unwrap();
        block_on(Hotcache::open(MemoryFetch::new(buf))).unwrap()
    }

    #[test]
    fn round_trips_mixed_inline_and_reference() {
        // Three small members (inlined) plus one large member (referenced by range).
        let rrs_boot = vec![1u8, 2, 3, 4, 5];
        let rrti_boot = vec![9u8; 64];
        let dict_boot = vec![7u8; 12];
        let rrvi_boot = vec![0xABu8; 4096]; // exceeds the 1 KB threshold -> referenced.

        let specs = vec![
            MemberSpec {
                tag: MemberTag::Rrs,
                data_file: "corpus.rrs".to_string(),
                boot_off: 0,
                boot_len: rrs_boot.len() as u32,
                boot_bytes: rrs_boot.clone(),
            },
            MemberSpec {
                tag: MemberTag::Rrti,
                data_file: "corpus.rrt".to_string(),
                boot_off: 0,
                boot_len: rrti_boot.len() as u32,
                boot_bytes: rrti_boot.clone(),
            },
            MemberSpec {
                tag: MemberTag::RrsrDict,
                data_file: "records.dict".to_string(),
                boot_off: 0,
                boot_len: dict_boot.len() as u32,
                boot_bytes: dict_boot.clone(),
            },
            MemberSpec {
                tag: MemberTag::Rrvi,
                data_file: "vectors.rrvi".to_string(),
                // A non-zero boot offset within the data file is preserved verbatim.
                boot_off: 48,
                boot_len: rrvi_boot.len() as u32,
                boot_bytes: rrvi_boot.clone(),
            },
        ];

        let hc = build(&specs, 1024);
        let members = hc.members();
        assert_eq!(members.len(), 4);

        // Tags and data-file names survive the round trip in manifest order.
        let tags: Vec<MemberTag> = members.iter().map(|m| m.tag).collect();
        assert_eq!(
            tags,
            vec![
                MemberTag::Rrs,
                MemberTag::Rrti,
                MemberTag::RrsrDict,
                MemberTag::Rrvi
            ]
        );
        let names: Vec<&str> = members.iter().map(|m| m.data_file.as_str()).collect();
        assert_eq!(
            names,
            vec!["corpus.rrs", "corpus.rrt", "records.dict", "vectors.rrvi"]
        );

        // The three small members are inlined; their resident bytes match the input.
        assert!(members[0].inlined);
        assert_eq!(hc.inlined(&members[0]), Some(rrs_boot.as_slice()));
        assert!(members[1].inlined);
        assert_eq!(hc.inlined(&members[1]), Some(rrti_boot.as_slice()));
        assert!(members[2].inlined);
        assert_eq!(hc.inlined(&members[2]), Some(dict_boot.as_slice()));

        // The large member is referenced by range, not inlined: inlined() is None and the
        // manifest carries the (boot_off, boot_len) to fetch from its data file.
        let rrvi = &members[3];
        assert!(!rrvi.inlined);
        assert_eq!(hc.inlined(rrvi), None);
        assert_eq!(rrvi.boot_off, 48);
        assert_eq!(rrvi.boot_len, rrvi_boot.len() as u32);
    }

    #[test]
    fn threshold_boundary_inlines_equal_size() {
        // A boot exactly at the threshold is inlined (<=); one byte over is referenced.
        let specs = vec![
            MemberSpec {
                tag: MemberTag::Rril,
                data_file: "ids.rril".to_string(),
                boot_off: 0,
                boot_len: 16,
                boot_bytes: vec![3u8; 16],
            },
            MemberSpec {
                tag: MemberTag::RrsrBin,
                data_file: "records.bin".to_string(),
                boot_off: 0,
                boot_len: 17,
                boot_bytes: vec![4u8; 17],
            },
        ];
        let hc = build(&specs, 16);
        assert!(hc.members()[0].inlined);
        assert_eq!(hc.inlined(&hc.members()[0]), Some(vec![3u8; 16].as_slice()));
        assert!(!hc.members()[1].inlined);
        assert_eq!(hc.inlined(&hc.members()[1]), None);
    }

    #[test]
    fn empty_manifest_round_trips() {
        let hc = build(&[], 1024);
        assert!(hc.members().is_empty());
    }

    #[test]
    fn rejects_bad_magic() {
        let bogus = MemoryFetch::new(vec![0u8; HEADER_SIZE]);
        assert!(matches!(
            block_on(Hotcache::open(bogus)),
            Err(IndexError::BadMagic(_))
        ));
    }

    #[test]
    fn member_tag_round_trips_through_u16() {
        for tag in [
            MemberTag::Rrs,
            MemberTag::Rrti,
            MemberTag::Rrsf,
            MemberTag::Rrvi,
            MemberTag::RrsrIdx,
            MemberTag::RrsrBin,
            MemberTag::RrsrDict,
            MemberTag::Rril,
            MemberTag::Rrm2,
        ] {
            assert_eq!(MemberTag::from_u16(tag.to_u16()), Some(tag));
        }
        assert_eq!(MemberTag::from_u16(0), None);
        assert_eq!(MemberTag::from_u16(10), None);
    }

    #[test]
    fn inlined_member_preserves_nonzero_boot_off() {
        // An inlined boot can still sit at a non-zero offset within its data file (a boot
        // region not starting at byte 0); the manifest preserves boot_off verbatim,
        // independent of whether the bytes were inlined.
        let specs = vec![MemberSpec {
            tag: MemberTag::Rrsf,
            data_file: "corpus.rrf".to_string(),
            boot_off: 12_345,
            boot_len: 8,
            boot_bytes: vec![5u8; 8],
        }];
        let hc = build(&specs, 1024);
        let m = &hc.members()[0];
        assert!(m.inlined);
        assert_eq!(m.boot_off, 12_345);
        assert_eq!(hc.inlined(m), Some(vec![5u8; 8].as_slice()));
    }

    #[test]
    fn malformed_inputs_error_without_panic() {
        // A valid `.rrhc` truncated past its declared sizes must error, not read out of
        // bounds: the body read demands more bytes than the buffer holds.
        let specs = vec![MemberSpec {
            tag: MemberTag::Rrs,
            data_file: "a.rrs".to_string(),
            boot_off: 0,
            boot_len: 4,
            boot_bytes: vec![1u8; 4],
        }];
        let mut buf = Vec::new();
        write_hotcache(&mut buf, &specs, 1024).unwrap();
        let truncated = buf[..buf.len() - 2].to_vec();
        assert!(block_on(Hotcache::open(MemoryFetch::new(truncated))).is_err());

        // A header whose memberCount the buffer cannot satisfy errors on the short body
        // rather than indexing past the end.
        let mut hdr = buf[..HEADER_SIZE].to_vec();
        hdr[8..12].copy_from_slice(&1000u32.to_le_bytes());
        assert!(block_on(Hotcache::open(MemoryFetch::new(hdr))).is_err());
    }
}
