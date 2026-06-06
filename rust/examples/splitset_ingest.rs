//! A worked **ingestion client** for the `RRSS` split set — the queue-as-WAL pattern the
//! library deliberately leaves to the embedding app (`SPLITSET.md` §writer). The library's
//! `SplitSetWriter` is pure (bytes in, bytes out); *this* file is the client that owns
//! transport, durability, and scheduling. Here the "object store" and "queue" are in-memory
//! stand-ins for S3 + SQS/Kinesis, so the whole thing runs with no backend:
//!
//!   cargo run --release --features splits --example splitset_ingest
//!
//! The loop is the one documented in `splitset_write.rs`:
//!   poll the source → `add` → when a trigger fires, `flush()` → PUT the split, then PUT the
//!   manifest (the atomic cutover) → ack the source. A reader that re-fetches the manifest
//!   after a cutover sees the new docs — so *freshness == the client's flush cadence*. A
//!   `compact()` later folds the accumulated L0 deltas into one split to bound read fan-out.

use futures::executor::block_on;
use roaringrange::{MemoryFetch, Policy, SplitFetcher, SplitSet, SplitSetWriter, WriterConfig};
use std::collections::HashMap;

/// The manifest's well-known key in the object store (the pointer flipped on every cutover).
const MANIFEST_KEY: &str = "index.rrss";

/// An in-memory stand-in for an object store (S3/GCS). A real client PUTs/GETs over HTTP; the
/// only contract the library relies on is that the manifest PUT is the **last** write of a
/// cutover, so a reader never sees a manifest pointing at a split that isn't durable yet.
#[derive(Default)]
struct ObjectStore {
    objects: HashMap<String, Vec<u8>>,
    put_bytes: u64,
}

impl ObjectStore {
    fn put(&mut self, key: &str, bytes: Vec<u8>) {
        self.put_bytes += bytes.len() as u64;
        self.objects.insert(key.to_string(), bytes);
    }
    fn delete(&mut self, key: &str) {
        self.objects.remove(key);
    }
}

/// A [`SplitFetcher`] over a snapshot of the store's current objects (what a reader would GET).
struct StoreResolver {
    objects: HashMap<String, Vec<u8>>,
}

impl SplitFetcher for StoreResolver {
    type Fetch = MemoryFetch;
    fn fetch_named(&self, name: &str) -> MemoryFetch {
        MemoryFetch::new(self.objects.get(name).cloned().unwrap_or_default())
    }
}

/// Re-opens the manifest from the store and runs `query` (top-`k`) — exactly what a stateless
/// reader (the browser, a Lambda) does on each request: GET the manifest, then the splits.
fn search(store: &ObjectStore, query: &str, k: usize) -> Vec<u32> {
    let manifest = store
        .objects
        .get(MANIFEST_KEY)
        .expect("manifest present")
        .clone();
    let ss = block_on(SplitSet::open(MemoryFetch::new(manifest))).unwrap();
    let resolver = StoreResolver {
        objects: store.objects.clone(),
    };
    block_on(ss.search(&resolver, query, k)).unwrap()
}

fn main() {
    // The client owns the writer config: a fresh stable-key writer (deltas are always
    // ingest-ordered), with term Bloom filters so a query can skip splits without its terms.
    let mut writer = SplitSetWriter::new(WriterConfig {
        gram_size: 3,
        head_boundary: 0,
        stride: 0,
        byte_cap: 1 << 20,
        name_prefix: "live".to_string(),
        policy: Policy::StableKey,
        tier_count: 0,
        sortcol: None,
        bloom_bits_per_key: 10,
    });
    let mut store = ObjectStore::default();

    // A simulated source: three batches of documents arriving over time (e.g. SQS messages).
    // The queue *is* the WAL — we ack only after the split + manifest are durable.
    let batches = [
        vec!["alpha bravo", "bravo charlie", "charlie delta"],
        vec!["delta echo", "echo foxtrot", "foxtrot golf"],
        vec!["golf hotel", "hotel india", "india juliet"],
    ];

    // Flush trigger: in production a size (`memtable_bytes`) or interval threshold. Here we
    // flush once per batch to show the cutover cadence on small data.
    let mut delta_keys: Vec<String> = Vec::new();
    for (i, batch) in batches.iter().enumerate() {
        for &text in batch {
            writer.add_text(text); // poll source -> add to the in-RAM memtable
        }
        println!(
            "batch {i}: added {} docs, memtable ~{} B",
            batch.len(),
            writer.memtable_bytes()
        );
        // flush(): seal the memtable into an immutable delta split + a new manifest, as bytes.
        if let Some(f) = writer.flush().unwrap() {
            store.put(&f.split_name, f.split_bytes); // 1. PUT the split
            store.put(MANIFEST_KEY, f.manifest); // 2. PUT the manifest = atomic cutover
            delta_keys.push(f.split_name.clone()); // (3. now safe to ack the source)
            println!("  flushed {} -> cutover", f.split_name);
        }
        // After each cutover a reader immediately sees the new docs — freshness = flush cadence.
        println!("  query \"echo\" now -> {:?}", search(&store, "echo", 10));
    }

    // The store now holds three L0 delta splits + the manifest. A short-interval client would
    // periodically compact them to bound read fan-out (and drop tombstoned docs).
    println!("\nbefore compaction: {} delta splits", delta_keys.len());
    let inputs: Vec<(String, Vec<u8>)> = delta_keys
        .iter()
        .map(|k| (k.clone(), store.objects[k].clone()))
        .collect();
    let c = writer.compact(&inputs).unwrap();
    store.put(&c.split_name, c.split_bytes); // PUT the merged split
    store.put(MANIFEST_KEY, c.manifest); // PUT the manifest = atomic cutover
    for key in &c.removed {
        store.delete(key); // delete the superseded inputs only after the cutover
    }
    println!(
        "compacted {} deltas into {} (deleted {:?})",
        inputs.len(),
        c.split_name,
        c.removed
    );

    let ss = block_on(SplitSet::open(MemoryFetch::new(
        store.objects[MANIFEST_KEY].clone(),
    )))
    .unwrap();
    println!("after compaction: {} split(s)", ss.splits().len());
    println!("query \"echo\"  -> {:?}", search(&store, "echo", 10));
    println!("query \"india\" -> {:?}", search(&store, "india", 10));
    println!(
        "query \"zzzz\"  -> {:?} (Bloom-pruned, no split reads)",
        search(&store, "zzzz", 10)
    );
    println!(
        "\ntotal bytes PUT to the store over the run: {} B",
        store.put_bytes
    );

    // Sanity: the freshly-ingested docs are findable ("hotel india" = 7, "india juliet" = 8),
    // and compaction preserved results.
    assert_eq!(search(&store, "india", 10), vec![7, 8]);
    assert!(search(&store, "zzzz", 10).is_empty());
}
