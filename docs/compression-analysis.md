# redb Value Compression: Research & Design Analysis

## Executive Summary

redb's data footprint is significantly larger than competing embedded databases—up to 4x RocksDB and 2.4x LMDB at benchmark scale. The primary driver is the absence of compression. This document analyses the root causes, surveys how forks and adjacent projects have approached the problem, assesses why compression is non-trivial in redb's architecture, and proposes concrete design options.

---

## 1. The Data Footprint Problem

From the current benchmark suite (Ryzen 9950X3D, 1M random key-value pairs):

| Database    | Uncompacted | Compacted    |
|-------------|-------------|--------------|
| redb        | 4.00 GiB    | 1.69 GiB     |
| lmdb        | 2.61 GiB    | 1.26 GiB     |
| rocksdb     | **893 MiB** | **454 MiB**  |
| sled        | 2.13 GiB    | N/A          |
| fjall       | 1.00 GiB    | 1.00 GiB     |
| sqlite      | 1.09 GiB    | 556 MiB      |

The compacted comparison is the most honest signal—dead page waste has been removed. redb compacted (1.69 GiB) vs RocksDB compacted (454 MiB) is a **3.7x difference**, almost entirely explained by RocksDB using LZ4 compression by default.

The uncompacted-to-compacted ratio for redb (4.00 → 1.69 GiB = **58% dead pages at peak**) is a secondary problem driven by copy-on-write accumulation and buddy allocator fragmentation—distinct from compression.

---

## 2. redb Architecture: What Makes Compression Hard

Understanding why compression isn't a simple bolt-on requires understanding redb's storage model.

### 2.1 Copy-on-Write B-tree on a Buddy Allocator

All data lives in a tree of fixed-size pages. Pages are allocated by a buddy allocator that issues power-of-2 multiples of the base page size (default 4KB). Large values that don't fit in a 4KB leaf page are stored in a single higher-order leaf page (8KB, 16KB, 32KB, ...). There are no "overflow" chains—one value, one contiguous allocation.

### 2.2 mmap-Based Zero-Copy Reads

Pages are read via mmap. The `Page` trait exposes raw `&[u8]` slices into the mapped memory. Deserialization is zero-copy—`from_bytes` is typically a cast or a slice borrow. Any compression scheme that requires decompression before reading breaks this invariant.

### 2.3 Page-Fill-Based B-tree Balancing

Leaf nodes split when their backing page is full. Branch nodes rebalance based on child count. Both heuristics operate on raw byte sizes. If pages were compressed, the split logic would not know the actual logical capacity of a page, leading either to premature splits (wasted pages) or missed splits (oversized pages). The maintainer identified this as a primary blocker in GitHub issue #301.

### 2.4 Checksum Coverage

redb uses a Merkle tree of XXH3_128 checksums. The root checksum covers page content used to detect partial commits. If pages are compressed before checksumming, the checksum must cover compressed bytes—otherwise crash recovery verification is over data that doesn't exist on disk. This requires care but is solvable.

### 2.5 Deferred Checksums and Two Commit Strategies

Checksums are computed lazily (DEFERRED marker, finalized at commit time). The 1PC+C commit path writes all data, then fsync, then flips the primary bit. Compression that changes page sizes would invalidate this if the allocator state diverges from what's on disk.

---

## 3. Fork & Ecosystem Survey

### 3.1 redb-turbo (russellromney/redb-turbo)

The only significant fork to implement compression. Active as of early 2026. The description: *"redb fork with AES-256-GCM page encryption and zstd compression"*.

**Implementation approach: storage-backend-level page compression**

Compression is hooked into `cached_file.rs` at the read/write boundary, intercepting every page I/O. The on-disk format for a compressed page:

```
[magic: 2B "ZS"][compressed_len: 4B][orig_len: 4B][compressed_data...][zero padding to page_size]
```

If compression doesn't reduce size, the page is stored uncompressed (detected on read by magic bytes `UC` instead of `ZS`). Header pages (offset 0) are skipped.

**API:**
```rust
Database::builder()
    .set_page_compression(ZstdPageCompression::new(true))
    .create(path)
```

**Critical limitation**: Compressed data is padded back to the original page size on disk. The buddy allocator still allocates and tracks pages in fixed units. The B-tree split logic is unchanged. **This approach does not reduce file size**—it reduces the information density of each page without reducing the number of pages.

What it does provide:
- Reduced disk I/O bandwidth (useful on slow/remote storage)
- A foundation for encryption (compress-then-encrypt achieves better ciphertext entropy)
- Proof that the hook point in `cached_file.rs` is viable with minimal invasiveness (~233 lines of changes to that file)

The fork adds 28 commits on top of a somewhat older upstream base. The encryption implementation (AES-256-GCM, nonce per page, 28 bytes of overhead per page) is well-structured and the combined compress+encrypt pipeline works correctly.

**Verdict**: Proven approach for the storage-backend layer. Not a solution to the footprint problem.

### 3.2 canopydb (arthurprs/canopydb)

A separate Rust B-tree embedded database with a different architecture. Relevant because it solves a similar problem more effectively.

**Approach: overflow-value compression with threshold gating**

canopydb distinguishes between in-node values and "overflow" values (large values stored outside the B-tree node in dedicated pages). Compression applies only to overflow pages, controlled by:

```rust
pub compress_overflow_values: Option<u32>  // threshold in bytes, e.g. Some(12 * 1024)
```

Key design decisions:
- **LZ4** is used (not zstd)—prioritises decompression speed since reads are more frequent than writes
- Compression happens **at checkpoint time**, not at insert time. A value mutated many times between checkpoints is only compressed once
- Decompressed values are cached in the page cache **in decompressed form**. Frequently accessed values are only decompressed once regardless of how many threads read them
- If compression doesn't reduce size by enough to fit in a smaller allocation, the value is stored uncompressed
- The B-tree node structure is unchanged—only the large out-of-band pages are affected

The `CompressedPageHeader` struct tracks `compressed_len` and `uncompressed_len`. The page allocation uses the compressed size to determine the actual on-disk footprint.

**This approach actually reduces file size** because the allocation order is chosen based on compressed size.

**Verdict**: The highest-signal design in the ecosystem. Applicable to redb with adaptation.

### 3.3 Upstream Issues #301 and #656

Both were closed by the maintainer. The key exchange in #301:

> "redb needs to balance its btree, and it does that based on page fill, so it splits nodes in the btree once their backing page has become full. However, with page compression that heuristic becomes very complex. Either the data needs to be compressed to check its size before determining when to split, or pages will end up with lots of wasted space because nodes will be split and then the data compressed leading to a mostly empty page."

And the suggested workaround:

> "I think compressing the individual values might work okay for you, if you pretrain a shared dictionary and then use that to compress/decompress each value."

This confirms: the maintainer views page-level compression as a design conflict, but per-value compression with a shared dictionary as viable.

### 3.4 Other Forks

- **pragmaxim/redb**: One commit, trivial change. Not relevant.
- **Kerollmops/redb**: Zero diff from upstream. Not relevant.
- **0x676e67/redb-32bit**: Port to 32-bit targets. Not relevant to compression.

---

## 4. Design Options

### Option A: Storage-Backend-Level Compression (redb-turbo approach)

**Description**: Hook into `PagedCachedFile` (or a new `StorageBackend` wrapper) to compress/decompress full pages at I/O boundary. Compressed data padded to page size on disk.

**How it works**:
- Add optional `PageCompression` trait to `Builder`
- `cached_file.rs` calls compress on write, decompress on read
- Header and small pages can be skipped (offset threshold)
- Compression fallback: if compressed >= original, store raw with a different magic marker

**Space savings**: None. The physical page count and file size are unchanged.

**What it does provide**:
- Reduced disk read/write bandwidth (relevant for spinning disks or NFS)
- Pairs well with encryption (encrypt compressed data for better entropy)
- Minimal invasiveness—~250 lines in `cached_file.rs`, no B-tree changes

**Complexity**: Low. Already proven by redb-turbo.

**Risks**:
- Breaks zero-copy read semantics (pages must be decompressed into a buffer before use)
- Adds per-page decompression overhead on every read (even cached reads go through cached_file)
- Mismatch between physical fill and logical fill confuses future optimisation work

**Verdict**: Only worthwhile if encryption is also a goal. Not a solution to the footprint problem.

---

### Option B: Large-Value (Higher-Order Page) Compression

**Description**: When writing a leaf page at order > 0 (i.e., the page holds a single large value), attempt to compress the value portion. If the compressed value fits in a lower-order page, allocate that smaller page and store the leaf with a `compressed` flag.

**How it works**:

1. **Detection point**: `LeafBuilder` / `RawLeafBuilder` (in `btree_base.rs`) already knows the key and value sizes. When constructing a leaf page, if the required `page_order > 0`, attempt compression.

2. **Compression decision**: Compress value bytes. If `compressed_size < threshold_for_lower_order`, use the smaller order allocation.

   ```
   Order 0: 4KB  → value up to ~3.9KB raw
   Order 1: 8KB  → value up to ~7.9KB raw; if compressed fits in 4KB, use order 0
   Order 2: 16KB → value up to ~15.9KB raw; if compressed fits in 8KB, use order 1, etc.
   ```

3. **On-disk format**: Reuse the existing reserved byte in the leaf page header to signal `value_compressed`. The actual bytes stored in the value slot are the compressed bytes, and the `value_end` offsets reflect the compressed length. On read, the `Value::from_bytes` path decompresses before returning.

4. **B-tree balancing**: Unchanged. Large values already have the `single_large_value` fast-path in `btree_mutator.rs` (lines 232–235, 770–773) that skips rebalancing for pages with a single oversized entry. This means large-value pages are already treated as a special case—we can gate compression on the same condition.

5. **Cache interaction**: The LRU write cache in `cached_file.rs` holds `Arc<[u8]>` slices. For compressed large-value pages, the cached slice holds compressed bytes. Decompression happens in `Value::from_bytes`, not in the page layer. This preserves the zero-copy model for small-value pages.

**Space savings**: Significant for large values. A 12KB JSON document compressing 3:1 to 4KB goes from a 16KB allocation (order 2) to a 4KB allocation (order 0)—**75% space saving for that value**.

**Complexity**: Moderate.
- `LeafBuilder`: add try-compress path when order > 0
- `LeafAccessor::value`: check compressed flag, decompress if set
- `MutInPlaceValue` / `insert_reserve`: cannot be used on compressed values (or decompresses on access, recompresses on drop—complex)
- The existing `fixed_width` value optimisation (no `value_end` offsets) would be disabled for compressed leaves

**Risks**:
- CPU overhead on writes (compression) and reads (decompression) for large values
- Compression is not always beneficial—incompressible binary data (images, already-compressed formats) will incur overhead with no savings. Need a minimum size threshold and a "compressibility check" (if compressed >= 90% of original, skip)
- `MutInPlaceValue::insert_reserve` breaks for compressed values—need to either decompress/recompress on access or disallow for compressed tables

**Algorithm choice**:
- **LZ4** (`lz4_flex` crate, pure Rust): fastest decompression (~5 GB/s), moderate ratio (~2:1 on JSON). Best for read-heavy workloads.
- **zstd** (`zstd` crate, level 3): slower (~500 MB/s decompress), better ratio (~3:1 on JSON). Best for write-once / read-infrequent archives.
- Recommendation: make the algorithm pluggable at table definition time, default to LZ4.

**Verdict**: The highest-value option. Solves the real problem (disk footprint) for large values without touching B-tree balancing. Moderate implementation effort.

---

### Option C: Per-Value Compression with Shared Dictionary

**Description**: Wrap compression at the `Value` trait level. Users store pre-compressed bytes; optionally, redb maintains a shared zstd dictionary in a system table.

**Variant C1 — User-Side (no redb changes)**:
Users store compressed values themselves using `zstd::encode_all(value, level)` before insert and `zstd::decode_all(bytes)` after get. The maintainer already suggested this. It works today with zero changes to redb.

Limitation: No cross-value dictionary compression. Each value compressed independently. Small values get poor ratios.

**Variant C2 — Library Wrapper `CompressedValue<V>`**:
A newtype wrapper in the redb crate that transparently compresses/decompresses:

```rust
pub struct CompressedValue<V: Value>(PhantomData<V>);

impl<V: Value> Value for CompressedValue<V> {
    fn from_bytes<'a>(data: &'a [u8]) -> V::SelfType<'a> {
        let decompressed = zstd::decode_all(data).unwrap();
        V::from_bytes(&decompressed)  // lifetime issue: decompressed is owned
    }
    fn as_bytes(value: ...) -> Vec<u8> {
        zstd::encode_all(V::as_bytes(value).as_ref(), 3).unwrap()
    }
}
```

This has a **lifetime problem**: `from_bytes` must return a type with lifetime `'a` tied to the input slice, but decompressed data is owned. Zero-copy is impossible for compressed values. The return type would need to be `Vec<u8>` or similar owned type, which is allowed (`SelfType<'a> = Vec<u8>`) but loses zero-copy.

**Variant C3 — Shared Dictionary in System Table**:
A zstd dictionary trained on representative data is stored in a system table entry. On write, values are compressed with the dictionary. On read, values are decompressed with the cached dictionary. The dictionary is loaded once at database open.

Benefits: Cross-value compression benefits. A batch of similar JSON documents can achieve 5-10x compression ratios with a good dictionary vs. 2-3x without.

Limitations:
- Dictionary must be trained offline on representative data (or gathered automatically during writes—complex)
- Dictionary is fixed at table creation; changing it requires rewriting all values
- Still has the same lifetime issue as C2 for the `Value` trait

**Verdict**: C1 (user-side) is available today and appropriate for many workloads. C2 as a library wrapper is useful but requires careful API design around the lifetime constraint. C3 offers the best compression ratios but significant complexity. None of these address small-value compression within shared leaf pages.

---

### Option D: B-tree Leaf Page Compression (RocksDB/LevelDB approach)

**Description**: Compress entire leaf pages before writing. Adjust the split heuristic to target a compressed fill ratio rather than raw fill.

**How it works**:
- When a leaf page is "full" (before the current split), compress it and check if the compressed size exceeds a threshold (e.g., 50% of page size)
- If not, continue adding entries
- Decompress on read into a userspace cache

**Space savings**: Maximum. Effective for many small values with shared structure (repeated JSON keys, repeated prefixes).

**Complexity**: Very high.
- The B-tree split heuristic must be redesigned around compressed fill
- Every page read requires either a userspace cache lookup or a decompression call
- The mmap-based zero-copy model is fundamentally incompatible—requires a parallel userspace page cache
- Page ordering guarantees (branch pages, leaf pages, consistent B-tree view) become complicated when compressed sizes vary
- The `PageNumber.page_order` (buddy allocator order) must track compressed size, not logical size

The maintainer explicitly rejected this path: the interaction between page-fill-based splitting and compression ratios creates either premature splits (wasted space) or insufficient splits (page overflow). Making the split heuristic compression-aware requires predicting compression ratios before writing, which requires actually compressing the data in a trial run—expensive.

**Verdict**: Architecturally incompatible with redb's current design. Would require replacing the B-tree layer with a compression-aware design (essentially rebuilding toward an LSM-tree architecture). Not recommended.

---

## 5. Comparison Matrix

| Option | File Size Reduction | Small Value Benefit | Large Value Benefit | Implementation Effort | Breaks Zero-Copy | B-tree Changes |
|--------|--------------------|--------------------|--------------------|-----------------------|------------------|----------------|
| A (storage-backend) | None | None | None | Low | Yes (for all reads) | No |
| B (large-value) | High | No | Yes | Moderate | No (for small values) | No |
| C1 (user-side) | High | Limited | Yes | Zero | No | No |
| C2 (Value wrapper) | High | Limited | Yes | Low | Yes (forced owned) | No |
| C3 (shared dict) | Very High | Limited | Yes | High | Yes | No |
| D (page-level) | Very High | Yes | Yes | Very High | Yes (all reads) | Yes (fundamental) |

---

## 6. Recommended Path

### Short term: Option B

Implement large-value compression at the leaf-page level for pages at order > 0. This is the best trade-off of impact vs. complexity:

1. Add a `CompressionAlgorithm` enum (None, Lz4, Zstd) to `TableDefinition` or `Builder`
2. In `LeafBuilder`, when building a page at order > 0, attempt compression using the configured algorithm
3. Store a `compressed` flag in the currently-reserved byte of the leaf page header
4. In `LeafAccessor`, detect the flag and decompress value bytes before returning
5. Keep a minimum size threshold (e.g., 4KB) and a minimum ratio improvement (e.g., must compress to 75% or less to be worth it)
6. Disable `MutInPlaceValue` / `insert_reserve` for tables with compression enabled

This targets the exact scenario where redb is most disadvantaged: large, compressible values (JSON blobs, serialised structs, text). It does not require any B-tree structural changes, does not affect small-value performance, and doesn't require a userspace page cache.

### Medium term: Option C2 + C3

A `CompressedValue<V>` wrapper in the library, plus optional shared dictionary stored in a system table, enables user opt-in compression for any value type without requiring file format changes. This addresses small values at the cost of losing zero-copy for those tables.

### Not recommended

Option A (storage-backend compression) is not recommended as a standalone feature—it doesn't reduce file size and adds decompression overhead on every page read. If encryption support is added in the future, Option A's implementation approach (from redb-turbo) is the right foundation for the encrypt-at-rest layer, and compression-before-encryption would be a natural companion.

Option D (full page-level B-tree compression) requires architectural changes incompatible with redb's design goals and should not be pursued.

---

## 7. Key Constraints for Any Implementation

- **File format stability**: redb v3 format is declared stable. Any compression feature must either (a) be addable as a new file format version with a clear upgrade path, or (b) be fully backwards-compatible (compression as a table-level flag stored in the table tree metadata, with fallback to uncompressed read if flag is absent)
- **Zero-copy for small values**: The `Value::from_bytes` returning `&'a [u8]` slices must continue to work for uncompressed tables. Compression should be opt-in, not default
- **Checksum integrity**: The checksum Merkle tree must cover the on-disk bytes (compressed or uncompressed, consistently). Checksums must be computed after compression if compression is page-level
- **MVCC correctness**: Copy-on-write pages that get compressed are still subject to the same MVCC rules—a compressed page and its uncompressed predecessor are both valid until the transaction tracker releases the old version

---

## Sources

- GitHub Issues: [#301 Compression support](https://github.com/cberner/redb/issues/301), [#656 Add Compression Support](https://github.com/cberner/redb/issues/656), [#1098 Storage Size Differences](https://github.com/cberner/redb/issues/1098)
- Fork: [russellromney/redb-turbo](https://github.com/russellromney/redb-turbo) — page-level zstd + AES-256-GCM
- Adjacent project: [arthurprs/canopydb](https://github.com/arthurprs/canopydb) — LZ4 overflow-value compression
