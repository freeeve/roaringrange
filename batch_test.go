package roaringrange

import (
	"bytes"
	"fmt"
	"io"
	"testing"
)

// countReaderAt wraps an io.ReaderAt and counts ReadAt calls, so a test can assert
// how many ranged reads a batch API issues.
type countReaderAt struct {
	r     io.ReaderAt
	reads int
}

func (c *countReaderAt) ReadAt(p []byte, off int64) (int, error) {
	c.reads++
	return c.r.ReadAt(p, off)
}

// TestPostingsDedupsSharedDictBlock asserts Index.Postings reads a shared dict
// block once for keys that fall in it (not once per key) and returns postings
// identical to per-key Posting.
func TestPostingsDedupsSharedDictBlock(t *testing.T) {
	entries := []IndexEntry{
		{Key: 10, Posting: mustPosting(t, 0, 5)},
		{Key: 20, Posting: mustPosting(t, 1, 5)},
		{Key: 30, Posting: mustPosting(t, 2, 5)},
	}
	var buf bytes.Buffer
	// stride 8 > 3 entries -> every key lands in a single sparse block.
	if err := WriteIndex(&buf, 3, 8, entries); err != nil {
		t.Fatal(err)
	}
	cr := &countReaderAt{r: bytes.NewReader(buf.Bytes())}
	idx, err := Open(cr)
	if err != nil {
		t.Fatal(err)
	}
	cr.reads = 0 // exclude the boot (header + sparse) reads

	got, err := idx.Postings([]uint64{10, 20, 30})
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 3 {
		t.Fatalf("Postings returned %d entries, want 3", len(got))
	}
	// One dict-block read (all keys share it) + one read per present posting.
	if cr.reads != 4 {
		t.Errorf("Postings issued %d reads, want 4 (1 dict block + 3 postings)", cr.reads)
	}

	// Differential: each posting matches the single-key Posting path exactly.
	for _, k := range []uint64{10, 20, 30} {
		want, ok, err := idx.Posting(k)
		if err != nil || !ok {
			t.Fatalf("Posting(%d) failed: ok=%v err=%v", k, ok, err)
		}
		if !bytes.Equal(got[k], want) {
			t.Errorf("Postings[%d] != Posting(%d)", k, k)
		}
	}

	// An absent key is omitted, not errored.
	got2, err := idx.Postings([]uint64{10, 999})
	if err != nil {
		t.Fatal(err)
	}
	if _, present := got2[999]; present {
		t.Error("absent key 999 should be omitted from the result")
	}
	if len(got2) != 1 {
		t.Errorf("Postings returned %d entries, want 1 (only present key 10)", len(got2))
	}
}

// TestGetManyCoalescesNearContiguousRecords asserts RecordStore.GetMany fetches a
// page of rank-adjacent records in a handful of coalesced reads and returns bytes
// identical to per-id Get.
func TestGetManyCoalescesNearContiguousRecords(t *testing.T) {
	records := make([][]byte, 30)
	for i := range records {
		records[i] = fmt.Appendf(nil, "rec-%02d-payload", i)
	}
	var binBuf, idxBuf bytes.Buffer
	if err := WriteRecords(&binBuf, &idxBuf, records); err != nil {
		t.Fatal(err)
	}
	idxR := &countReaderAt{r: bytes.NewReader(idxBuf.Bytes())}
	binR := &countReaderAt{r: bytes.NewReader(binBuf.Bytes())}
	store, err := OpenRecordStore(idxR, binR)
	if err != nil {
		t.Fatal(err)
	}
	idxR.reads = 0 // exclude the boot header read
	binR.reads = 0

	ids := make([]uint32, 20)
	for i := range ids {
		ids[i] = uint32(i) // 0..19, contiguous (== rank-adjacent)
	}
	got, err := store.GetMany(ids)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 20 {
		t.Fatalf("GetMany returned %d records, want 20", len(got))
	}
	// Contiguous ids: the offset table coalesces to one read and the adjacent blobs
	// (no gaps here) to one read -- far below the ~2-per-id of the naive path.
	if total := idxR.reads + binR.reads; total > 4 {
		t.Errorf("GetMany issued %d reads for 20 near-contiguous records, want <= 4", total)
	}

	// Differential vs per-id Get.
	for _, id := range ids {
		want, ok, err := store.Get(id)
		if err != nil || !ok {
			t.Fatalf("Get(%d) failed: ok=%v err=%v", id, ok, err)
		}
		if !bytes.Equal(got[id], want) {
			t.Errorf("GetMany[%d]=%q != Get(%d)=%q", id, got[id], id, want)
		}
	}

	// Out-of-range ids are omitted.
	got2, err := store.GetMany([]uint32{0, 999})
	if err != nil {
		t.Fatal(err)
	}
	if _, present := got2[999]; present {
		t.Error("out-of-range id 999 should be omitted from the result")
	}
	if len(got2) != 1 {
		t.Errorf("GetMany returned %d records, want 1 (only in-range id 0)", len(got2))
	}
}
