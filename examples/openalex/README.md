# OpenAlex search demo dataset

Build a static, range-fetchable [roaringrange](../../roaringrange) search index
over the [OpenAlex](https://openalex.org) Works corpus, then serve it as a
no-backend demo (same shape as the DeepLibby rebuild).

OpenAlex data is **CC0** (public domain). The bulk snapshot is hosted in the
public `openalex` S3 bucket and needs no AWS account (`--no-sign-request`).

## Pipeline

```
download.sh  ──▶  openalexbuild  ──▶  upload (S3/CloudFront)  ──▶  demo
 (subset .gz)     (.rrs/.rrf/.bin/.idx)
```

### 1. Download a subset

```bash
./download.sh                 # first 1 updated_date partition -> /tmp/openalex/works
PARTITIONS=3 ./download.sh     # first 3 partitions
DEST=/data/oa ./download.sh    # custom destination
```

Requires `awscli` v2 (no credentials). Each partition is a
`updated_date=YYYY-MM-DD/` folder of gzip JSON Lines files
(`<NNNN>_part_<NN>.gz`), one Work per line. See the script header for pulling
the **full** Works dump (~330 GB compressed / ~1.6 TB raw).

### 2. Build the index

The loader lives in the `rr-e2e` module (it uses the local `roaringrange` /
`roaringsearch` via `replace` directives).

```bash
cd /Users/efreeman/rr-e2e
go run ./openalexbuild \
  -in "/tmp/openalex/works/*/*.gz" \
  -rrs /tmp/openalex.rrs \
  -facets /tmp/openalex.rrf \
  -bin /tmp/openalex-records.bin \
  -idx /tmp/openalex-records.idx \
  -limit 2000000          # 0 = all matched works
```

Outputs:

| File                     | Contents                                                        |
|--------------------------|-----------------------------------------------------------------|
| `openalex.rrs`           | RRS2 trigram text index (range-fetchable head/tail postings)    |
| `openalex.rrf`           | RRSF facet sidecar: `year`, `type`, `oa`, `language`, `topic`   |
| `openalex-records.bin`   | Concatenated record JSON, in popularity-rank order              |
| `openalex-records.idx`   | Little-endian `uint64` offsets, length `numDocs+1`              |

**Ranking:** works are stable-sorted by `cited_by_count` descending, so doc ID
== popularity rank and the most-cited works sit at the head of every posting.

**Indexed text** per work = title + reconstructed abstract + author names +
venue. The abstract is rebuilt from `abstract_inverted_index` (capped ~2000
chars).

**Record JSON** shape (compact keys):

```json
{"id":"W2741809807","t":"<title>","a":"Author One; Author Two","y":2018,"v":"<venue>","c":1234}
```

(`a`, `v`, `y` are omitted when empty/zero; `c` = `cited_by_count`.)

### 3. Upload

Upload the four artifacts to the static origin (S3 bucket fronted by
CloudFront), alongside the demo UI. The reader fetches byte ranges out of
`openalex.rrs` / `openalex.rrf` and slices records out of the `.bin` using the
`.idx` offsets.

### 4. Demo

Point the roaringrange reader/demo at the uploaded `.rrs` + `.rrf` + record
store. Search resolves trigram postings via HTTP range requests; facets filter
by `year` / `type` / `oa` / `language` / `topic`; hits are de-referenced
against the record store by doc ID.

## OpenAlex schema reference

Snapshot layout (`s3://openalex/data/works/`):

```
manifest
updated_date=YYYY-MM-DD/<NNNN>_part_<NN>.gz   # gzip JSON Lines, one Work/line
```

Work fields consumed by `openalexbuild`:

| JSON field                              | Used as                          |
|-----------------------------------------|----------------------------------|
| `id`                                    | record id (kept the `W...` tail) |
| `display_name`                          | title (indexed + `t`)            |
| `abstract_inverted_index`               | abstract text (indexed)          |
| `authorships[].author.display_name`     | authors (indexed + `a`)          |
| `publication_year`                      | `y` + `year` facet               |
| `type`                                  | `type` facet                     |
| `open_access.oa_status`                 | `oa` facet                       |
| `language`                              | `language` facet                 |
| `primary_topic.display_name`            | `topic` facet (fallback below)   |
| `concepts[0].display_name`              | `topic` facet fallback           |
| `cited_by_count`                        | popularity rank + `c`            |
| `primary_location.source.display_name`  | venue (indexed + `v`)            |

Docs:
- Snapshot: <https://developers.openalex.org/download-all-data/openalex-snapshot>
- Format: <https://developers.openalex.org/download-all-data/snapshot-data-format>
- Work object: <https://developers.openalex.org/api-entities/works/work-object>
