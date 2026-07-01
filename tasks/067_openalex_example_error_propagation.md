# 067: fix(examples/openalex): stop swallowing build-pipeline errors

**Severity: MED (silently incomplete/corrupt 484M-doc builds that exit 0).** `examples/openalex/main.go` + `stream.go`. Line refs @ 849f9c2.

## Findings

1. **`main.go:164-189` (`loadWorks`) and `stream.go:291-303` (`streamLines`): `bufio.Scanner` errors never checked.** Neither checks `sc.Err()` after the loop; a line over the 16 MB buffer (`ErrTooLong`) or a mid-file gzip read error just ENDS that file's scan and the build continues -- a silently incomplete index that looks successful. Check `sc.Err()` and fail (or at minimum log-and-count) per file.
2. **`main.go:300-314` (`buildIndexAndStore`) and `stream.go:244-270` (`writeRecordsOrdered`): write/flush/close errors dropped.** `writeOff` discards `iw.Write` errors; `bw.Flush()`, `iw.Flush()`, `bin.Close()`, `recIdx.Close()` return values all ignored. Disk-full during the multi-GB record-store write yields a truncated store with exit code 0; every doc after the truncation renders the wrong record. Propagate all of them (Close errors matter on buffered/OS-cached writes).
3. **`main.go:337-338` and `stream.go:430-431`: nil-deref after ignored `os.Stat` error.** `fi, _ := os.Stat(...)` then `fi.Size()` panics if stat fails; `stream.go:127` already has the guarded pattern (`if fi != nil`) -- make the other two match.
4. Library-side footgun (fold in here or into 057): `records.go:36-79` `RecordWriter` writes its declared `count` up front (:50) but never enforces it -- no `Finish`/`Close` verifying `written == count`. An undercount leaves header-claimed records whose offset entries don't exist; readers of high ids get garbage at runtime instead of a build-time error. Add a closing check (API addition, no byte changes for correct writers).

## Acceptance

- Simulated failures (tiny scanner buffer, write-error-injecting io.Writer, ENOSPC-ish mock) each abort the build non-zero with a clear message.
- RecordWriter undercount returns an error at Finish/Close; exact-count path byte-identical to current output.
