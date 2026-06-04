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
    assert version == 1
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
