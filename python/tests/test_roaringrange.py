"""Smoke tests for the roaringrange PyO3 bindings.

Run with `pytest python/tests`. They exercise both builders end-to-end and
validate the files by their magic / header bytes, so CI can run them against the
prebuilt abi3 wheel on every supported CPython (3.12–3.14).
"""
import math
import struct

import pytest
import roaringrange as rr


def test_tokenize_returns_keys():
    # "abc" maps to a single trigram key (see FORMAT.md test vectors); a 2-char
    # string yields none.
    assert rr.tokenize("abc") == [6382179]
    assert rr.tokenize("ab") == []


def test_text_builder_writes_dataset(tmp_path):
    b = rr.Builder(gram_size=3)
    b.add(rank=10, text="hello world", record=b'{"t":"hello"}', facets={"year": ["2020"]})
    b.add(rank=5, text="goodbye world", record=b'{"t":"bye"}', facets={"year": ["2021"]})
    assert len(b) == 2

    stats = b.build(str(tmp_path))
    assert stats.docs == 2
    assert stats.fields == 1
    assert stats.ngrams > 0

    assert (tmp_path / "index.rrs").read_bytes()[:4] == b"RRSI"
    assert (tmp_path / "index.rrf").read_bytes()[:4] == b"RRSF"
    assert (tmp_path / "records.idx").read_bytes()[:4] == b"RRSR"
    assert (tmp_path / "records.bin").exists()


def test_text_builder_add_many(tmp_path):
    # add_many stages a batch; result is identical to per-row add().
    b = rr.Builder(gram_size=3)
    b.add_many([
        (10, "hello world", b'{"t":"hello"}', {"year": ["2020"]}),
        (5, "goodbye world", b'{"t":"bye"}', None),
    ])
    assert len(b) == 2
    stats = b.build(str(tmp_path))
    assert stats.docs == 2
    assert (tmp_path / "index.rrs").read_bytes()[:4] == b"RRSI"


def test_write_term_index_writes_rrti(tmp_path):
    out = tmp_path / "terms.rrti"
    rr.write_term_index(
        str(out),
        [(0, "hello world"), (1, "goodbye world")],
    )
    head = out.read_bytes()[:16]
    assert head[:4] == b"RRTI"
    version, _flags, nterms = struct.unpack_from("<HHI", head, 4)
    (head_boundary,) = struct.unpack_from("<I", head, 12)
    assert version == 2  # v2 = blocked dictionary with a router FST (task 009)
    assert nterms == 3  # hello, world, goodbye
    assert head_boundary == 65536


def test_write_term_index_custom_head_boundary(tmp_path):
    out = tmp_path / "terms2.rrti"
    rr.write_term_index(str(out), [(0, "alpha beta")], head_boundary=131072)
    (head_boundary,) = struct.unpack_from("<I", out.read_bytes(), 12)
    assert head_boundary == 131072


def _unit_vectors(n, dim, seed=42):
    x = seed
    out = []
    for i in range(n):
        v = []
        for _ in range(dim):
            x ^= (x >> 12) & 0xFFFFFFFFFFFFFFFF
            x ^= (x << 25) & 0xFFFFFFFFFFFFFFFF
            x ^= (x >> 27) & 0xFFFFFFFFFFFFFFFF
            x &= 0xFFFFFFFFFFFFFFFF
            v.append(((x * 0x2545F4914F6CDD1D) & 0xFFFFFFFFFFFFFFFF) / 2**64 - 0.5)
        norm = math.sqrt(sum(c * c for c in v)) or 1.0
        out.append((i, [c / norm for c in v]))
    return out


def test_vector_builder_writes_rrvi(tmp_path):
    dim, nlist, m, n = 8, 4, 4, 60
    vb = rr.VectorBuilder(dim=dim, nlist=nlist, m=m, nbits=8, metric="ip")
    vb.add_many(_unit_vectors(n, dim))
    assert len(vb) == n

    out = tmp_path / "vectors.rrvi"
    stats = vb.build(str(out))
    assert stats.vectors == n
    assert (stats.dim, stats.nlist, stats.m, stats.nbits) == (dim, nlist, m, 8)

    head = out.read_bytes()[:48]
    assert head[:4] == b"RRVI"
    version, metric, _flags = struct.unpack_from("<HBB", head, 4)
    h_dim, h_nlist, h_m = struct.unpack_from("<III", head, 8)
    (h_n,) = struct.unpack_from("<Q", head, 24)
    assert version == 1
    assert metric == 0  # inner product
    assert (h_dim, h_nlist, h_m, head[20], h_n) == (dim, nlist, m, 8, n)


@pytest.mark.parametrize(
    "kwargs",
    [
        dict(dim=8, nlist=4, m=3),  # m does not divide dim
        dict(dim=8, nlist=4, m=4, nbits=9),  # nbits out of range
        dict(dim=8, nlist=4, m=4, metric="bogus"),  # unknown metric
        dict(dim=0, nlist=4, m=1),  # dim zero
    ],
)
def test_vector_builder_rejects_bad_params(kwargs):
    with pytest.raises(ValueError):
        rr.VectorBuilder(**kwargs)


def test_vector_builder_rejects_wrong_length(tmp_path):
    vb = rr.VectorBuilder(dim=8, nlist=2, m=2)
    with pytest.raises(ValueError):
        vb.add(0, [1.0, 2.0])  # length 2 != dim 8


def test_vector_builder_l2_metric(tmp_path):
    vb = rr.VectorBuilder(dim=4, nlist=2, m=2, metric="l2")
    vb.add_many(_unit_vectors(20, 4))
    out = tmp_path / "v.rrvi"
    vb.build(str(out))
    assert out.read_bytes()[6] == 1  # metric byte == L2


def _f32(vals):
    return struct.pack("<%df" % len(vals), *vals)


def _u32(vals):
    return struct.pack("<%dI" % len(vals), *vals)


def test_write_rrvi_from_faiss(tmp_path):
    # Already-"trained" parts (as a FAISS OPQ,IVF,PQ export would supply them),
    # passed as little-endian byte buffers. dim=4, m=2, nbits=2 (ksub=4), nlist=2.
    dim, nlist, m, nbits = 4, 2, 2, 2
    centroids = _f32([0, 0, 0, 0, 2, 2, 2, 2])
    codebook_2d = [0, 0, 1, 0, 0, 1, 1, 1]
    codebooks = _f32(codebook_2d + codebook_2d)  # two subspaces
    ids = _u32([10, 11, 12, 13])
    assignments = _u32([0, 0, 1, 1])
    codes = bytes([3, 3, 0, 0, 0, 0, 3, 3])  # n*m = 8

    out = tmp_path / "faiss.rrvi"
    stats = rr.write_rrvi_from_faiss(
        str(out), dim, nlist, m, centroids, codebooks, ids, assignments, codes,
        nbits=nbits, metric="l2",
    )
    assert (stats.vectors, stats.dim, stats.nlist, stats.m, stats.nbits) == (4, 4, 2, 2, 2)

    head = out.read_bytes()[:48]
    assert head[:4] == b"RRVI"
    assert head[6] == 1  # metric L2
    h_dim, h_nlist, h_m = struct.unpack_from("<III", head, 8)
    (h_n,) = struct.unpack_from("<Q", head, 24)
    assert (h_dim, h_nlist, h_m, head[20], h_n) == (dim, nlist, m, nbits, 4)


def test_write_rrvi_from_faiss_validates_lengths(tmp_path):
    with pytest.raises(ValueError):
        rr.write_rrvi_from_faiss(
            str(tmp_path / "bad.rrvi"), 4, 2, 2,
            _f32([0] * 8), _f32([0] * 16), _u32([0, 1]), _u32([0, 1]),
            bytes([0, 0, 0]),  # codes length 3, should be n*m = 4
            nbits=2, metric="l2",
        )


def test_splitset_builder_writes_manifest_and_splits(tmp_path):
    # A small byte cap forces several splits; every doc contains "abc".
    b = rr.SplitSetBuilder(policy="tiered", byte_cap=4096, gram_size=3, name_prefix="corpus")
    for i in range(200):
        b.add(f"abc tok{i:04}")
    assert len(b) == 200

    stats = b.build(str(tmp_path), manifest_name="index")
    assert stats.docs == 200
    assert stats.splits > 1
    assert stats.total_bytes > 0

    manifest = (tmp_path / "index.rrss").read_bytes()
    assert manifest[:4] == b"RRSS"
    version, _flags = struct.unpack_from("<HH", manifest, 4)
    split_count, base_count = struct.unpack_from("<II", manifest, 12)
    assert version == 1
    assert split_count == stats.splits
    assert base_count == stats.splits  # all base for a batch build
    # Every named split file was written.
    assert (tmp_path / "corpus-s00000.rrs").read_bytes()[:4] == b"RRSI"


def test_splitset_builder_rejects_unknown_policy():
    with pytest.raises(ValueError):
        rr.SplitSetBuilder(policy="bogus")


def test_splitset_builder_add_faceted_writes_sidecars(tmp_path):
    # Faceted docs across two splits -> per-split .rrf sidecars + the FACET header flag.
    b = rr.SplitSetBuilder(policy="tiered", byte_cap=4096, gram_size=3, name_prefix="corpus")
    for i in range(200):
        b.add_faceted(f"abc tok{i:04}", {"year": [str(2000 + i % 5)], "type": ["article"]})
    stats = b.build(str(tmp_path), manifest_name="index")
    assert stats.splits > 1

    manifest = (tmp_path / "index.rrss").read_bytes()
    assert manifest[:4] == b"RRSS"
    (flags,) = struct.unpack_from("<H", manifest, 6)
    assert flags & 0b10  # FLAG_FACET (bit 1) set
    # Each split that carried a faceted doc has an RRSF sidecar.
    assert (tmp_path / "corpus-s00000.rrf").read_bytes()[:4] == b"RRSF"


def test_term_splitset_builder_writes_rrti_splits(tmp_path):
    # The FST/term-bodied split set: .rrt split files and body_kind=1 in the manifest.
    b = rr.TermSplitSetBuilder(policy="tiered", byte_cap=600, name_prefix="corpus", language="english")
    for i in range(60):
        b.add(f"abc tok{i:04}")
    assert len(b) == 60
    stats = b.build(str(tmp_path), manifest_name="index")
    assert stats.splits > 1

    manifest = (tmp_path / "index.rrss").read_bytes()
    assert manifest[:4] == b"RRSS"
    assert manifest[9] == 1  # bodyKind = term (RRTI)
    # Split data files use the .rrt extension and carry the RRTI magic.
    assert (tmp_path / "corpus-s00000.rrt").read_bytes()[:4] == b"RRTI"


def test_splitset_writer_flush_and_resume(tmp_path):
    # Fresh writer: add two docs, flush -> (name, split bytes, manifest bytes).
    w = rr.SplitSetWriter(gram_size=3, name_prefix="corpus", policy="stable_key")
    assert w.add("abc hello") == 0
    assert w.add("abc world") == 1
    assert w.doc_count() == 2
    assert w.memtable_bytes() > 0
    assert w.flush() is not None

    name, split_bytes, manifest = None, None, None
    # Re-flush yields None (nothing pending); so capture from the first flush instead.
    w2 = rr.SplitSetWriter(gram_size=3, name_prefix="corpus", policy="stable_key")
    w2.add("abc hello")
    name, split_bytes, manifest = w2.flush()
    assert isinstance(name, str) and name.endswith(".rrs")
    assert isinstance(split_bytes, bytes) and split_bytes[:4] == b"RRSI"
    assert isinstance(manifest, bytes) and manifest[:4] == b"RRSS"
    assert w2.flush() is None  # memtable now empty

    # Resume over the manifest: ids continue, a new flush appends a delta.
    w3 = rr.SplitSetWriter.resume(manifest, gram_size=3, name_prefix="corpus")
    assert w3.add("abc again") == 1  # global id continues past the resumed split
    _name2, _split2, manifest2 = w3.flush()
    split_count, base_count = struct.unpack_from("<II", manifest2, 12)
    assert split_count == 2  # original delta + the new one


def test_splitset_writer_case_sensitive_flag_set_and_inherited():
    # Default folds case: manifest flag clear, delta split is RRSI v3.
    w = rr.SplitSetWriter(gram_size=3, name_prefix="corpus", policy="stable_key")
    w.add("Abc Hello")
    _n, split_bytes, manifest = w.flush()
    (flags,) = struct.unpack_from("<H", manifest, 6)
    (version,) = struct.unpack_from("<H", split_bytes, 4)
    assert flags & (1 << 4) == 0  # FLAG_CASE_SENSITIVE clear
    assert version == 3

    # case_sensitive=True: manifest flag set, delta split is the case-sensitive v4.
    cw = rr.SplitSetWriter(
        gram_size=3, name_prefix="corpus", policy="stable_key", case_sensitive=True
    )
    cw.add("Abc Hello")
    _n2, cs_split, cs_manifest = cw.flush()
    (cs_flags,) = struct.unpack_from("<H", cs_manifest, 6)
    (cs_version,) = struct.unpack_from("<H", cs_split, 4)
    assert cs_flags & (1 << 4) != 0
    assert cs_version == 4

    # Resume (without re-passing the flag) inherits case sensitivity from the
    # manifest, so the appended delta stays v4 and the flag persists.
    rw = rr.SplitSetWriter.resume(cs_manifest, gram_size=3, name_prefix="corpus")
    rw.add("Xyz World")
    _n3, resumed_split, resumed_manifest = rw.flush()
    (r_flags,) = struct.unpack_from("<H", resumed_manifest, 6)
    (r_version,) = struct.unpack_from("<H", resumed_split, 4)
    assert r_flags & (1 << 4) != 0, "resumed manifest lost the case-sensitive flag"
    assert r_version == 4, "resumed delta silently reverted to case-folded v3"


def test_builder_rejects_non_container_head_boundary():
    # A head_boundary off a 65536 container boundary would straddle a roaring
    # container; the ctor must reject it rather than build a misaligned sidecar.
    with pytest.raises(ValueError):
        rr.Builder(gram_size=3, head_boundary=65537)
    # A multiple is accepted.
    rr.Builder(gram_size=3, head_boundary=131072)


def test_splitset_writer_delete_then_compact(tmp_path):
    w = rr.SplitSetWriter(gram_size=3, name_prefix="corpus", policy="stable_key")
    w.add("abc one")
    w.add("abc two")
    name0, split0, _m0 = w.flush()
    w.delete(0)
    name1, split1, _m1 = w.flush()  # deletes-only flush carries a tombstone

    # Compact the two delta splits into one absolute-id split.
    cname, csplit, cmanifest, removed = w.compact([(name0, split0), (name1, split1)])
    assert csplit[:4] == b"RRSI"
    assert cmanifest[:4] == b"RRSS"
    assert set(removed) == {name0, name1}
    split_count, _base = struct.unpack_from("<II", cmanifest, 12)
    assert split_count == 1  # two deltas merged into one
