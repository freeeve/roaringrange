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
pub(crate) const HEADER_PREFIX: usize = 4096;

/// Needed containers within this many bytes of each other are fetched as one
/// ranged read rather than separately, so a run of consecutive keys collapses to
/// a single request. Bridging a gap wastes at most this many bytes but saves a
/// round-trip — which dominates when the candidate set still spans many keys.
const SPAN_GAP: usize = 16384;

/// One container's location within a posting: its high key, cardinality (needed
/// to re-frame it into a standalone bitmap), and byte range relative to the
/// posting start.
#[derive(Clone)]
pub(crate) struct Container {
    pub(crate) key: u16,
    pub(crate) card: u32,
    pub(crate) start: usize,
    pub(crate) len: usize,
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

/// The exact cardinality of the posting at `(off, len)` without fetching its
/// body: roaring's NO_RUNCONTAINER descriptive header stores each container's
/// cardinality (`[key u16][card-1 u16]` per container), so the count costs the
/// 8-byte cookie plus `4·containers` bytes — KBs for a posting of any size. A
/// small posting or a run-container layout (whose header has no fixed-stride
/// cardinalities) falls back to reading and deserializing the whole posting.
pub(crate) async fn posting_cardinality<F: RangeFetch>(
    fetch: &F,
    off: u64,
    len: usize,
) -> Result<u64, IndexError> {
    let prefix = fetch.read(off, len.min(HEADER_PREFIX)).await?;
    if prefix.len() >= len {
        return Ok(deserialize(&prefix)?.len());
    }
    // prefix.len() == HEADER_PREFIX (4 KB) here, so the cookie is in hand.
    if read_u32(&prefix, 0) != NO_RUNCONTAINER_COOKIE {
        return Ok(deserialize(&fetch.read(off, len).await?)?.len());
    }
    let size = read_u32(&prefix, 4) as usize;
    let desc_len = match size.checked_mul(4).and_then(|n| n.checked_add(8)) {
        Some(d) if d <= len => d,
        _ => {
            return Err(IndexError::Malformed(
                "posting descriptive header out of range",
            ))
        }
    };
    let desc = if desc_len <= prefix.len() {
        prefix
    } else {
        fetch.read(off, desc_len).await?
    };
    Ok((0..size)
        .map(|i| read_u16(&desc, 8 + i * 4 + 2) as u64 + 1)
        .sum())
}

/// Reads the posting at `(off, len)` restricted to the containers whose high key
/// is in `keys`, returned as a RoaringBitmap. Falls back to a full read when the
/// posting is small enough that seeking saves nothing, or when its layout is not
/// the seekable NO_RUNCONTAINER-with-offsets variant.
pub(crate) async fn read_posting_subset<F: RangeFetch>(
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
    fetch_containers(fetch, off, &needed).await
}

/// Fetches the first `eager_buckets` container buckets (keys `0..eager_buckets`, i.e. docs
/// `[0, eager_buckets·65536)`) of the posting at `(off, len)` — the v3 eager-prefix read that
/// replaces the v2 directly-addressed head blob. Reuses the container-seek path; a small or
/// non-seekable posting is read whole and then trimmed to the prefix.
pub(crate) async fn fetch_head_prefix<F: RangeFetch>(
    fetch: &F,
    off: u64,
    len: usize,
    eager_buckets: u16,
) -> Result<RoaringBitmap, IndexError> {
    let keys: Vec<u16> = (0..eager_buckets).collect();
    let mut bm = read_posting_subset(fetch, off, len, &keys).await?;
    // read_posting_subset returns the whole posting on the small/non-seekable fallback, so trim
    // any docs at or above the prefix to keep the head exact.
    bm.remove_range((eager_buckets as u32 * 65_536)..);
    Ok(bm)
}

/// Fetches a posting's `needed` containers (in ascending offset order) and
/// reassembles them into a standalone bitmap. The containers are coalesced into a
/// few byte spans, bridging gaps up to SPAN_GAP, so a run of consecutive keys is
/// one ranged read instead of hundreds; each span is fetched once and the bodies
/// sliced back out. Empty when nothing is needed. Shared by the whole-tail seek
/// ([`read_posting_subset`]) and the key-windowed scan ([`read_dir_subset`]).
async fn fetch_containers<F: RangeFetch>(
    fetch: &F,
    off: u64,
    needed: &[&Container],
) -> Result<RoaringBitmap, IndexError> {
    if needed.is_empty() {
        return Ok(RoaringBitmap::new());
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut span_of: Vec<usize> = Vec::with_capacity(needed.len());
    for c in needed {
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
pub(crate) fn needed_header_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 8 || read_u32(bytes, 0) != NO_RUNCONTAINER_COOKIE {
        return None;
    }
    // Checked: size is corruption-controlled and size*8 overflows 32-bit usize
    // on wasm32; None falls back to the full-posting read path.
    let size = read_u32(bytes, 4) as usize;
    size.checked_mul(8).and_then(|n| n.checked_add(8))
}

/// Parses a NO_RUNCONTAINER posting's container directory from `header` (which
/// must already span the whole header). `total` is the posting's byte length,
/// used as the final container's end. Returns `None` if the offset table is
/// absent or inconsistent, so the caller can fall back to a full read.
pub(crate) fn parse_dir(header: &[u8], total: usize) -> Option<Vec<Container>> {
    if header.len() < 8 || read_u32(header, 0) != NO_RUNCONTAINER_COOKIE {
        return None;
    }
    let size = read_u32(header, 4) as usize;
    if size == 0 {
        return Some(Vec::new());
    }
    // Checked: a corruption-controlled size wraps these sums on wasm32, which
    // would defeat the bounds checks below and index past the header buffer.
    let desc = 8usize;
    let offs = size.checked_mul(4).and_then(|n| n.checked_add(desc))?;
    let data = size.checked_mul(4).and_then(|n| n.checked_add(offs))?;
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
pub(crate) fn assemble(sel: &[(u16, u32, &[u8])]) -> Vec<u8> {
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

/// One tail posting prepared for incremental, key-ordered intersection: its file
/// offset and parsed container directory (read once, so paging the tail re-reads
/// no headers). Containers are keyed by `doc_id >> 16`, ascending.
struct TailDir {
    off: u64,
    dir: Vec<Container>,
}

/// Reads each tail posting's container directory — one header read each (KB), not
/// the whole posting. Returns `None` if any posting isn't the seekable
/// NO_RUNCONTAINER-with-offsets layout, so the caller can fall back to a full load.
async fn open_tail_dirs<F: RangeFetch>(
    fetch: &F,
    tails: &[(u64, usize)],
) -> Result<Option<Vec<TailDir>>, IndexError> {
    // Wave 1: every posting's header prefix concurrently — the reads are
    // independent, so a strict AND (or a facet filter) with many tails pays one
    // round trip rather than one per posting.
    let prefixes = join_all(
        tails
            .iter()
            .map(|&(off, len)| fetch.read(off, len.min(HEADER_PREFIX))),
    )
    .await;
    let mut headers: Vec<Vec<u8>> = Vec::with_capacity(tails.len());
    for p in prefixes {
        headers.push(p?);
    }
    // A posting with more containers than the prefix captured needs one exact
    // re-read; batch those (rare) into a single second wave.
    let mut refetch: Vec<(usize, usize)> = Vec::new(); // (index, exact header len)
    for (i, header) in headers.iter().enumerate() {
        match needed_header_len(header) {
            Some(hl) if hl <= header.len() => {}
            Some(hl) => refetch.push((i, hl)),
            None => return Ok(None), // not the seekable layout — fall back to a full load
        }
    }
    if !refetch.is_empty() {
        let reads = join_all(refetch.iter().map(|&(i, hl)| fetch.read(tails[i].0, hl))).await;
        for (&(i, _), r) in refetch.iter().zip(reads) {
            headers[i] = r?;
        }
    }
    // Parse every directory (no fetches).
    let mut out = Vec::with_capacity(tails.len());
    for (&(off, len), header) in tails.iter().zip(&headers) {
        match parse_dir(header, len) {
            Some(dir) => out.push(TailDir { off, dir }),
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

/// The container keys present in EVERY posting (ascending) — the only buckets a
/// strict AND can populate. Computed from the cached directories (no fetches):
/// start from the smallest directory's keys and retain those present in all others.
fn candidate_keys(dirs: &[TailDir]) -> Vec<u16> {
    if dirs.is_empty() {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..dirs.len()).collect();
    order.sort_by_key(|&i| dirs[i].dir.len());
    let mut keys: Vec<u16> = dirs[order[0]].dir.iter().map(|c| c.key).collect();
    for &i in &order[1..] {
        if keys.is_empty() {
            break;
        }
        let other = &dirs[i].dir;
        keys.retain(|k| other.binary_search_by_key(k, |c| c.key).is_ok());
    }
    keys
}

/// Like [`read_posting_subset`] but over a cached directory (no header re-read):
/// the containers whose key is in `keys`, assembled into a bitmap.
async fn read_dir_subset<F: RangeFetch>(
    fetch: &F,
    d: &TailDir,
    keys: &[u16],
) -> Result<RoaringBitmap, IndexError> {
    let needed: Vec<&Container> = d
        .dir
        .iter()
        .filter(|c| keys.binary_search(&c.key).is_ok())
        .collect();
    fetch_containers(fetch, d.off, &needed).await
}

/// A posting's exact byte cost within a key window — the sum of its container lengths at
/// the window's keys, free to compute from the cached directory. Drives the seed choice
/// (and the decision to seed at all) in [`intersect_key_window`].
fn window_cost(d: &TailDir, window: &[u16]) -> u64 {
    d.dir
        .iter()
        .filter(|c| window.binary_search(&c.key).is_ok())
        .map(|c| c.len as u64)
        .sum()
}

/// A window seed only pays for its extra dependent round trip when some other posting is
/// substantially denser here — then every key the sparse seed rules out skips that
/// posting's ~8 KB containers. Below this ratio the postings are similar-density (the
/// `year + common term` shape, where shrink can't skip much) and the single parallel wave
/// wins on latency.
const SEED_COST_RATIO: u64 = 4;

/// Strict-AND intersect the cached tail directories over only the container keys
/// in `window`, reading just those container bodies. Surviving docs, ascending.
///
/// When one posting's window bytes are much smaller than another's (a selective trigram
/// among common ones), it is read first as a **seed** and the rest are fetched only at
/// the container keys that survive it — the windowed form of [`tail_intersect_and`]'s
/// smallest-first shrink, trading one extra round trip for skipping the dense postings'
/// containers in every bucket the seed already rules out. Similar-density postings keep
/// the single concurrent wave.
async fn intersect_key_window<F: RangeFetch>(
    fetch: &F,
    dirs: &[TailDir],
    window: &[u16],
) -> Result<RoaringBitmap, IndexError> {
    if dirs.is_empty() || window.is_empty() {
        return Ok(RoaringBitmap::new());
    }
    let mut seeded: Option<(usize, RoaringBitmap)> = None;
    let mut surviving: Vec<u16> = Vec::new();
    if dirs.len() > 1 {
        let costs: Vec<u64> = dirs.iter().map(|d| window_cost(d, window)).collect();
        let seed = (0..dirs.len()).min_by_key(|&i| costs[i]).unwrap_or(0);
        let densest = costs.iter().copied().max().unwrap_or(0);
        if costs[seed].saturating_mul(SEED_COST_RATIO) <= densest {
            let acc = read_dir_subset(fetch, &dirs[seed], window).await?;
            if acc.is_empty() {
                return Ok(RoaringBitmap::new()); // the seed alone kills the AND
            }
            surviving = distinct_high_keys(&acc);
            seeded = Some((seed, acc));
        }
    }

    let keys = if seeded.is_some() { &surviving } else { window };
    let reads = dirs
        .iter()
        .enumerate()
        .filter(|(i, _)| seeded.as_ref().map(|(s, _)| s) != Some(i))
        .map(|(_, d)| read_dir_subset(fetch, d, keys));
    let results = join_all(reads).await;
    let mut bms = Vec::with_capacity(dirs.len());
    for r in results {
        let b = r?;
        if b.is_empty() {
            return Ok(RoaringBitmap::new()); // a missing posting kills the AND
        }
        bms.push(b);
    }
    if let Some((_, acc)) = &seeded {
        bms.push(acc.clone());
    }
    bms.sort_by_key(|b| b.len());
    let mut iter = bms.into_iter();
    let mut acc = iter.next().unwrap();
    for b in iter {
        acc &= b;
        if acc.is_empty() {
            break;
        }
    }
    Ok(acc)
}

/// The union of container keys across a facet field's category postings
/// (ascending, deduped) — the buckets where that field's category-OR could
/// contribute a doc.
fn union_keys(field: &[TailDir]) -> Vec<u16> {
    let mut keys: Vec<u16> = field
        .iter()
        .flat_map(|d| d.dir.iter().map(|c| c.key))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Incremental, doc-ID-ordered tail intersection for the strict-AND path,
/// optionally ANDed with a facet filter (within-field OR, across-field AND). Holds
/// every posting's cached directory plus the candidate keys (buckets present in all
/// trigrams and, per facet field, in at least one selected category);
/// [`TailScan::next_window`] intersects the next slice of buckets, letting a cursor
/// page the tail in rank order while fetching only the containers each page spans —
/// never the whole (possibly hundreds-of-MB) tail.
///
/// Draining every window yields exactly the strict-AND result of
/// [`tail_intersect_and`] (ANDed with the facet filter), in the same ascending order.
pub(crate) struct TailScan {
    trigrams: Vec<TailDir>,
    /// Per facet field, the selected categories' tail directories (ORed within a
    /// field, ANDed across fields). Empty when the query is unfiltered.
    facets: Vec<Vec<TailDir>>,
    /// Excluded categories' tail directories, unioned and subtracted from every
    /// window: a doc surviving the include-AND is dropped if it matches ANY of
    /// these (`result = includeAnd ANDNOT (OR of excludes)`). Empty when the
    /// filter has no negated categories. Excludes never prune the candidate keys
    /// — an excluded doc removes docs from a bucket, not the whole bucket.
    excludes: Vec<TailDir>,
    keys: Vec<u16>,
    pos: usize,
}

impl TailScan {
    /// Opens a scan over the trigram `tails`, the optional `facet_fields` (each a
    /// field's selected-category `(tail_off, tail_size)` list) to AND in, and the
    /// optional `exclude_ranges` (a flat list of negated categories' tails) to
    /// subtract, reading every posting's directory once. Returns `None` if any
    /// posting isn't seekable, so the caller falls back to the whole-tail
    /// intersection (which applies the same includes and excludes).
    pub(crate) async fn open<F: RangeFetch>(
        fetch: &F,
        tails: &[(u64, usize)],
        facet_fetch: Option<&F>,
        facet_fields: &[Vec<(u64, usize)>],
        exclude_ranges: &[(u64, usize)],
        min_key: u16,
    ) -> Result<Option<TailScan>, IndexError> {
        let trigrams = match open_tail_dirs(fetch, tails).await? {
            Some(d) => d,
            None => return Ok(None),
        };
        // Facet postings (includes and excludes) live in the separate facet
        // sidecar, so they are read with the filter's own fetcher, not the index
        // fetcher.
        let mut facets = Vec::with_capacity(facet_fields.len());
        for field in facet_fields {
            let ff = facet_fetch.ok_or(IndexError::BadQuery(
                "facet fetcher required for a filtered tail scan",
            ))?;
            match open_tail_dirs(ff, field).await? {
                Some(d) => facets.push(d),
                None => return Ok(None),
            }
        }
        let excludes = if exclude_ranges.is_empty() {
            Vec::new()
        } else {
            let ff = facet_fetch.ok_or(IndexError::BadQuery(
                "facet fetcher required for a filtered tail scan",
            ))?;
            match open_tail_dirs(ff, exclude_ranges).await? {
                Some(d) => d,
                None => return Ok(None),
            }
        };
        // Only buckets present in all trigrams AND (per field) in at least one
        // selected category can survive the filtered AND — so a selective facet
        // never reads trigram containers for buckets it would empty anyway.
        let mut keys = candidate_keys(&trigrams);
        // Skip the buckets the cursor already loaded as the eager prefix (`< min_key`); this scan
        // pages the rest. The keys are ascending, so this drops a prefix.
        keys.retain(|&k| k >= min_key);
        for field in &facets {
            if keys.is_empty() {
                break;
            }
            let field_keys = union_keys(field);
            keys.retain(|k| field_keys.binary_search(k).is_ok());
        }
        Ok(Some(TailScan {
            trigrams,
            facets,
            excludes,
            keys,
            pos: 0,
        }))
    }

    /// Whether every candidate bucket has been intersected.
    pub(crate) fn exhausted(&self) -> bool {
        self.pos >= self.keys.len()
    }

    /// Intersects up to `batch` more candidate buckets — the trigram strict AND,
    /// then each facet field's category-OR ANDed in, then the excluded categories
    /// subtracted — and returns the surviving docs (ascending), advancing the
    /// scan. Empty once exhausted.
    pub(crate) async fn next_window<F: RangeFetch>(
        &mut self,
        fetch: &F,
        facet_fetch: Option<&F>,
        batch: usize,
    ) -> Result<RoaringBitmap, IndexError> {
        if self.exhausted() {
            return Ok(RoaringBitmap::new());
        }
        let end = (self.pos + batch).min(self.keys.len());
        let window = &self.keys[self.pos..end];
        let mut bm = intersect_key_window(fetch, &self.trigrams, window).await?;
        if !self.facets.is_empty() {
            let ff = facet_fetch.ok_or(IndexError::BadQuery(
                "facet fetcher required for a filtered tail scan",
            ))?;
            for field in &self.facets {
                if bm.is_empty() {
                    break;
                }
                let reads = field.iter().map(|cat| read_dir_subset(ff, cat, window));
                let mut field_bm = RoaringBitmap::new();
                for c in join_all(reads).await {
                    field_bm |= c?;
                }
                bm &= field_bm;
            }
        }
        if !self.excludes.is_empty() && !bm.is_empty() {
            let ff = facet_fetch.ok_or(IndexError::BadQuery(
                "facet fetcher required for a filtered tail scan",
            ))?;
            let reads = self
                .excludes
                .iter()
                .map(|cat| read_dir_subset(ff, cat, window));
            let mut x = RoaringBitmap::new();
            for c in join_all(reads).await {
                x |= c?;
            }
            bm -= x;
        }
        self.pos = end;
        Ok(bm)
    }
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

    /// Draining the incremental TailScan must equal the whole-tail strict AND, in
    /// the same ascending order, for any batch size.
    #[test]
    fn tail_scan_matches_full_and() {
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
        for d in 300000..306000u32 {
            a.insert(d);
            b.insert(d);
            c.insert(d);
        }
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
        let want = block_on(tail_intersect_and(&fetch, &tails)).unwrap();
        for batch in [1usize, 2, 3, 100] {
            let mut scan = block_on(TailScan::open(&fetch, &tails, None, &[], &[], 0))
                .unwrap()
                .unwrap();
            let mut got = RoaringBitmap::new();
            while !scan.exhausted() {
                got |= block_on(scan.next_window(&fetch, None, batch)).unwrap();
            }
            assert_eq!(got, want, "batch={batch}");
        }
    }

    /// A [`MemoryFetch`] wrapper summing the bytes requested, so a test can assert what a
    /// windowed intersection actually fetched (not just what it returned).
    #[derive(Clone)]
    struct ByteCountingFetch {
        inner: MemoryFetch,
        bytes: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl RangeFetch for ByteCountingFetch {
        async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, crate::fetch::FetchError> {
            self.bytes.set(self.bytes.get() + len);
            self.inner.read(offset, len).await
        }
    }

    /// A selective posting must seed the window (smallest-first shrink): the result equals
    /// the in-memory AND, and the dense postings' containers are fetched only in the
    /// buckets the seed leaves alive — not across the whole window.
    #[test]
    fn window_seed_skips_dense_containers_where_the_seed_is_empty() {
        // rare: three docs, in buckets 2, 5, and 9 (5's doc misses the dense postings).
        // dense a/b: a ~5k-doc bitmap container in every bucket 1..=10 (~8 KB each).
        let mut rare = RoaringBitmap::new();
        for d in [2 * 65_536 + 10, 5 * 65_536 + 6_000, 9 * 65_536 + 40] {
            rare.insert(d);
        }
        let mut da = RoaringBitmap::new();
        let mut db = RoaringBitmap::new();
        for k in 1..=10u32 {
            for d in 0..5_000u32 {
                da.insert(k * 65_536 + d);
                db.insert(k * 65_536 + d);
            }
        }
        let want = {
            let mut x = rare.clone();
            x &= &da;
            x &= &db;
            x
        };
        assert_eq!(want.len(), 2, "buckets 2 and 9 survive, bucket 5 does not");

        let (sr, sa, sb) = (ser(&rare), ser(&da), ser(&db));
        let mut buf = Vec::new();
        let or = buf.len() as u64;
        buf.extend_from_slice(&sr);
        let oa = buf.len() as u64;
        buf.extend_from_slice(&sa);
        let ob = buf.len() as u64;
        buf.extend_from_slice(&sb);
        let total = buf.len();
        let bytes = std::rc::Rc::new(std::cell::Cell::new(0));
        let fetch = ByteCountingFetch {
            inner: MemoryFetch::new(buf),
            bytes: bytes.clone(),
        };
        let tails = vec![(or, sr.len()), (oa, sa.len()), (ob, sb.len())];
        let dirs = block_on(open_tail_dirs(&fetch, &tails))
            .unwrap()
            .expect("seekable layout");

        let window: Vec<u16> = (1..=10).collect();
        bytes.set(0);
        let got = block_on(intersect_key_window(&fetch, &dirs, &window)).unwrap();
        assert_eq!(got, want);
        // Unseeded, the two dense postings would read all 10 buckets each (~160 KB, most
        // of `total`). Seeded, they read only the seed's 3 surviving buckets.
        assert!(
            bytes.get() < total / 2,
            "seeded window read {} of {total} bytes — dense containers were not skipped",
            bytes.get()
        );

        // An all-dense window (no posting 4x smaller) keeps the single wave and must
        // still intersect correctly.
        let dense_dirs = &dirs[1..];
        let want_dense = {
            let mut x = da.clone();
            x &= &db;
            x
        };
        let got_dense = block_on(intersect_key_window(&fetch, dense_dirs, &window)).unwrap();
        assert_eq!(got_dense, want_dense);
    }

    /// A facet-aware TailScan must equal the filtered AND:
    /// `(trigram_a AND trigram_b) AND (cat1 OR cat2)`, for any batch size.
    #[test]
    fn tail_scan_with_facets_matches_filtered_and() {
        let mut a = RoaringBitmap::new();
        let mut b = RoaringBitmap::new();
        let mut cat1 = RoaringBitmap::new();
        let mut cat2 = RoaringBitmap::new();
        // Trigram AND lands docs across several buckets; the facet OR keeps a subset.
        for d in [70001u32, 130000, 200005, 400000, 5_000_000] {
            a.insert(d);
            b.insert(d);
        }
        for d in 300000..304000u32 {
            a.insert(d);
            b.insert(d);
        }
        for d in [70001u32, 200005, 303000, 5_000_000] {
            cat1.insert(d);
        }
        for d in [130000u32, 303500] {
            cat2.insert(d);
        }
        let want = {
            let mut x = a.clone();
            x &= &b;
            let mut f = cat1.clone();
            f |= &cat2;
            x &= &f;
            x
        };

        let mut buf = Vec::new();
        let mut put = |bm: &RoaringBitmap| {
            let s = ser(bm);
            let off = buf.len() as u64;
            buf.extend_from_slice(&s);
            (off, s.len())
        };
        let ra = put(&a);
        let rb = put(&b);
        let r1 = put(&cat1);
        let r2 = put(&cat2);
        let fetch = MemoryFetch::new(buf);

        let tails = vec![ra, rb];
        let facet_fields = vec![vec![r1, r2]]; // one field, categories cat1/cat2
        for batch in [1usize, 2, 7, 100] {
            let mut scan = block_on(TailScan::open(
                &fetch,
                &tails,
                Some(&fetch),
                &facet_fields,
                &[],
                0,
            ))
            .unwrap()
            .unwrap();
            let mut got = RoaringBitmap::new();
            while !scan.exhausted() {
                got |= block_on(scan.next_window(&fetch, Some(&fetch), batch)).unwrap();
            }
            assert_eq!(got, want, "batch={batch}");
        }
    }

    /// An exclude-aware TailScan must subtract the negated categories:
    /// `(trigram_a AND trigram_b) AND (cat1 OR cat2) ANDNOT (excl1 OR excl2)`,
    /// across every batch size (regression for excludes dropped on the
    /// incremental tail path).
    #[test]
    fn tail_scan_with_excludes_matches_filtered_andnot() {
        let mut a = RoaringBitmap::new();
        let mut b = RoaringBitmap::new();
        let mut cat1 = RoaringBitmap::new();
        let mut cat2 = RoaringBitmap::new();
        let mut excl1 = RoaringBitmap::new();
        let mut excl2 = RoaringBitmap::new();
        for d in [70001u32, 130000, 200005, 400000, 5_000_000] {
            a.insert(d);
            b.insert(d);
        }
        for d in 300000..304000u32 {
            a.insert(d);
            b.insert(d);
        }
        for d in [70001u32, 200005, 303000, 5_000_000] {
            cat1.insert(d);
        }
        for d in [130000u32, 303500] {
            cat2.insert(d);
        }
        // Excludes land on docs across several buckets, including one inside the
        // dense bucket-4 run and one that is NOT in the include set (a no-op).
        for d in [200005u32, 303000, 303501] {
            excl1.insert(d);
        }
        for d in [130000u32, 400000, 6_000_000] {
            excl2.insert(d);
        }
        let want = {
            let mut x = a.clone();
            x &= &b;
            let mut f = cat1.clone();
            f |= &cat2;
            x &= &f;
            let mut ex = excl1.clone();
            ex |= &excl2;
            x -= &ex;
            x
        };

        let mut buf = Vec::new();
        let mut put = |bm: &RoaringBitmap| {
            let s = ser(bm);
            let off = buf.len() as u64;
            buf.extend_from_slice(&s);
            (off, s.len())
        };
        let ra = put(&a);
        let rb = put(&b);
        let r1 = put(&cat1);
        let r2 = put(&cat2);
        let x1 = put(&excl1);
        let x2 = put(&excl2);
        let fetch = MemoryFetch::new(buf);

        let tails = vec![ra, rb];
        let facet_fields = vec![vec![r1, r2]]; // one field, categories cat1/cat2
        let excludes = vec![x1, x2];
        for batch in [1usize, 2, 7, 100] {
            let mut scan = block_on(TailScan::open(
                &fetch,
                &tails,
                Some(&fetch),
                &facet_fields,
                &excludes,
                0,
            ))
            .unwrap()
            .unwrap();
            let mut got = RoaringBitmap::new();
            while !scan.exhausted() {
                got |= block_on(scan.next_window(&fetch, Some(&fetch), batch)).unwrap();
            }
            assert_eq!(got, want, "include+exclude batch={batch}");
        }
    }

    /// An excludes-only TailScan (no include fields) must return the trigram AND
    /// minus the negated categories — not the unfiltered AND.
    #[test]
    fn tail_scan_excludes_only() {
        let mut a = RoaringBitmap::new();
        let mut b = RoaringBitmap::new();
        let mut excl = RoaringBitmap::new();
        for d in [70001u32, 130000, 200005, 400000, 5_000_000] {
            a.insert(d);
            b.insert(d);
        }
        for d in [130000u32, 5_000_000] {
            excl.insert(d);
        }
        let want = {
            let mut x = a.clone();
            x &= &b;
            x -= &excl;
            x
        };

        let mut buf = Vec::new();
        let mut put = |bm: &RoaringBitmap| {
            let s = ser(bm);
            let off = buf.len() as u64;
            buf.extend_from_slice(&s);
            (off, s.len())
        };
        let ra = put(&a);
        let rb = put(&b);
        let xr = put(&excl);
        let fetch = MemoryFetch::new(buf);

        let tails = vec![ra, rb];
        let excludes = vec![xr];
        for batch in [1usize, 2, 100] {
            let mut scan = block_on(TailScan::open(
                &fetch,
                &tails,
                Some(&fetch),
                &[],
                &excludes,
                0,
            ))
            .unwrap()
            .unwrap();
            let mut got = RoaringBitmap::new();
            while !scan.exhausted() {
                got |= block_on(scan.next_window(&fetch, Some(&fetch), batch)).unwrap();
            }
            assert_eq!(got, want, "excludes-only batch={batch}");
            assert!(
                !got.contains(130000) && !got.contains(5_000_000),
                "excluded docs must not survive (batch={batch})"
            );
        }
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

    /// Yields once (Pending then Ready) so concurrently-polled read futures
    /// interleave under a single-threaded executor rather than each running to
    /// completion before the next is polled.
    #[derive(Default)]
    struct YieldOnce(bool);
    impl std::future::Future for YieldOnce {
        type Output = ();
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            if self.0 {
                std::task::Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }
    }

    /// Wraps a [`MemoryFetch`] and records peak concurrent reads, so a test can
    /// prove a fetch wave overlaps rather than running sequentially.
    struct InflightFetch {
        inner: MemoryFetch,
        inflight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        max_inflight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl InflightFetch {
        fn new(bytes: Vec<u8>) -> Self {
            InflightFetch {
                inner: MemoryFetch::new(bytes),
                inflight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                max_inflight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
        fn max_inflight(&self) -> usize {
            self.max_inflight.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl crate::fetch::RangeFetch for InflightFetch {
        async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, crate::fetch::FetchError> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.inflight.fetch_add(1, SeqCst) + 1;
            self.max_inflight.fetch_max(now, SeqCst);
            YieldOnce::default().await;
            let r = self.inner.read(offset, len).await;
            self.inflight.fetch_sub(1, SeqCst);
            r
        }
    }

    /// `open_tail_dirs` must read every posting's header prefix in one concurrent
    /// wave (task 061 item 1), not one round trip per posting. With three tails
    /// the peak in-flight read count must exceed one.
    #[test]
    fn open_tail_dirs_reads_headers_concurrently() {
        let mut a = RoaringBitmap::new();
        let mut b = RoaringBitmap::new();
        let mut c = RoaringBitmap::new();
        for d in [70000u32, 130000, 200005, 5_000_000] {
            a.insert(d);
            b.insert(d);
            c.insert(d);
        }
        for d in 300000..306000u32 {
            a.insert(d);
            b.insert(d);
            c.insert(d);
        }
        let (sa, sb, sc) = (ser(&a), ser(&b), ser(&c));
        let mut buf = Vec::new();
        let oa = buf.len() as u64;
        buf.extend_from_slice(&sa);
        let ob = buf.len() as u64;
        buf.extend_from_slice(&sb);
        let oc = buf.len() as u64;
        buf.extend_from_slice(&sc);
        let fetch = InflightFetch::new(buf);
        let tails = vec![(oa, sa.len()), (ob, sb.len()), (oc, sc.len())];
        let dirs = block_on(open_tail_dirs(&fetch, &tails))
            .unwrap()
            .expect("seekable layout");
        assert_eq!(dirs.len(), 3);
        assert!(
            fetch.max_inflight() >= 2,
            "header prefixes must be read concurrently, peak was {}",
            fetch.max_inflight()
        );
    }
}
