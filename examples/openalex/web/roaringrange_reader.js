/* @ts-self-types="./roaringrange_reader.d.ts" */

/**
 * A range-fetchable [`Catalog`] exposed to JavaScript: one object bundling the
 * `RRS` index with an optional `RRSF` facet sidecar and `RRSR` record store, so
 * the whole "search → ranked IDs + records + facet counts" flow is one call.
 * Mirrors [`RrsIndex`]/[`RrsRecords`]; adopt it in place of wiring those three
 * together by hand.
 */
export class RrsCatalog {
    static __wrap(ptr) {
        const obj = Object.create(RrsCatalog.prototype);
        obj.__wbg_ptr = ptr;
        RrsCatalogFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        RrsCatalogFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_rrscatalog_free(ptr, 0);
    }
    /**
     * Returns the facet fields and their full-corpus category counts as a JSON
     * string `[{"field":"<name>","cats":[{"name":"<name>","count":<u32>},...]},...]`,
     * or `"[]"` when no facet sidecar is attached. Mirrors [`RrsIndex::facets_json`].
     * @returns {string}
     */
    facetsJson() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.rrscatalog_facetsJson(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * Number of n-grams in the index dictionary.
     * @returns {number}
     */
    ngramCount() {
        const ret = wasm.rrscatalog_ngramCount(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Boots a catalog over the index at `index_url` alone (header + sparse
     * dictionary). Attach the optional sidecars with [`RrsCatalog::open_facets`]
     * and [`RrsCatalog::open_records`]. Returns a `Promise<RrsCatalog>`.
     * @param {string} index_url
     * @returns {Promise<RrsCatalog>}
     */
    static open(index_url) {
        const ptr0 = passStringToWasm0(index_url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.rrscatalog_open(ptr0, len0);
        return ret;
    }
    /**
     * Boots the catalog with all three resources at once: the index at
     * `index_url`, the facet sidecar at `facets_url`, and the record store
     * (`records_idx_url` offset index + `records_bin_url` blob). Returns a
     * `Promise<RrsCatalog>`.
     * @param {string} index_url
     * @param {string} facets_url
     * @param {string} records_idx_url
     * @param {string} records_bin_url
     * @returns {Promise<RrsCatalog>}
     */
    static openAll(index_url, facets_url, records_idx_url, records_bin_url) {
        const ptr0 = passStringToWasm0(index_url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(facets_url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ptr2 = passStringToWasm0(records_idx_url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len2 = WASM_VECTOR_LEN;
        const ptr3 = passStringToWasm0(records_bin_url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len3 = WASM_VECTOR_LEN;
        const ret = wasm.rrscatalog_openAll(ptr0, len0, ptr1, len1, ptr2, len2, ptr3, len3);
        return ret;
    }
    /**
     * Opens the facet sidecar at `url` and attaches it, enabling filtered search
     * and facet counts.
     * @param {string} url
     * @returns {Promise<void>}
     */
    openFacets(url) {
        const ptr0 = passStringToWasm0(url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.rrscatalog_openFacets(this.__wbg_ptr, ptr0, len0);
        return ret;
    }
    /**
     * Opens the record store (`idx_url` offset index + `bin_url` record blob)
     * and attaches it, so [`RrsCatalog::search`] returns record bytes.
     * @param {string} idx_url
     * @param {string} bin_url
     * @returns {Promise<void>}
     */
    openRecords(idx_url, bin_url) {
        const ptr0 = passStringToWasm0(idx_url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(bin_url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.rrscatalog_openRecords(this.__wbg_ptr, ptr0, len0, ptr1, len1);
        return ret;
    }
    /**
     * Runs the full search flow and resolves to a JS object:
     * `{ ids: Uint32Array, records: Array<Uint8Array|null> | null,
     * facetCounts: Array<{field, cats:[{name, count}]}> | null }`.
     *
     * `filters_json` is a JSON array of `[field, category]` pairs (e.g.
     * `[["format","ebook"],["language","en"]]`); `null`, `""`, or `"[]"` means
     * no filter. Within a field categories OR, across fields they AND. The page
     * covers ranked doc IDs `[offset, offset+len)`; `max_missing` is the fuzzy
     * tolerance (0 = strict). `records`/`facetCounts` are `null` unless the
     * matching sidecar is attached.
     * @param {string} query
     * @param {number} offset
     * @param {number} len
     * @param {number} max_missing
     * @param {string | null} [filters_json]
     * @returns {Promise<any>}
     */
    search(query, offset, len, max_missing, filters_json) {
        const ptr0 = passStringToWasm0(query, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        var ptr1 = isLikeNone(filters_json) ? 0 : passStringToWasm0(filters_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        var len1 = WASM_VECTOR_LEN;
        const ret = wasm.rrscatalog_search(this.__wbg_ptr, ptr0, len0, offset, len, max_missing, ptr1, len1);
        return ret;
    }
}
if (Symbol.dispose) RrsCatalog.prototype[Symbol.dispose] = RrsCatalog.prototype.free;

/**
 * A stateful pagination cursor exposed to JavaScript.
 */
export class RrsCursor {
    static __wrap(ptr) {
        const obj = Object.create(RrsCursor.prototype);
        obj.__wbg_ptr = ptr;
        RrsCursorFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        RrsCursorFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_rrscursor_free(ptr, 0);
    }
    /**
     * Returns the search-filtered facet counts as a JSON string in the form
     * `[{"field":"<name>","cats":[{"name":"<name>","count":<n>},...]},...]` —
     * how many of this query's results fall in each category. `"[]"` when no
     * facet sidecar is open.
     * @returns {string}
     */
    facetCountsJson() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.rrscursor_facetCountsJson(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * Number of doc IDs materialized so far.
     * @returns {number}
     */
    loaded() {
        const ret = wasm.rrscursor_loaded(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Returns the next `n` doc IDs as a `Uint32Array`. Pages within the head
     * cost no fetches; crossing into the tail triggers one concurrent wave.
     * @param {number} n
     * @returns {Promise<Uint32Array>}
     */
    next(n) {
        const ret = wasm.rrscursor_next(this.__wbg_ptr, n);
        return ret;
    }
    /**
     * Random-access page: up to `limit` doc IDs starting at `offset`. Paging
     * backward never fetches; paging past the head fetches the tail once.
     * @param {number} offset
     * @param {number} limit
     * @returns {Promise<Uint32Array>}
     */
    page(offset, limit) {
        const ret = wasm.rrscursor_page(this.__wbg_ptr, offset, limit);
        return ret;
    }
}
if (Symbol.dispose) RrsCursor.prototype[Symbol.dispose] = RrsCursor.prototype.free;

/**
 * A range-fetchable RRS index exposed to JavaScript. Optionally carries an
 * opened facet sidecar (`RRSF`) used to filter queries.
 */
export class RrsIndex {
    static __wrap(ptr) {
        const obj = Object.create(RrsIndex.prototype);
        obj.__wbg_ptr = ptr;
        RrsIndexFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        RrsIndexFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_rrsindex_free(ptr, 0);
    }
    /**
     * Returns the facet fields and their categories as a JSON string in the form
     * `[{"field":"<name>","cats":[{"name":"<name>","count":<u32>},...]},...]`.
     * Yields `"[]"` when no sidecar is open. Counts are full-corpus and free
     * (served from the in-memory meta region).
     * @returns {string}
     */
    facetsJson() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.rrsindex_facetsJson(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * Number of n-grams in the index dictionary.
     * @returns {number}
     */
    ngramCount() {
        const ret = wasm.rrsindex_ngramCount(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Boots the index at `url`: fetches the header and sparse index. Returns a
     * `Promise<RrsIndex>` to JavaScript. Facets are not opened here; call
     * [`RrsIndex::open_facets`] afterward if a sidecar is available.
     * @param {string} url
     * @returns {Promise<RrsIndex>}
     */
    static open(url) {
        const ptr0 = passStringToWasm0(url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.rrsindex_open(ptr0, len0);
        return ret;
    }
    /**
     * Boots the optional facet sidecar at `url` and attaches it to this index,
     * enabling [`RrsIndex::facets_json`] and filtered search.
     * @param {string} url
     * @returns {Promise<void>}
     */
    openFacets(url) {
        const ptr0 = passStringToWasm0(url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.rrsindex_openFacets(this.__wbg_ptr, ptr0, len0);
        return ret;
    }
    /**
     * Returns up to `limit` matching doc IDs, most-popular first. Resolves to a
     * `Uint32Array` in JavaScript.
     * @param {string} query
     * @param {number} limit
     * @returns {Promise<Uint32Array>}
     */
    search(query, limit) {
        const ptr0 = passStringToWasm0(query, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.rrsindex_search(this.__wbg_ptr, ptr0, len0, limit);
        return ret;
    }
    /**
     * Opens a stateful pagination cursor for `query` (one head fetch wave up
     * front). Resolves to an `RrsCursor`.
     * @param {string} query
     * @param {number} max_missing
     * @returns {Promise<RrsCursor>}
     */
    searchCursor(query, max_missing) {
        const ptr0 = passStringToWasm0(query, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.rrsindex_searchCursor(this.__wbg_ptr, ptr0, len0, max_missing);
        return ret;
    }
    /**
     * Like [`RrsIndex::search_cursor`] but ANDs the selected facets into the
     * result. Each `filters` entry is `"field\tcategory"` (tab-separated);
     * within a field categories OR, across fields they AND. The filter is
     * applied only when a sidecar is open and `filters` is non-empty. Resolves
     * to an `RrsCursor`.
     * @param {string} query
     * @param {number} max_missing
     * @param {string[]} filters
     * @returns {Promise<RrsCursor>}
     */
    searchCursorFiltered(query, max_missing, filters) {
        const ptr0 = passStringToWasm0(query, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArrayJsValueToWasm0(filters, wasm.__wbindgen_malloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.rrsindex_searchCursorFiltered(this.__wbg_ptr, ptr0, len0, max_missing, ptr1, len1);
        return ret;
    }
}
if (Symbol.dispose) RrsIndex.prototype[Symbol.dispose] = RrsIndex.prototype.free;

/**
 * A range-fetchable `RRSR` record store exposed to JavaScript: maps a ranked
 * doc ID to its raw record bytes over HTTP Range. The offset index (`.idx`) and
 * the record blob (`.bin`) are each backed by their own [`WasmFetch`] URL.
 */
export class RrsRecords {
    static __wrap(ptr) {
        const obj = Object.create(RrsRecords.prototype);
        obj.__wbg_ptr = ptr;
        RrsRecordsFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        RrsRecordsFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_rrsrecords_free(ptr, 0);
    }
    /**
     * Raw record bytes for doc `id` as a `Uint8Array`, or `undefined` (a JS
     * `null`) when `id` is out of range. One Range read of the offset pair, one
     * of the record slice.
     * @param {number} id
     * @returns {Promise<any>}
     */
    get(id) {
        const ret = wasm.rrsrecords_get(this.__wbg_ptr, id);
        return ret;
    }
    /**
     * Raw record bytes for several doc IDs (a results page is the typical
     * input). Resolves to a JS `Array` aligned with `ids`: each element is a
     * `Uint8Array`, or `null` for an out-of-range doc ID.
     * @param {Uint32Array} ids
     * @returns {Promise<Array<any>>}
     */
    getMany(ids) {
        const ptr0 = passArray32ToWasm0(ids, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.rrsrecords_getMany(this.__wbg_ptr, ptr0, len0);
        return ret;
    }
    /**
     * Record bytes for doc `id` decoded as a UTF-8 string, or `undefined` (a JS
     * `null`) when `id` is out of range. Convenience for JSON/text records;
     * invalid UTF-8 is replaced lossily.
     * @param {number} id
     * @returns {Promise<any>}
     */
    getText(id) {
        const ret = wasm.rrsrecords_getText(this.__wbg_ptr, id);
        return ret;
    }
    /**
     * Whether the store holds no records.
     * @returns {boolean}
     */
    isEmpty() {
        const ret = wasm.rrsrecords_isEmpty(this.__wbg_ptr);
        return ret !== 0;
    }
    /**
     * Number of records (doc IDs `0..len`).
     * @returns {number}
     */
    len() {
        const ret = wasm.rrsrecords_len(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Boots the record store: reads and validates the 16-byte `RRSR` header of
     * the offset index at `idx_url`, with records served from the blob at
     * `bin_url`. Returns a `Promise<RrsRecords>` to JavaScript.
     * @param {string} idx_url
     * @param {string} bin_url
     * @returns {Promise<RrsRecords>}
     */
    static open(idx_url, bin_url) {
        const ptr0 = passStringToWasm0(idx_url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(bin_url, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.rrsrecords_open(ptr0, len0, ptr1, len1);
        return ret;
    }
}
if (Symbol.dispose) RrsRecords.prototype[Symbol.dispose] = RrsRecords.prototype.free;
function __wbg_get_imports() {
    const import0 = {
        __proto__: null,
        __wbg_Error_ef53bc310eb298a0: function(arg0, arg1) {
            const ret = Error(getStringFromWasm0(arg0, arg1));
            return ret;
        },
        __wbg___wbindgen_is_function_754e9f305ff6029e: function(arg0) {
            const ret = typeof(arg0) === 'function';
            return ret;
        },
        __wbg___wbindgen_is_undefined_67b456be8673d3d7: function(arg0) {
            const ret = arg0 === undefined;
            return ret;
        },
        __wbg___wbindgen_string_get_72bdf95d3ae505b1: function(arg0, arg1) {
            const obj = arg1;
            const ret = typeof(obj) === 'string' ? obj : undefined;
            var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            var len1 = WASM_VECTOR_LEN;
            getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
        },
        __wbg___wbindgen_throw_1506f2235d1bdba0: function(arg0, arg1) {
            throw new Error(getStringFromWasm0(arg0, arg1));
        },
        __wbg__wbg_cb_unref_61db23ac97f16c31: function(arg0) {
            arg0._wbg_cb_unref();
        },
        __wbg_arrayBuffer_05927079aabe6d46: function() { return handleError(function (arg0) {
            const ret = arg0.arrayBuffer();
            return ret;
        }, arguments); },
        __wbg_call_9c758de292015997: function() { return handleError(function (arg0, arg1, arg2) {
            const ret = arg0.call(arg1, arg2);
            return ret;
        }, arguments); },
        __wbg_get_2b48c7d0d006a781: function(arg0, arg1) {
            const ret = arg0[arg1 >>> 0];
            return ret;
        },
        __wbg_get_de6a0f7d4d18a304: function() { return handleError(function (arg0, arg1) {
            const ret = Reflect.get(arg0, arg1);
            return ret;
        }, arguments); },
        __wbg_get_unchecked_33f6e5c9e2f2d6b2: function(arg0, arg1) {
            const ret = arg0[arg1 >>> 0];
            return ret;
        },
        __wbg_instanceof_ArrayBuffer_8f49811467741499: function(arg0) {
            let result;
            try {
                result = arg0 instanceof ArrayBuffer;
            } catch (_) {
                result = false;
            }
            const ret = result;
            return ret;
        },
        __wbg_instanceof_Promise_d0db99486956c8e8: function(arg0) {
            let result;
            try {
                result = arg0 instanceof Promise;
            } catch (_) {
                result = false;
            }
            const ret = result;
            return ret;
        },
        __wbg_instanceof_Response_cb984bd66d7bd408: function(arg0) {
            let result;
            try {
                result = arg0 instanceof Response;
            } catch (_) {
                result = false;
            }
            const ret = result;
            return ret;
        },
        __wbg_isArray_67c2c9c4313f4448: function(arg0) {
            const ret = Array.isArray(arg0);
            return ret;
        },
        __wbg_length_4a591ecaa01354d9: function(arg0) {
            const ret = arg0.length;
            return ret;
        },
        __wbg_length_66f1a4b2e9026940: function(arg0) {
            const ret = arg0.length;
            return ret;
        },
        __wbg_new_578aeef4b6b94378: function(arg0) {
            const ret = new Uint8Array(arg0);
            return ret;
        },
        __wbg_new_ce1ab61c1c2b300d: function() {
            const ret = new Object();
            return ret;
        },
        __wbg_new_e436d06bc8e77460: function() { return handleError(function () {
            const ret = new Headers();
            return ret;
        }, arguments); },
        __wbg_new_from_slice_18fa1f71286d66b8: function(arg0, arg1) {
            const ret = new Uint8Array(getArrayU8FromWasm0(arg0, arg1));
            return ret;
        },
        __wbg_new_from_slice_47be4219028de35d: function(arg0, arg1) {
            const ret = new Uint32Array(getArrayU32FromWasm0(arg0, arg1));
            return ret;
        },
        __wbg_new_typed_bf31d18f92484486: function(arg0, arg1) {
            try {
                var state0 = {a: arg0, b: arg1};
                var cb0 = (arg0, arg1) => {
                    const a = state0.a;
                    state0.a = 0;
                    try {
                        return wasm_bindgen__convert__closures_____invoke__h54ade6753008af0f(a, state0.b, arg0, arg1);
                    } finally {
                        state0.a = a;
                    }
                };
                const ret = new Promise(cb0);
                return ret;
            } finally {
                state0.a = 0;
            }
        },
        __wbg_new_with_length_690552eb9e6aeac9: function(arg0) {
            const ret = new Array(arg0 >>> 0);
            return ret;
        },
        __wbg_new_with_str_and_init_bcd02b79a793d27f: function() { return handleError(function (arg0, arg1, arg2) {
            const ret = new Request(getStringFromWasm0(arg0, arg1), arg2);
            return ret;
        }, arguments); },
        __wbg_ok_fb13c30bc1893039: function(arg0) {
            const ret = arg0.ok;
            return ret;
        },
        __wbg_parse_03863847d06c4e89: function() { return handleError(function (arg0, arg1) {
            const ret = JSON.parse(getStringFromWasm0(arg0, arg1));
            return ret;
        }, arguments); },
        __wbg_prototypesetcall_3249fc62a0fafa30: function(arg0, arg1, arg2) {
            Uint8Array.prototype.set.call(getArrayU8FromWasm0(arg0, arg1), arg2);
        },
        __wbg_queueMicrotask_35c611f4a14830b2: function(arg0) {
            queueMicrotask(arg0);
        },
        __wbg_queueMicrotask_404ed0a58e0b63cc: function(arg0) {
            const ret = arg0.queueMicrotask;
            return ret;
        },
        __wbg_resolve_25a7e548d5881dca: function(arg0) {
            const ret = Promise.resolve(arg0);
            return ret;
        },
        __wbg_rrscatalog_new: function(arg0) {
            const ret = RrsCatalog.__wrap(arg0);
            return ret;
        },
        __wbg_rrscursor_new: function(arg0) {
            const ret = RrsCursor.__wrap(arg0);
            return ret;
        },
        __wbg_rrsindex_new: function(arg0) {
            const ret = RrsIndex.__wrap(arg0);
            return ret;
        },
        __wbg_rrsrecords_new: function(arg0) {
            const ret = RrsRecords.__wrap(arg0);
            return ret;
        },
        __wbg_set_25ef40a9aeff260d: function() { return handleError(function (arg0, arg1, arg2, arg3, arg4) {
            arg0.set(getStringFromWasm0(arg1, arg2), getStringFromWasm0(arg3, arg4));
        }, arguments); },
        __wbg_set_6e30c9374c26414c: function() { return handleError(function (arg0, arg1, arg2) {
            const ret = Reflect.set(arg0, arg1, arg2);
            return ret;
        }, arguments); },
        __wbg_set_dca99999bba88a9a: function(arg0, arg1, arg2) {
            arg0[arg1 >>> 0] = arg2;
        },
        __wbg_set_headers_7c1e39ece7826bec: function(arg0, arg1) {
            arg0.headers = arg1;
        },
        __wbg_set_method_7a6811dec7a4feff: function(arg0, arg1, arg2) {
            arg0.method = getStringFromWasm0(arg1, arg2);
        },
        __wbg_set_mode_c90e3667002857d4: function(arg0, arg1) {
            arg0.mode = __wbindgen_enum_RequestMode[arg1];
        },
        __wbg_static_accessor_GLOBAL_9d53f2689e622ca1: function() {
            const ret = typeof global === 'undefined' ? null : global;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_static_accessor_GLOBAL_THIS_a1a35cec07001a8a: function() {
            const ret = typeof globalThis === 'undefined' ? null : globalThis;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_static_accessor_SELF_4c59f6c7ea29a144: function() {
            const ret = typeof self === 'undefined' ? null : self;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_static_accessor_WINDOW_e70ae9f2eb052253: function() {
            const ret = typeof window === 'undefined' ? null : window;
            return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
        },
        __wbg_status_00549d55b78d949e: function(arg0) {
            const ret = arg0.status;
            return ret;
        },
        __wbg_stringify_8286df6dcc591521: function() { return handleError(function (arg0) {
            const ret = JSON.stringify(arg0);
            return ret;
        }, arguments); },
        __wbg_then_18f476d590e58992: function(arg0, arg1, arg2) {
            const ret = arg0.then(arg1, arg2);
            return ret;
        },
        __wbg_then_ac7b025999b52837: function(arg0, arg1) {
            const ret = arg0.then(arg1);
            return ret;
        },
        __wbindgen_cast_0000000000000001: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [Externref], shim_idx: 155, ret: Result(Unit), inner_ret: Some(Result(Unit)) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, wasm_bindgen__convert__closures_____invoke__h68218ae5a35c5694);
            return ret;
        },
        __wbindgen_cast_0000000000000002: function(arg0) {
            // Cast intrinsic for `F64 -> Externref`.
            const ret = arg0;
            return ret;
        },
        __wbindgen_cast_0000000000000003: function(arg0, arg1) {
            // Cast intrinsic for `Ref(String) -> Externref`.
            const ret = getStringFromWasm0(arg0, arg1);
            return ret;
        },
        __wbindgen_cast_0000000000000004: function(arg0, arg1) {
            var v0 = getArrayU32FromWasm0(arg0, arg1).slice();
            wasm.__wbindgen_free(arg0, arg1 * 4, 4);
            // Cast intrinsic for `Vector(U32) -> Externref`.
            const ret = v0;
            return ret;
        },
        __wbindgen_init_externref_table: function() {
            const table = wasm.__wbindgen_externrefs;
            const offset = table.grow(4);
            table.set(0, undefined);
            table.set(offset + 0, undefined);
            table.set(offset + 1, null);
            table.set(offset + 2, true);
            table.set(offset + 3, false);
        },
    };
    return {
        __proto__: null,
        "./roaringrange_reader_bg.js": import0,
    };
}

function wasm_bindgen__convert__closures_____invoke__h68218ae5a35c5694(arg0, arg1, arg2) {
    const ret = wasm.wasm_bindgen__convert__closures_____invoke__h68218ae5a35c5694(arg0, arg1, arg2);
    if (ret[1]) {
        throw takeFromExternrefTable0(ret[0]);
    }
}

function wasm_bindgen__convert__closures_____invoke__h54ade6753008af0f(arg0, arg1, arg2, arg3) {
    wasm.wasm_bindgen__convert__closures_____invoke__h54ade6753008af0f(arg0, arg1, arg2, arg3);
}


const __wbindgen_enum_RequestMode = ["same-origin", "no-cors", "cors", "navigate"];
const RrsCatalogFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_rrscatalog_free(ptr, 1));
const RrsCursorFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_rrscursor_free(ptr, 1));
const RrsIndexFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_rrsindex_free(ptr, 1));
const RrsRecordsFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_rrsrecords_free(ptr, 1));

function addToExternrefTable0(obj) {
    const idx = wasm.__externref_table_alloc();
    wasm.__wbindgen_externrefs.set(idx, obj);
    return idx;
}

const CLOSURE_DTORS = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(state => wasm.__wbindgen_destroy_closure(state.a, state.b));

function getArrayU32FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint32ArrayMemory0().subarray(ptr / 4, ptr / 4 + len);
}

function getArrayU8FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}

let cachedDataViewMemory0 = null;
function getDataViewMemory0() {
    if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer.detached === true || (cachedDataViewMemory0.buffer.detached === undefined && cachedDataViewMemory0.buffer !== wasm.memory.buffer)) {
        cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
    }
    return cachedDataViewMemory0;
}

function getStringFromWasm0(ptr, len) {
    return decodeText(ptr >>> 0, len);
}

let cachedUint32ArrayMemory0 = null;
function getUint32ArrayMemory0() {
    if (cachedUint32ArrayMemory0 === null || cachedUint32ArrayMemory0.byteLength === 0) {
        cachedUint32ArrayMemory0 = new Uint32Array(wasm.memory.buffer);
    }
    return cachedUint32ArrayMemory0;
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function handleError(f, args) {
    try {
        return f.apply(this, args);
    } catch (e) {
        const idx = addToExternrefTable0(e);
        wasm.__wbindgen_exn_store(idx);
    }
}

function isLikeNone(x) {
    return x === undefined || x === null;
}

function makeMutClosure(arg0, arg1, f) {
    const state = { a: arg0, b: arg1, cnt: 1 };
    const real = (...args) => {

        // First up with a closure we increment the internal reference
        // count. This ensures that the Rust closure environment won't
        // be deallocated while we're invoking it.
        state.cnt++;
        const a = state.a;
        state.a = 0;
        try {
            return f(a, state.b, ...args);
        } finally {
            state.a = a;
            real._wbg_cb_unref();
        }
    };
    real._wbg_cb_unref = () => {
        if (--state.cnt === 0) {
            wasm.__wbindgen_destroy_closure(state.a, state.b);
            state.a = 0;
            CLOSURE_DTORS.unregister(state);
        }
    };
    CLOSURE_DTORS.register(real, state, state);
    return real;
}

function passArray32ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 4, 4) >>> 0;
    getUint32ArrayMemory0().set(arg, ptr / 4);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passArrayJsValueToWasm0(array, malloc) {
    const ptr = malloc(array.length * 4, 4) >>> 0;
    for (let i = 0; i < array.length; i++) {
        const add = addToExternrefTable0(array[i]);
        getDataViewMemory0().setUint32(ptr + 4 * i, add, true);
    }
    WASM_VECTOR_LEN = array.length;
    return ptr;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

function takeFromExternrefTable0(idx) {
    const value = wasm.__wbindgen_externrefs.get(idx);
    wasm.__externref_table_dealloc(idx);
    return value;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(ptr, len) {
    numBytesDecoded += len;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
        cachedTextDecoder.decode();
        numBytesDecoded = len;
    }
    return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

let wasmModule, wasmInstance, wasm;
function __wbg_finalize_init(instance, module) {
    wasmInstance = instance;
    wasm = instance.exports;
    wasmModule = module;
    cachedDataViewMemory0 = null;
    cachedUint32ArrayMemory0 = null;
    cachedUint8ArrayMemory0 = null;
    wasm.__wbindgen_start();
    return wasm;
}

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = module.ok && expectedResponseType(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else { throw e; }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }

    function expectedResponseType(type) {
        switch (type) {
            case 'basic': case 'cors': case 'default': return true;
        }
        return false;
    }
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (module !== undefined) {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;


    if (module_or_path !== undefined) {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (module_or_path === undefined) {
        module_or_path = new URL('roaringrange_reader_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync, __wbg_init as default };
