//! Container-level ranged reads into tail postings.
//!
//! A strict-AND query over a rare phrase made of common trigrams is the worst
//! case for the index: the popular "head" yields nothing, so every trigram's
//! full tail posting must be intersected over the whole corpus — and a common
//! trigram's tail is megabytes (a roaring bitmap spanning hundreds of 8 KB
//! containers). Fetching them all is the bulk of a pathological query's egress.
//!
//! This module avoids that. A portable RoaringBitmap is a sorted directory of
//! containers, each keyed by the doc ID's high 16 bits, with an offset table
//! pointing at each container's bytes (the `roaring` crate writes the
//! NO_RUNCONTAINER layout *with* that offset table). So once the candidate set
//! has been narrowed by the rarest postings, the remaining postings need only
//! the containers whose key still appears among the candidates — a few KB, not
//! the whole posting. [`tail_intersect_and`] does exactly that: read the rarest
//! tail in full, then intersect the rest at container granularity.

use crate::fetch::RangeFetch;
use crate::index::{deserialize, read_u16, read_u32, IndexError};
use futures::future::join_all;
use roaring::RoaringBitmap;

/// Portable-format cookie for a bitmap with no run containers. The `roaring`
/// crate emits this followed by `size`, the `(key, card-1)` descriptive header,
/// an offset table (`u32` per container), then the container bodies.
const NO_RUNCONTAINER_COOKIE: u32 = 12346;

/// Bytes read up front to (usually) capture a posting's whole header in one
/// read. Headers larger than this — postings with very many containers — trigger
/// one exact re-read; the body containers are still fetched selectively.
const HEADER_PREFIX: usize = 4096;

/// Needed containers within this many bytes of each other are fetched as one
/// ranged read rather than separately, so a run of consecutive keys collapses to
/// a single request. Bridging a gap wastes at most this many bytes but saves a
/// round-trip — which dominates when the candidate set still spans many keys.
const SPAN_GAP: usize = 16384;

/// One container's location within a posting: its high key, cardinality (needed
/// to re-frame it into a standalone bitmap), and byte range relative to the
/// posting start.
struct Container {
    key: u16,
    card: u32,
    start: usize,
    len: usize,
}

/// Intersects a set of tail postings (`(offset, len)` byte ranges of standalone
/// portable RoaringBitmaps) as a strict AND, fetching only what it must.
///
/// The smallest posting is read in full to seed the candidate set; each
/// remaining posting is then read at container granularity — only the
/// containers whose high key still appears among the survivors. For a rare
/// phrase of common trigrams this turns tens of megabytes into a handful of
/// container reads. Returns an empty bitmap when `tails` is empty or the
/// intersection is empty.
pub(crate) async fn tail_intersect_and<F: RangeFetch>(
    fetch: &F,
    tails: &[(u64, usize)],
) -> Result<RoaringBitmap, IndexError> {
    if tails.is_empty() {
        return Ok(RoaringBitmap::new());
    }
    // Smallest first: the candidate set shrinks fastest, so later (larger)
    // postings touch the fewest containers. Byte size is a cheap cardinality proxy.
    let mut order: Vec<usize> = (0..tails.len()).collect();
    order.sort_by_key(|&i| tails[i].1);

    let (off0, len0) = tails[order[0]];
    let mut running = deserialize(&fetch.read(off0, len0).await?)?;

    for &i in &order[1..] {
        if running.is_empty() {
            break;
        }
        let (off, len) = tails[i];
        let keys = distinct_high_keys(&running);
        running &= read_posting_subset(fetch, off, len, &keys).await?;
    }
    Ok(running)
}

/// The distinct container high keys (`doc >> 16`) present in `bm`, ascending.
/// `bm` iterates ascending, so adjacent dedup yields a sorted, unique list
/// suitable for `binary_search`.
fn distinct_high_keys(bm: &RoaringBitmap) -> Vec<u16> {
    let mut keys = Vec::new();
    let mut last: Option<u16> = None;
    for d in bm.iter() {
        let k = (d >> 16) as u16;
        if last != Some(k) {
            keys.push(k);
            last = Some(k);
        }
    }
    keys
}

/// Reads the posting at `(off, len)` restricted to the containers whose high key
/// is in `keys`, returned as a RoaringBitmap. Falls back to a full read when the
/// posting is small enough that seeking saves nothing, or when its layout is not
/// the seekable NO_RUNCONTAINER-with-offsets variant.
async fn read_posting_subset<F: RangeFetch>(
    fetch: &F,
    off: u64,
    len: usize,
    keys: &[u16],
) -> Result<RoaringBitmap, IndexError> {
    let prefix = fetch.read(off, len.min(HEADER_PREFIX)).await?;
    if prefix.len() >= len {
        // The whole posting is already in hand — nothing to seek.
        return deserialize(&prefix);
    }
    let header = match needed_header_len(&prefix) {
        Some(hl) if hl <= prefix.len() => prefix,
        Some(hl) => fetch.read(off, hl).await?,
        None => return deserialize(&fetch.read(off, len).await?), // not seekable
    };
    let dir = match parse_dir(&header, len) {
        Some(d) => d,
        None => return deserialize(&fetch.read(off, len).await?), // malformed/unexpected layout
    };
    let needed: Vec<&Container> = dir
        .iter()
        .filter(|c| keys.binary_search(&c.key).is_ok())
        .collect();
    if needed.is_empty() {
        return Ok(RoaringBitmap::new());
    }
    // Coalesce the needed containers (already in ascending offset order) into a
    // few byte spans, bridging gaps up to SPAN_GAP, so a run of consecutive keys
    // is one ranged read instead of hundreds. Each span is fetched once; the
    // container bodies are sliced back out for reassembly.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut span_of: Vec<usize> = Vec::with_capacity(needed.len());
    for c in &needed {
        let (cs, ce) = (c.start, c.start + c.len);
        match spans.last_mut() {
            Some(last) if cs <= last.1 + SPAN_GAP => last.1 = ce,
            _ => spans.push((cs, ce)),
        }
        span_of.push(spans.len() - 1);
    }
    let reads = spans
        .iter()
        .map(|&(s, e)| fetch.read(off + s as u64, e - s));
    let datas = join_all(reads).await;
    let mut span_bytes = Vec::with_capacity(spans.len());
    for d in datas {
        span_bytes.push(d?);
    }
    let sel: Vec<(u16, u32, &[u8])> = needed
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (s, _) = spans[span_of[i]];
            let rel = c.start - s;
            (c.key, c.card, &span_bytes[span_of[i]][rel..rel + c.len])
        })
        .collect();
    deserialize(&assemble(&sel))
}

/// The full header length (cookie + size + descriptive header + offset table) of
/// a NO_RUNCONTAINER posting, read from its first 8 bytes, or `None` if the
/// cookie is not that variant.
fn needed_header_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 8 || read_u32(bytes, 0) != NO_RUNCONTAINER_COOKIE {
        return None;
    }
    let size = read_u32(bytes, 4) as usize;
    Some(8 + size * 4 + size * 4)
}

/// Parses a NO_RUNCONTAINER posting's container directory from `header` (which
/// must already span the whole header). `total` is the posting's byte length,
/// used as the final container's end. Returns `None` if the offset table is
/// absent or inconsistent, so the caller can fall back to a full read.
fn parse_dir(header: &[u8], total: usize) -> Option<Vec<Container>> {
    if header.len() < 8 || read_u32(header, 0) != NO_RUNCONTAINER_COOKIE {
        return None;
    }
    let size = read_u32(header, 4) as usize;
    if size == 0 {
        return Some(Vec::new());
    }
    let desc = 8;
    let offs = desc + size * 4;
    let data = offs + size * 4;
    if header.len() < data || data > total {
        return None;
    }
    // The first container must begin right after the offset table; if not, this
    // is not the offset-table layout we can seek into.
    if read_u32(header, offs) as usize != data {
        return None;
    }
    let mut out = Vec::with_capacity(size);
    for i in 0..size {
        let key = read_u16(header, desc + i * 4);
        let card = read_u16(header, desc + i * 4 + 2) as u32 + 1;
        let start = read_u32(header, offs + i * 4) as usize;
        let end = if i + 1 < size {
            read_u32(header, offs + (i + 1) * 4) as usize
        } else {
            total
        };
        if end < start || end > total {
            return None;
        }
        out.push(Container {
            key,
            card,
            start,
            len: end - start,
        });
    }
    Some(out)
}

/// Re-frames a selection of containers into a standalone portable RoaringBitmap,
/// byte-for-byte the NO_RUNCONTAINER layout the `roaring` crate deserializes.
/// `sel` is `(key, cardinality, body)` in ascending key order, as the format
/// requires.
fn assemble(sel: &[(u16, u32, &[u8])]) -> Vec<u8> {
    let n = sel.len();
    let data_start = 8 + n * 4 + n * 4;
    let total: usize = sel.iter().map(|(_, _, b)| b.len()).sum();
    let mut blob = Vec::with_capacity(data_start + total);
    blob.extend_from_slice(&NO_RUNCONTAINER_COOKIE.to_le_bytes());
    blob.extend_from_slice(&(n as u32).to_le_bytes());
    for &(key, card, _) in sel {
        blob.extend_from_slice(&key.to_le_bytes());
        blob.extend_from_slice(&((card - 1) as u16).to_le_bytes());
    }
    let mut pos = data_start;
    for &(_, _, body) in sel {
        blob.extend_from_slice(&(pos as u32).to_le_bytes());
        pos += body.len();
    }
    for &(_, _, body) in sel {
        blob.extend_from_slice(body);
    }
    blob
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryFetch;
    use futures::executor::block_on;

    fn ser(bm: &RoaringBitmap) -> Vec<u8> {
        let mut v = Vec::new();
        bm.serialize_into(&mut v).unwrap();
        v
    }

    /// The container-level AND must equal a full deserialize-everything AND,
    /// across array and bitmap containers and regardless of which posting is
    /// read in full first.
    #[test]
    fn ranged_and_matches_full_and() {
        let mut a = RoaringBitmap::new();
        let mut b = RoaringBitmap::new();
        let mut c = RoaringBitmap::new();
        for d in [70000u32, 70001, 130000, 200005, 5_000_000] {
            a.insert(d);
        }
        for d in [70001u32, 130000, 200005, 200006, 9_000_000] {
            b.insert(d);
        }
        for d in [70001u32, 130000, 200005, 3_000_000] {
            c.insert(d);
        }
        // Key 4: a dense (bitmap) container shared by all three.
        for d in 300000..306000u32 {
            a.insert(d);
            b.insert(d);
            c.insert(d);
        }
        let want = {
            let mut x = a.clone();
            x &= &b;
            x &= &c;
            x
        };

        let (sa, sb, sc) = (ser(&a), ser(&b), ser(&c));
        let mut buf = Vec::new();
        let oa = buf.len() as u64;
        buf.extend_from_slice(&sa);
        let ob = buf.len() as u64;
        buf.extend_from_slice(&sb);
        let oc = buf.len() as u64;
        buf.extend_from_slice(&sc);
        let fetch = MemoryFetch::new(buf);

        let tails = vec![(oa, sa.len()), (ob, sb.len()), (oc, sc.len())];
        let got = block_on(tail_intersect_and(&fetch, &tails)).unwrap();
        assert_eq!(got, want);
    }

    /// The directory must report sorted keys, a first container starting right
    /// after the offset table, and a last container ending at the posting end.
    #[test]
    fn parse_dir_reads_offset_directory() {
        let mut a = RoaringBitmap::new();
        for d in [70000u32, 200005, 5_000_000] {
            a.insert(d);
        }
        for d in 300000..306000u32 {
            a.insert(d);
        }
        let s = ser(&a);
        let dir = parse_dir(&s, s.len()).expect("seekable layout");
        assert!(dir.windows(2).all(|w| w[0].key < w[1].key));
        assert_eq!(dir[0].start, 8 + 2 * dir.len() * 4);
        let last = dir.last().unwrap();
        assert_eq!(last.start + last.len, s.len());
    }

    #[test]
    fn empty_tail_set_is_empty() {
        let fetch = MemoryFetch::new(Vec::new());
        assert!(block_on(tail_intersect_and(&fetch, &[]))
            .unwrap()
            .is_empty());
    }
}
