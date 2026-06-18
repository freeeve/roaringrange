"""Type stubs for the roaringrange PyO3 bindings — the build-side index writers.

The readers live in the wasm/JS bundle; Python is for *building* the static index
files (trigram index, facets, records, vectors, terms, split sets). All builders write
files to a path (or, for `SplitSetWriter`, return bytes the caller persists).
"""

class BuildStats:
    """Result of `Builder.build`."""
    docs: int
    ngrams: int
    fields: int

class VectorBuildStats:
    """Result of `VectorBuilder.build` / `write_rrvi_from_faiss`."""
    dim: int
    nlist: int
    m: int
    nbits: int
    vectors: int

class SplitSetBuildStats:
    """Result of `SplitSetBuilder.build` / `TermSplitSetBuilder.build`."""
    docs: int
    splits: int
    total_bytes: int

class Builder:
    """Batch trigram index + facets + records (`RRSI`/`RRSF`/`RRSR`)."""
    def __init__(self, gram_size: int = 3, head_boundary: int = 65536) -> None: ...
    def add(
        self,
        rank: int,
        text: str,
        record: bytes,
        facets: dict[str, list[str]] | None = ...,
    ) -> None: ...
    def add_many(
        self,
        rows: list[tuple[int, str, bytes, dict[str, list[str]] | None]],
    ) -> None: ...
    def build(self, out_dir: str) -> BuildStats: ...
    def __len__(self) -> int: ...

class VectorBuilder:
    """IVFPQ vector index trainer (`RRVI`). `metric` is `"ip"`/`"cosine"`/`"l2"`."""
    def __init__(
        self,
        dim: int,
        nlist: int,
        m: int,
        nbits: int = 8,
        metric: str = "ip",
        kmeans_iters: int = 25,
        seed: int | None = ...,
    ) -> None: ...
    def add(self, doc_id: int, vector: list[float]) -> None: ...
    def add_many(self, items: list[tuple[int, list[float]]]) -> None: ...
    def build(self, out_path: str) -> VectorBuildStats: ...
    def __len__(self) -> int: ...

class TermBuilder:
    """Streaming term index builder (`RRTI` v2 blocked dictionary). `language` e.g.
    `"english"`; `block_cap` is the dict block byte cap (`None`/`0` = default)."""
    def __init__(
        self,
        head_boundary: int | None = ...,
        language: str | None = ...,
        stopwords: bool = False,
        block_cap: int | None = ...,
    ) -> None: ...
    def add_many(self, docs: list[tuple[int, str]]) -> None: ...
    def term_count(self) -> int: ...
    def finish(self, path: str) -> None: ...

class SplitSetBuilder:
    """Batch trigram split-set builder (`RRSS` with `RRSI` bodies). `sortcol` is
    `(rrsc_name, column, descending)` for the stable-key policy."""
    def __init__(
        self,
        policy: str = "tiered",
        byte_cap: int = 33554432,
        gram_size: int = 3,
        head_boundary: int = 0,
        stride: int = 0,
        name_prefix: str = "split",
        sortcol: tuple[str, int, bool] | None = ...,
        bloom_bits_per_key: int = 10,
    ) -> None: ...
    def add(self, text: str) -> int: ...
    def add_faceted(self, text: str, facets: dict[str, list[str]]) -> int: ...
    def doc_count(self) -> int: ...
    def build(self, out_dir: str, manifest_name: str = "index") -> SplitSetBuildStats: ...
    def __len__(self) -> int: ...

class TermSplitSetBuilder:
    """Term/FST split-set builder (`RRSS` with `RRTI` bodies). `language` enables
    Snowball stemming; `stopwords` drops stop words."""
    def __init__(
        self,
        policy: str = "tiered",
        byte_cap: int = 33554432,
        head_boundary: int = 0,
        name_prefix: str = "split",
        sortcol: tuple[str, int, bool] | None = ...,
        language: str | None = ...,
        stopwords: bool = False,
    ) -> None: ...
    def add(self, text: str) -> int: ...
    def add_faceted(self, text: str, facets: dict[str, list[str]]) -> int: ...
    def doc_count(self) -> int: ...
    def build(self, out_dir: str, manifest_name: str = "index") -> SplitSetBuildStats: ...
    def __len__(self) -> int: ...

class SplitSetWriter:
    """Pure, resumable `RRSS` ingestion writer. `flush`/`compact` return bytes the
    caller persists (PUT the split, then the manifest = atomic cutover)."""
    def __init__(
        self,
        gram_size: int = 3,
        byte_cap: int = 33554432,
        name_prefix: str = "split",
        policy: str = "stable_key",
        head_boundary: int = 0,
        stride: int = 0,
        tier_count: int = 0,
        sortcol: tuple[str, int, bool] | None = ...,
        bloom_bits_per_key: int = 10,
    ) -> None: ...
    @staticmethod
    def resume(
        manifest: bytes,
        gram_size: int = 3,
        head_boundary: int = 0,
        stride: int = 0,
        name_prefix: str = "split",
        bloom_bits_per_key: int = 10,
    ) -> SplitSetWriter: ...
    def add(self, text: str) -> int: ...
    def delete(self, doc_id: int) -> None: ...
    def doc_count(self) -> int: ...
    def memtable_doc_count(self) -> int: ...
    def memtable_bytes(self) -> int: ...
    # (split_name, split_bytes, manifest), or None when nothing to flush.
    def flush(self) -> tuple[str, bytes, bytes] | None: ...
    # (split_name, split_bytes, manifest, removed_split_names).
    def compact(self, inputs: list[tuple[str, bytes]]) -> tuple[str, bytes, bytes, list[str]]: ...

def tokenize(text: str, gram_size: int = 3) -> list[int]: ...
def write_term_index(
    path: str, docs: list[tuple[int, str]], head_boundary: int | None = ...
) -> None: ...
def write_rrvi_from_faiss(
    out_path: str,
    dim: int,
    nlist: int,
    m: int,
    centroids: bytes,
    codebooks: bytes,
    ids: bytes,
    assignments: bytes,
    codes: bytes,
    nbits: int = 8,
    metric: str = "ip",
    opq: bytes | None = ...,
) -> VectorBuildStats: ...
