//! Reader for the optional sort-column store (`RRSC`). See `SORTCOLS.md`.
//!
//! A search over the [`crate::index::Index`] returns doc IDs in the primary
//! static-rank order. An `RRSC` store holds one or more **dense, fixed-width
//! columns indexed by doc ID**, so the reader can fetch a stored value per doc and
//! re-rank a *materialized* candidate set client-side (sort by rating, publication
//! date, …) — the same no-backend HTTP-Range model as the text and facet indexes.
//!
//! Boot reads only the compact meta region (header + column table + name blob) and
//! keeps it in memory; the dense data — the bulk of the file — is range-fetched per
//! query. A batch of doc IDs is sorted by offset and coalesced into a few spans
//! fetched in one concurrent wave (mirroring [`crate::posting`] and
//! [`crate::records::RecordStore::get_many`]).
//!
//! The same container is how a **secondary full index** maps back to the primary
//! doc-ID space: a one-column `u32` store where `primary[secondary_id]` is the
//! permutation `secondary_docid → primary_docid`. A result page is a contiguous run
//! of secondary doc IDs, so [`SortCols::slice_u32`] resolves it in one ranged read.

use crate::fetch::RangeFetch;
use crate::index::{read_u16, read_u32, read_u64, IndexError};
use futures::future::join_all;

/// `RRSC` magic.
const MAGIC: &[u8; 4] = b"RRSC";
/// Header size in bytes.
const HEADER_SIZE: usize = 16;
/// Column-table entry size: nameOff(4) + nameLen(2) + valueType(1) + pad(1) +
/// dataOff(8) + rows(4) + reserved(4).
const COL_ENTRY: usize = 24;
/// Values within this many bytes of each other are fetched as one ranged read
/// rather than separately, so a run of nearby doc IDs collapses to one request.
/// Bridging a gap wastes at most this many bytes but saves a round-trip.
const SPAN_GAP: u64 = 4096;

/// A column's stored value type. The on-disk code is `1`=u16, `2`=u32, `3`=i32,
/// `4`=f32; `width` is 2 for `u16`, else 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    /// Unsigned 16-bit.
    U16,
    /// Unsigned 32-bit.
    U32,
    /// Signed 32-bit.
    I32,
    /// IEEE-754 32-bit float.
    F32,
}

impl ValueType {
    /// Maps the on-disk type code to a [`ValueType`], or `None` if unknown.
    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(ValueType::U16),
            2 => Some(ValueType::U32),
            3 => Some(ValueType::I32),
            4 => Some(ValueType::F32),
            _ => None,
        }
    }

    /// Width in bytes of one stored value.
    pub fn width(self) -> usize {
        match self {
            ValueType::U16 => 2,
            _ => 4,
        }
    }

    /// Decodes one value from `buf` at byte offset `off`.
    fn decode(self, buf: &[u8], off: usize) -> Value {
        match self {
            ValueType::U16 => Value::U16(read_u16(buf, off)),
            ValueType::U32 => Value::U32(read_u32(buf, off)),
            ValueType::I32 => Value::I32(read_u32(buf, off) as i32),
            ValueType::F32 => Value::F32(f32::from_bits(read_u32(buf, off))),
        }
    }
}

/// One decoded column value, tagged with its type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// Unsigned 16-bit.
    U16(u16),
    /// Unsigned 32-bit.
    U32(u32),
    /// Signed 32-bit.
    I32(i32),
    /// IEEE-754 32-bit float.
    F32(f32),
}

impl Value {
    /// The value as an `f64`, the common key used to sort across types. Every
    /// `u16`/`u32`/`i32`/`f32` is exactly representable in `f64`.
    pub fn as_f64(self) -> f64 {
        match self {
            Value::U16(x) => x as f64,
            Value::U32(x) => x as f64,
            Value::I32(x) => x as f64,
            Value::F32(x) => x as f64,
        }
    }
}

/// One column's metadata: display name, value type, and the location of its dense
/// data. The data itself is range-fetched on demand.
#[derive(Debug, Clone)]
pub struct ColInfo {
    /// Column display name.
    pub name: String,
    /// Stored value type.
    pub value_type: ValueType,
    /// Absolute file offset of the column's dense data.
    data_off: u64,
}

/// A range-fetchable sort-column store. Holds only the meta region in memory;
/// column data is read on demand via `F`. Cheaply [`Clone`]able (the meta plus a
/// fetcher handle) so a [`crate::secondary::SecondaryCursor`] can own a copy.
#[derive(Clone)]
pub struct SortCols<F: RangeFetch + Clone> {
    fetch: F,
    rows: u32,
    columns: Vec<ColInfo>,
}

impl<F: RangeFetch + Clone> SortCols<F> {
    /// Boots the store: reads the 16-byte header and the meta region (the column
    /// table and name blob), validating magic, version, and value-type codes. No
    /// dense data is read — boot is a few KB regardless of row count.
    pub async fn open(fetch: F) -> Result<Self, IndexError> {
        let header = fetch.read(0, HEADER_SIZE).await?;
        if &header[0..4] != MAGIC {
            let mut m = [0u8; 4];
            m.copy_from_slice(&header[0..4]);
            return Err(IndexError::BadMagic(m));
        }
        let version = read_u16(&header, 4);
        if version != 1 {
            return Err(IndexError::BadVersion(version));
        }
        let col_count = read_u16(&header, 6) as usize;
        let rows = read_u32(&header, 8);
        let str_bytes = read_u32(&header, 12) as usize;

        // Checked layout: on wasm32 (usize = 32-bit) an attacker-controlled
        // colCount/strBytes could otherwise wrap to a short read, then drive
        // out-of-bounds indexing below.
        let str_blob = col_count
            .checked_mul(COL_ENTRY)
            .and_then(|x| x.checked_add(HEADER_SIZE))
            .ok_or(IndexError::Malformed("RRSC column table size overflow"))?;
        let meta_len = str_blob
            .checked_add(str_bytes)
            .ok_or(IndexError::Malformed("RRSC meta size overflow"))?;
        let buf = fetch.read(0, meta_len).await?;
        if buf.len() < meta_len {
            return Err(IndexError::Malformed("RRSC meta region truncated"));
        }

        let read_name = |off: usize, len: usize| -> Result<String, IndexError> {
            let end = off
                .checked_add(len)
                .filter(|&e| e <= str_bytes)
                .ok_or(IndexError::Malformed("RRSC name out of string blob"))?;
            Ok(String::from_utf8_lossy(&buf[str_blob + off..str_blob + end]).into_owned())
        };

        let mut columns = Vec::with_capacity(col_count);
        for i in 0..col_count {
            let b = HEADER_SIZE + i * COL_ENTRY;
            let name_off = read_u32(&buf, b) as usize;
            let name_len = read_u16(&buf, b + 4) as usize;
            let value_type = ValueType::from_code(buf[b + 6])
                .ok_or(IndexError::Malformed("RRSC unknown value type"))?;
            let data_off = read_u64(&buf, b + 8);
            let col_rows = read_u32(&buf, b + 16);
            if col_rows != rows {
                return Err(IndexError::Malformed("RRSC column row count mismatch"));
            }
            columns.push(ColInfo {
                name: read_name(name_off, name_len)?,
                value_type,
                data_off,
            });
        }

        Ok(SortCols {
            fetch,
            rows,
            columns,
        })
    }

    /// Number of rows (doc IDs `0..rows`) every column holds.
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// The columns' metadata, in stored order.
    pub fn columns(&self) -> &[ColInfo] {
        &self.columns
    }

    /// The index of the column named `name`, or `None` if absent.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Values for `ids` in column `col`, aligned with `ids`. The doc IDs are sorted
    /// by byte offset and coalesced into spans (bridging gaps up to [`SPAN_GAP`]),
    /// fetched in one concurrent wave; a candidate set clustered in the popular
    /// low-doc-ID range collapses to a handful of reads. Errors if `col` is out of
    /// range or any id is `>= rows`.
    pub async fn values(&self, col: usize, ids: &[u32]) -> Result<Vec<Value>, IndexError> {
        let info = self
            .columns
            .get(col)
            .ok_or(IndexError::BadQuery("sortcols column index out of range"))?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let w = info.value_type.width() as u64;

        let mut order: Vec<usize> = (0..ids.len()).collect();
        order.sort_unstable_by_key(|&i| ids[i]);
        if ids[order[order.len() - 1]] >= self.rows {
            return Err(IndexError::BadQuery("sortcols doc id out of range"));
        }

        // Coalesce the (offset-sorted) reads into spans, recording each id's span.
        let mut spans: Vec<(u64, u64)> = Vec::new();
        let mut span_of: Vec<usize> = vec![0; ids.len()];
        for &i in &order {
            let s = ids[i] as u64 * w;
            let e = s + w;
            match spans.last_mut() {
                Some(last) if s <= last.1 + SPAN_GAP => {
                    if e > last.1 {
                        last.1 = e;
                    }
                }
                _ => spans.push((s, e)),
            }
            span_of[i] = spans.len() - 1;
        }

        let reads = spans
            .iter()
            .map(|&(s, e)| self.fetch.read(info.data_off + s, (e - s) as usize));
        let datas = join_all(reads).await;
        let mut bytes = Vec::with_capacity(spans.len());
        for d in datas {
            bytes.push(d?);
        }

        let mut out = vec![Value::U32(0); ids.len()];
        for i in 0..ids.len() {
            let span = span_of[i];
            let rel = (ids[i] as u64 * w - spans[span].0) as usize;
            out[i] = info.value_type.decode(&bytes[span], rel);
        }
        Ok(out)
    }

    /// Like [`SortCols::values`] but for a `u32` column, returning the raw `u32`s —
    /// the permutation gather (`secondary_docid → primary_docid`) a secondary
    /// index's result page uses. The page's doc IDs are scattered (only the matches),
    /// so this gathers (coalesced) rather than slicing. Errors if `col` is not `u32`.
    pub async fn values_u32(&self, col: usize, ids: &[u32]) -> Result<Vec<u32>, IndexError> {
        let info = self
            .columns
            .get(col)
            .ok_or(IndexError::BadQuery("sortcols column index out of range"))?;
        if info.value_type != ValueType::U32 {
            return Err(IndexError::BadQuery("sortcols column is not u32"));
        }
        Ok(self
            .values(col, ids)
            .await?
            .into_iter()
            .map(|v| match v {
                Value::U32(x) => x,
                _ => unreachable!("column validated as u32"),
            })
            .collect())
    }

    /// One value for doc `id` in column `col`, or `None` if `id >= rows`.
    pub async fn value(&self, col: usize, id: u32) -> Result<Option<Value>, IndexError> {
        if id >= self.rows {
            return Ok(None);
        }
        Ok(self.values(col, &[id]).await?.into_iter().next())
    }

    /// The contiguous run `[start, start+len)` of a `u32` column in one ranged read
    /// — the permutation-page fast path (`secondary_docid → primary_docid`). Clamps
    /// to `rows`, so paging past the end returns a short (or empty) vector. Errors
    /// if `col` is out of range or is not a `u32` column.
    pub async fn slice_u32(
        &self,
        col: usize,
        start: u32,
        len: usize,
    ) -> Result<Vec<u32>, IndexError> {
        let info = self
            .columns
            .get(col)
            .ok_or(IndexError::BadQuery("sortcols column index out of range"))?;
        if info.value_type != ValueType::U32 {
            return Err(IndexError::BadQuery("sortcols column is not u32"));
        }
        let avail = (self.rows as u64).saturating_sub(start as u64);
        let take = (len as u64).min(avail) as usize;
        if take == 0 {
            return Ok(Vec::new());
        }
        let off = info.data_off + start as u64 * 4;
        let buf = self.fetch.read(off, take * 4).await?;
        Ok((0..take).map(|i| read_u32(&buf, i * 4)).collect())
    }

    /// The top `k` of `candidates` by column `col`, descending (largest first) when
    /// `descending`, else ascending. Ties keep ascending doc-ID order, so equal
    /// secondary values stay in primary-rank order (e.g. "newest, then
    /// most-cited"). One coalesced fetch of the candidates' values, then a
    /// partial sort.
    pub async fn topk(
        &self,
        col: usize,
        candidates: &[u32],
        k: usize,
        descending: bool,
    ) -> Result<Vec<u32>, IndexError> {
        let vals = self.values(col, candidates).await?;
        let mut paired: Vec<(u32, f64)> = candidates
            .iter()
            .zip(vals.iter())
            .map(|(&id, v)| (id, v.as_f64()))
            .collect();
        paired.sort_by(|a, b| {
            let by_val = if descending {
                b.1.partial_cmp(&a.1)
            } else {
                a.1.partial_cmp(&b.1)
            };
            by_val
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        paired.truncate(k);
        Ok(paired.into_iter().map(|(id, _)| id).collect())
    }
}

// Uses the native-only build writer; gated to native so `wasm-pack test` builds.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::build::{write_sortcols, ColumnValues, SortColumn};
    use crate::MemoryFetch;
    use futures::executor::block_on;

    fn store(cols: Vec<SortColumn>) -> SortCols<MemoryFetch> {
        let mut out = Vec::new();
        write_sortcols(&mut out, cols).unwrap();
        block_on(SortCols::open(MemoryFetch::new(out))).unwrap()
    }

    #[test]
    fn round_trips_all_value_types() {
        let sc = store(vec![
            SortColumn {
                name: "year".to_string(),
                values: ColumnValues::U16(vec![2020, 1999, 2024]),
            },
            SortColumn {
                name: "cites".to_string(),
                values: ColumnValues::U32(vec![10, 4_000_000_000, 0]),
            },
            SortColumn {
                name: "delta".to_string(),
                values: ColumnValues::I32(vec![-5, 0, 7]),
            },
            SortColumn {
                name: "score".to_string(),
                values: ColumnValues::F32(vec![1.5, -2.25, 0.0]),
            },
        ]);
        assert_eq!(sc.rows(), 3);
        assert_eq!(sc.column_index("delta"), Some(2));
        assert_eq!(sc.column_index("missing"), None);

        assert_eq!(
            block_on(sc.values(0, &[2, 0, 1])).unwrap(),
            vec![Value::U16(2024), Value::U16(2020), Value::U16(1999)]
        );
        assert_eq!(
            block_on(sc.value(1, 1)).unwrap(),
            Some(Value::U32(4_000_000_000))
        );
        assert_eq!(
            block_on(sc.values(2, &[0, 2])).unwrap(),
            vec![Value::I32(-5), Value::I32(7)]
        );
        assert_eq!(block_on(sc.value(3, 1)).unwrap(), Some(Value::F32(-2.25)));
        assert!(block_on(sc.value(0, 3)).unwrap().is_none());
    }

    #[test]
    fn coalesced_values_match_per_id_reads() {
        // A dense block plus a far-away id: one span for the cluster, one for the
        // outlier. The result must equal a naive per-id read regardless of order.
        let vals: Vec<u32> = (0..5000u32).collect();
        let sc = store(vec![SortColumn {
            name: "v".to_string(),
            values: ColumnValues::U32(vals.clone()),
        }]);
        let ids = vec![4999u32, 0, 1, 2, 3, 2500, 4];
        let got = block_on(sc.values(0, &ids)).unwrap();
        let want: Vec<Value> = ids.iter().map(|&d| Value::U32(vals[d as usize])).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn slice_u32_reads_contiguous_run_and_clamps() {
        let vals: Vec<u32> = (100..200u32).collect();
        let sc = store(vec![SortColumn {
            name: "primary".to_string(),
            values: ColumnValues::U32(vals.clone()),
        }]);
        assert_eq!(
            block_on(sc.slice_u32(0, 10, 3)).unwrap(),
            vec![110, 111, 112]
        );
        // Past the end clamps to the available tail.
        assert_eq!(block_on(sc.slice_u32(0, 98, 10)).unwrap(), vec![198, 199]);
        assert!(block_on(sc.slice_u32(0, 100, 5)).unwrap().is_empty());
    }

    #[test]
    fn slice_u32_rejects_non_u32_column() {
        let sc = store(vec![SortColumn {
            name: "year".to_string(),
            values: ColumnValues::U16(vec![2000, 2001]),
        }]);
        assert!(matches!(
            block_on(sc.slice_u32(0, 0, 2)),
            Err(IndexError::BadQuery(_))
        ));
    }

    #[test]
    fn topk_orders_by_value_then_doc_id() {
        // Years with a tie at 2020 between docs 1 and 4 -> doc 1 first (lower rank).
        let sc = store(vec![SortColumn {
            name: "year".to_string(),
            values: ColumnValues::U16(vec![2019, 2020, 2010, 2024, 2020]),
        }]);
        let cands = vec![0u32, 1, 2, 3, 4];
        // Newest first: 2024(3), 2020(1), 2020(4), 2019(0), 2010(2).
        assert_eq!(
            block_on(sc.topk(0, &cands, 3, true)).unwrap(),
            vec![3, 1, 4]
        );
        // Oldest first: 2010(2), 2019(0), 2020(1), ...
        assert_eq!(block_on(sc.topk(0, &cands, 2, false)).unwrap(), vec![2, 0]);
    }

    #[test]
    fn out_of_range_id_errors() {
        let sc = store(vec![SortColumn {
            name: "v".to_string(),
            values: ColumnValues::U32(vec![1, 2, 3]),
        }]);
        assert!(matches!(
            block_on(sc.values(0, &[3])),
            Err(IndexError::BadQuery(_))
        ));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(b"XXXX");
        assert!(matches!(
            block_on(SortCols::open(MemoryFetch::new(bytes))),
            Err(IndexError::BadMagic(_))
        ));
    }
}
