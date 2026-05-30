// Command openalexbuild builds the RRS2 text index, record store, and facet
// sidecar for an OpenAlex Works snapshot subset, assigning doc IDs in
// DESCENDING popularity (cited_by_count) so ascending doc ID == popularity rank
// and the head of every posting holds the most-cited works.
//
// It mirrors the buildranked pipeline:
//
//	Phase 1: stream the input .gz works files (gzip JSON Lines, one Work per
//	         line) -> items {indexed text, record JSON, popularity, facets}.
//	Phase 2: stable-sort items by popularity desc; doc ID = position.
//	Phase 3: build the roaringsearch index + record store in that order and
//	         transcode the index to RRS2.
//	Phase 4: write the RRSF facet sidecar.
//
// OpenAlex snapshot (CC0): the Works bulk dump lives at
//
//	s3://openalex/data/works/updated_date=YYYY-MM-DD/<NNNN>_part_<NN>.gz
//
// accessible with `aws s3 ... --no-sign-request`. Each .gz is gzip-compressed
// JSON Lines, one Work per line. See https://developers.openalex.org/
// download-all-data/snapshot-data-format and .../api-entities/works/work-object.
package main

import (
	"bufio"
	"compress/gzip"
	"encoding/binary"
	"encoding/json"
	"flag"
	"log"
	"os"
	"path/filepath"
	"sort"
	"strings"

	rr "github.com/freeeve/roaringrange"
	rs "github.com/freeeve/roaringsearch"
)

// abstractCharCap bounds the reconstructed abstract length so a few very long
// works do not bloat the indexed text.
const abstractCharCap = 2000

// work mirrors only the OpenAlex Work fields we need. Field names follow the
// snapshot/API schema; unused properties are ignored by the JSON decoder.
type work struct {
	ID          string           `json:"id"`           // "https://openalex.org/W..."
	DisplayName string           `json:"display_name"` // title
	AbstractIdx map[string][]int `json:"abstract_inverted_index"`
	Authorships []struct {
		Author struct {
			DisplayName string `json:"display_name"`
		} `json:"author"`
	} `json:"authorships"`
	PublicationYear int    `json:"publication_year"`
	Type            string `json:"type"`
	OpenAccess      struct {
		OAStatus string `json:"oa_status"`
	} `json:"open_access"`
	Language     string `json:"language"`
	PrimaryTopic struct {
		DisplayName string `json:"display_name"`
	} `json:"primary_topic"`
	Concepts []struct {
		DisplayName string `json:"display_name"`
	} `json:"concepts"`
	CitedByCount    int `json:"cited_by_count"`
	PrimaryLocation struct {
		Source struct {
			DisplayName string `json:"display_name"`
		} `json:"source"`
	} `json:"primary_location"`
}

// record is the stored JSON shape returned for a search hit. Compact keys keep
// the record store small: id, title, authors, year, venue, cited_by_count.
type record struct {
	ID string `json:"id"`
	T  string `json:"t"`
	A  string `json:"a,omitempty"`
	Y  int    `json:"y,omitempty"`
	V  string `json:"v,omitempty"`
	C  int    `json:"c"`
}

// item is one fully-prepared work ready for ranking and indexing.
type item struct {
	text  string // indexed text (title + abstract + authors + venue)
	rec   []byte // record JSON
	pop   int    // cited_by_count, used as the popularity rank key
	year  string // publication_year as string ("" when 0)
	typ   string // work type
	oa    string // open access status
	lang  string // language code
	topic string // primary topic / first concept name
}

func main() {
	in := flag.String("in", "/tmp/openalex/works/*/*.gz", "glob of input works .gz files")
	rrs := flag.String("rrs", "/tmp/openalex.rrs", "output RRS index")
	facetsPath := flag.String("facets", "/tmp/openalex.rrf", "output facet sidecar (RRSF)")
	binP := flag.String("bin", "/tmp/openalex-records.bin", "output record blob")
	idxP := flag.String("idx", "/tmp/openalex-records.idx", "output record offset index")
	ftsr := flag.String("ftsr", "/tmp/openalex.ftsr", "temp FTSR index")
	limit := flag.Int("limit", 0, "max works to load (0 = all)")
	stream := flag.Bool("stream", false, "two-pass streaming build (constant source memory; for corpora too large to load whole)")
	tmpRec := flag.String("tmprec", "/tmp/openalex-rec.tmp", "temp record file used by the streaming build")
	flag.Parse()

	files, err := filepath.Glob(*in)
	if err != nil {
		log.Fatal(err)
	}
	if len(files) == 0 {
		log.Fatalf("no input files matched %q", *in)
	}
	sort.Strings(files)
	log.Printf("matched %d input files", len(files))

	if *stream {
		if err := streamBuild(files, *rrs, *facetsPath, *binP, *idxP, *ftsr, *tmpRec, *limit); err != nil {
			log.Fatal(err)
		}
		return
	}

	items := loadWorks(files, *limit)
	log.Printf("loaded %d works", len(items))
	if len(items) == 0 {
		log.Fatal("no works loaded")
	}

	sort.SliceStable(items, func(i, j int) bool { return items[i].pop > items[j].pop })
	log.Printf("sorted by popularity; top cited_by_count=%d", items[0].pop)

	if err := buildIndexAndStore(items, *ftsr, *rrs, *binP, *idxP); err != nil {
		log.Fatal(err)
	}
	if err := buildFacets(items, *facetsPath); err != nil {
		log.Fatal(err)
	}
}

// loadWorks streams every input .gz file line by line, decodes each Work, and
// returns prepared items. It stops once limit items are loaded (limit <= 0
// loads all). Blank lines and undecodable or untitled records are skipped.
func loadWorks(files []string, limit int) []item {
	items := make([]item, 0, 1<<20)
	var seen int64
	for _, path := range files {
		f, err := os.Open(path)
		if err != nil {
			log.Printf("skip %s: %v", path, err)
			continue
		}
		gz, err := gzip.NewReader(f)
		if err != nil {
			log.Printf("skip %s: %v", path, err)
			f.Close()
			continue
		}
		sc := bufio.NewScanner(gz)
		sc.Buffer(make([]byte, 1<<20), 1<<24)
		for sc.Scan() {
			line := sc.Bytes()
			if len(line) == 0 {
				continue
			}
			var w work
			if json.Unmarshal(line, &w) != nil {
				continue
			}
			if w.DisplayName == "" {
				continue
			}
			items = append(items, prepare(&w))
			seen++
			if seen%1_000_000 == 0 {
				log.Printf("  loaded %dM works", seen/1_000_000)
			}
			if limit > 0 && len(items) >= limit {
				gz.Close()
				f.Close()
				return items
			}
		}
		gz.Close()
		f.Close()
	}
	return items
}

// prepare turns a decoded Work into an item: it builds the indexed text, the
// record JSON, and extracts the facet values.
func prepare(w *work) item {
	return item{
		text:  buildText(w),
		rec:   buildRecord(w),
		pop:   w.CitedByCount,
		year:  facetValue(w, 0),
		typ:   w.Type,
		oa:    w.OpenAccess.OAStatus,
		lang:  w.Language,
		topic: topicName(w),
	}
}

// trimOpenAlexID drops the "https://openalex.org/" prefix, keeping the "W..."
// tail used as the public id.
func trimOpenAlexID(id string) string {
	if i := strings.LastIndexByte(id, '/'); i >= 0 {
		return id[i+1:]
	}
	return id
}

// authorNames joins the authorship display names with "; ".
func authorNames(w *work) string {
	names := make([]string, 0, len(w.Authorships))
	for _, a := range w.Authorships {
		if a.Author.DisplayName != "" {
			names = append(names, a.Author.DisplayName)
		}
	}
	return strings.Join(names, "; ")
}

// topicName returns the primary topic display name, falling back to the first
// concept display name.
func topicName(w *work) string {
	if w.PrimaryTopic.DisplayName != "" {
		return w.PrimaryTopic.DisplayName
	}
	if len(w.Concepts) > 0 {
		return w.Concepts[0].DisplayName
	}
	return ""
}

// reconstructAbstract rebuilds abstract text from an OpenAlex
// abstract_inverted_index, which maps each word to the 0-based positions where
// it occurs. Words are placed at their positions and joined with single
// spaces; the result is capped at abstractCharCap bytes.
func reconstructAbstract(idx map[string][]int) string {
	if len(idx) == 0 {
		return ""
	}
	maxPos := -1
	for _, ps := range idx {
		for _, p := range ps {
			if p > maxPos {
				maxPos = p
			}
		}
	}
	if maxPos < 0 {
		return ""
	}
	words := make([]string, maxPos+1)
	for word, ps := range idx {
		for _, p := range ps {
			if p >= 0 && p <= maxPos {
				words[p] = word
			}
		}
	}
	abstract := strings.Join(words, " ")
	if len(abstract) > abstractCharCap {
		abstract = abstract[:abstractCharCap]
	}
	return abstract
}

// appendField appends " " + s to b when s is non-empty.
func appendField(b *strings.Builder, s string) {
	if s != "" {
		b.WriteByte(' ')
		b.WriteString(s)
	}
}

// buildIndexAndStore builds the roaringsearch index and the record store in
// popularity-ranked order (doc ID == rank), then transcodes the index to RRS2.
// The record store is a blob of concatenated record JSON plus a little-endian
// uint64 offset index of length len(items)+1, mirroring buildranked.
func buildIndexAndStore(items []item, ftsr, rrs, binP, idxP string) error {
	idx := rs.NewIndex(3)
	bin, err := os.Create(binP)
	if err != nil {
		return err
	}
	recIdx, err := os.Create(idxP)
	if err != nil {
		return err
	}
	bw := bufio.NewWriterSize(bin, 1<<20)
	iw := bufio.NewWriterSize(recIdx, 1<<20)
	var off uint64
	var u [8]byte
	writeOff := func(v uint64) {
		binary.LittleEndian.PutUint64(u[:], v)
		iw.Write(u[:])
	}
	writeOff(0)
	for rank, it := range items {
		idx.Add(uint32(rank), it.text)
		bw.Write(it.rec)
		off += uint64(len(it.rec))
		writeOff(off)
	}
	bw.Flush()
	iw.Flush()
	bin.Close()
	recIdx.Close()
	log.Printf("built index + record store (%d docs)", len(items))

	if err := idx.SaveToFile(ftsr); err != nil {
		return err
	}
	src, err := os.Open(ftsr)
	if err != nil {
		return err
	}
	dst, err := os.Create(rrs)
	if err != nil {
		src.Close()
		return err
	}
	if err := rr.Transcode(src, dst); err != nil {
		src.Close()
		dst.Close()
		return err
	}
	src.Close()
	dst.Close()
	os.Remove(ftsr)
	fi, _ := os.Stat(rrs)
	log.Printf("wrote RRS %s (%d bytes)", rrs, fi.Size())
	return nil
}

// buildFacets accumulates one doc-ID posting per (field, category) over the
// popularity-ranked items (doc ID == rank) and writes the RRSF sidecar. Fields
// are emitted in order: year, type, oa, language, topic. Category strings are
// interned per field so repeated values share a single bitmap.
func buildFacets(items []item, path string) error {
	fa := newFacetAccum()
	for rank, it := range items {
		doc := uint32(rank)
		fa.addValue(doc, 0, it.year)
		fa.addValue(doc, 1, it.typ)
		fa.addValue(doc, 2, it.oa)
		fa.addValue(doc, 3, it.lang)
		fa.addValue(doc, 4, it.topic)
	}
	return fa.write(path)
}
