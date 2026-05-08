/// Spike validation for Option B (large-value leaf-page compression).
///
/// Tests cover:
///   1. Correctness — round-trip get / insert / delete / iterate on compressed tables.
///   2. File-size reduction — compressed DB must be measurably smaller than uncompressed.
///   3. Overwrite semantics — old value returned on insert is the correct *decompressed* bytes.
///   4. Backward compatibility — a DB written without compression is readable normally (byte[1]==0
///      is treated as `CompressionAlgorithm::None`).
///
/// Run with:  cargo test --features compression --test compression_tests
#[cfg(feature = "compression")]
mod compression {
    use redb::{
        CompressionAlgorithm, Database, ReadableDatabase, ReadableTable, ReadableTableMetadata,
        TableDefinition, WriteTransaction,
    };
    use std::fs;
    use std::time::Instant;
    use tempfile::NamedTempFile;

    const TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("compressed");

    // ── helpers ──────────────────────────────────────────────────────────────────

    /// Returns a compressible JSON-like blob of `size` bytes (lots of repeated structure).
    fn json_blob(size: usize) -> Vec<u8> {
        let template = br#"{"id":1234567890,"name":"Alice Smith","email":"alice@example.com","active":true,"score":99.5,"tags":["rust","embedded","database"]}"#;
        let mut out = Vec::with_capacity(size);
        while out.len() < size {
            out.extend_from_slice(template);
        }
        out.truncate(size);
        out
    }

    fn open_compressed(path: &std::path::Path) -> Database {
        Database::builder()
            .create(path)
            .expect("create db")
    }

    // Insert `n` values of `value_size` bytes into `table` inside `txn`.
    fn fill_table(
        txn: &WriteTransaction,
        table_def: TableDefinition<u64, &[u8]>,
        n: u64,
        value_size: usize,
        compression: CompressionAlgorithm,
    ) {
        let blob = json_blob(value_size);
        let mut table = txn.open_table_with_compression(table_def, compression).unwrap();
        for i in 0..n {
            table.insert(&i, blob.as_slice()).unwrap();
        }
    }

    // ── correctness ──────────────────────────────────────────────────────────────

    #[test]
    fn round_trip_get() {
        let file = NamedTempFile::new().unwrap();
        let db = open_compressed(file.path());

        let blob = json_blob(16 * 1024); // 16 KB — well above 4 KB page size
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            table.insert(&42u64, blob.as_slice()).unwrap();
        }
        txn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let table = rtxn.open_table(TABLE).unwrap();
        let guard = table.get(&42u64).unwrap().expect("key should exist");
        assert_eq!(guard.value(), blob.as_slice(), "decompressed value must match original");
    }

    #[test]
    fn round_trip_small_value_uncompressed() {
        // Values that fit in a single page must NOT be compressed (LeafBuilder only compresses
        // when required_size > page_size).
        let file = NamedTempFile::new().unwrap();
        let db = open_compressed(file.path());

        let blob = b"tiny";
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            table.insert(&1u64, blob.as_ref()).unwrap();
        }
        txn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let table = rtxn.open_table(TABLE).unwrap();
        let guard = table.get(&1u64).unwrap().expect("key should exist");
        assert_eq!(guard.value(), blob.as_ref());
    }

    #[test]
    fn overwrite_returns_correct_old_value() {
        let file = NamedTempFile::new().unwrap();
        let db = open_compressed(file.path());

        let v1 = json_blob(16 * 1024);
        let v2 = json_blob(12 * 1024);

        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            table.insert(&1u64, v1.as_slice()).unwrap();
        }
        txn.commit().unwrap();

        // Overwrite: the returned old value must be the *decompressed* v1.
        let txn2 = db.begin_write().unwrap();
        {
            let mut table = txn2.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            let old = table.insert(&1u64, v2.as_slice()).unwrap();
            assert!(old.is_some(), "should return old value on overwrite");
            assert_eq!(old.unwrap().value(), v1.as_slice(), "old value must be decompressed v1");
        }
        txn2.commit().unwrap();

        // Final read must return v2.
        let rtxn = db.begin_read().unwrap();
        let table = rtxn.open_table(TABLE).unwrap();
        let guard = table.get(&1u64).unwrap().unwrap();
        assert_eq!(guard.value(), v2.as_slice());
    }

    #[test]
    fn delete_returns_correct_value() {
        let file = NamedTempFile::new().unwrap();
        let db = open_compressed(file.path());

        let blob = json_blob(16 * 1024);
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            table.insert(&99u64, blob.as_slice()).unwrap();
        }
        txn.commit().unwrap();

        let txn2 = db.begin_write().unwrap();
        {
            let mut table = txn2.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            let removed = table.remove(&99u64).unwrap();
            assert!(removed.is_some());
            assert_eq!(removed.unwrap().value(), blob.as_slice());
        }
        txn2.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let table = rtxn.open_table(TABLE).unwrap();
        assert!(table.get(&99u64).unwrap().is_none());
    }

    #[test]
    fn iter_returns_correct_values() {
        let file = NamedTempFile::new().unwrap();
        let db = open_compressed(file.path());
        let n = 20u64;
        let value_size = 16 * 1024;

        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            for i in 0..n {
                let blob = json_blob(value_size + i as usize); // slightly different per key
                table.insert(&i, blob.as_slice()).unwrap();
            }
        }
        txn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let table = rtxn.open_table(TABLE).unwrap();
        let mut count = 0u64;
        for entry in table.iter().unwrap() {
            let (k, v) = entry.unwrap();
            let expected = json_blob(value_size + k.value() as usize);
            assert_eq!(v.value(), expected.as_slice(), "iterator value mismatch at key {}", k.value());
            count += 1;
        }
        assert_eq!(count, n);
    }

    #[test]
    fn multiple_keys_mixed_sizes() {
        // Insert some small (uncompressed) and some large (compressed) values in the same table.
        let file = NamedTempFile::new().unwrap();
        let db = open_compressed(file.path());
        let large_blob = json_blob(32 * 1024);
        let small_blob = b"small";

        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            table.insert(&1u64, small_blob.as_ref()).unwrap();
            table.insert(&2u64, large_blob.as_slice()).unwrap();
            table.insert(&3u64, small_blob.as_ref()).unwrap();
            table.insert(&4u64, large_blob.as_slice()).unwrap();
        }
        txn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let table = rtxn.open_table(TABLE).unwrap();
        assert_eq!(table.get(&1u64).unwrap().unwrap().value(), small_blob.as_ref());
        assert_eq!(table.get(&2u64).unwrap().unwrap().value(), large_blob.as_slice());
        assert_eq!(table.get(&3u64).unwrap().unwrap().value(), small_blob.as_ref());
        assert_eq!(table.get(&4u64).unwrap().unwrap().value(), large_blob.as_slice());
    }

    // ── file-size reduction ───────────────────────────────────────────────────────

    #[test]
    fn file_size_is_smaller_with_compression() {
        const N: u64 = 200;
        const VALUE_SIZE: usize = 16 * 1024; // 16 KB — compressible JSON

        let compressed_file = NamedTempFile::new().unwrap();
        let uncompressed_file = NamedTempFile::new().unwrap();

        // Write compressed DB
        {
            let db = Database::builder().create(compressed_file.path()).unwrap();
            let txn = db.begin_write().unwrap();
            fill_table(&txn, TABLE, N, VALUE_SIZE, CompressionAlgorithm::Lz4);
            txn.commit().unwrap();
        }

        // Write uncompressed DB (same data)
        {
            let db = Database::builder().create(uncompressed_file.path()).unwrap();
            let txn = db.begin_write().unwrap();
            fill_table(&txn, TABLE, N, VALUE_SIZE, CompressionAlgorithm::None);
            txn.commit().unwrap();
        }

        let compressed_size = fs::metadata(compressed_file.path()).unwrap().len();
        let uncompressed_size = fs::metadata(uncompressed_file.path()).unwrap().len();

        eprintln!(
            "File sizes: compressed={} KB, uncompressed={} KB, ratio={:.2}x",
            compressed_size / 1024,
            uncompressed_size / 1024,
            uncompressed_size as f64 / compressed_size as f64,
        );

        assert!(
            compressed_size < uncompressed_size,
            "compressed ({compressed_size} B) must be smaller than uncompressed ({uncompressed_size} B)"
        );

        // Expect at least 2x reduction on compressible JSON (LZ4 on this data gives ~4–5x).
        let ratio = uncompressed_size as f64 / compressed_size as f64;
        assert!(
            ratio >= 2.0,
            "expected at least 2x compression, got {ratio:.2}x"
        );
    }

    // ── performance ───────────────────────────────────────────────────────────────

    #[test]
    fn write_throughput_compressed_vs_uncompressed() {
        const N: u64 = 500;
        const VALUE_SIZE: usize = 16 * 1024;

        let compressed_file = NamedTempFile::new().unwrap();
        let uncompressed_file = NamedTempFile::new().unwrap();
        let blob = json_blob(VALUE_SIZE);

        // Compressed write
        let t = Instant::now();
        {
            let db = Database::builder().create(compressed_file.path()).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
                for i in 0..N {
                    table.insert(&i, blob.as_slice()).unwrap();
                }
            }
            txn.commit().unwrap();
        }
        let compressed_write_ms = t.elapsed().as_millis();

        // Uncompressed write
        let t = Instant::now();
        {
            let db = Database::builder().create(uncompressed_file.path()).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::None).unwrap();
                for i in 0..N {
                    table.insert(&i, blob.as_slice()).unwrap();
                }
            }
            txn.commit().unwrap();
        }
        let uncompressed_write_ms = t.elapsed().as_millis();

        eprintln!(
            "Write: compressed={compressed_write_ms}ms, uncompressed={uncompressed_write_ms}ms"
        );

        // Compressed read
        let t = Instant::now();
        {
            let db = Database::builder().open(compressed_file.path()).unwrap();
            let rtxn = db.begin_read().unwrap();
            let table = rtxn.open_table(TABLE).unwrap();
            for i in 0..N {
                let _ = table.get(&i).unwrap().unwrap();
            }
        }
        let compressed_read_ms = t.elapsed().as_millis();

        // Uncompressed read
        let t = Instant::now();
        {
            let db = Database::builder().open(uncompressed_file.path()).unwrap();
            let rtxn = db.begin_read().unwrap();
            let table = rtxn.open_table(TABLE).unwrap();
            for i in 0..N {
                let _ = table.get(&i).unwrap().unwrap();
            }
        }
        let uncompressed_read_ms = t.elapsed().as_millis();

        eprintln!(
            "Read:  compressed={compressed_read_ms}ms, uncompressed={uncompressed_read_ms}ms"
        );

        // No hard assertion on timing (CI varies), but print ratios for inspection.
        // Compressed reads are expected to be slower due to decompression overhead.
    }

    // ── retain / extract_if on compressed tables ──────────────────────────────────

    #[test]
    fn retain_on_compressed_table() {
        let file = NamedTempFile::new().unwrap();
        let blob_a = json_blob(16 * 1024);
        let blob_b = json_blob(32 * 1024);
        let blob_c = json_blob(24 * 1024);

        let db = Database::builder().create(file.path()).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            table.insert(&1u64, blob_a.as_slice()).unwrap();
            table.insert(&2u64, blob_b.as_slice()).unwrap();
            table.insert(&3u64, blob_c.as_slice()).unwrap();
        }
        txn.commit().unwrap();

        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            table.retain(|k, v| {
                assert!(v[0] == b'{', "predicate received compressed bytes");
                k % 2 == 0
            }).unwrap();
        }
        txn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let table = rtxn.open_table(TABLE).unwrap();
        assert!(table.get(&1u64).unwrap().is_none());
        let val2 = table.get(&2u64).unwrap().expect("key 2 must survive");
        assert_eq!(val2.value(), blob_b.as_slice());
        assert!(table.get(&3u64).unwrap().is_none());
        assert_eq!(table.len().unwrap(), 1);
    }

    #[test]
    fn retain_preserves_all_on_compressed_table() {
        let file = NamedTempFile::new().unwrap();
        let blob = json_blob(16 * 1024);

        let db = Database::builder().create(file.path()).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            for i in 0..5u64 {
                table.insert(&i, blob.as_slice()).unwrap();
            }
        }
        txn.commit().unwrap();

        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            table.retain(|_k, v| {
                assert!(v[0] == b'{');
                true
            }).unwrap();
        }
        txn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let table = rtxn.open_table(TABLE).unwrap();
        assert_eq!(table.len().unwrap(), 5);
        for i in 0..5u64 {
            let guard = table.get(&i).unwrap().expect("all keys must survive");
            assert_eq!(guard.value(), blob.as_slice());
        }
    }

    #[test]
    fn extract_if_on_compressed_table() {
        let file = NamedTempFile::new().unwrap();
        let blob_a = json_blob(16 * 1024);
        let blob_b = json_blob(32 * 1024);

        let db = Database::builder().create(file.path()).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            table.insert(&10u64, blob_a.as_slice()).unwrap();
            table.insert(&20u64, blob_b.as_slice()).unwrap();
        }
        txn.commit().unwrap();

        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table_with_compression(TABLE, CompressionAlgorithm::Lz4).unwrap();
            let extracted: Vec<_> = table
                .extract_if(|k, v| {
                    assert!(v[0] == b'{', "predicate received compressed bytes");
                    k == 10
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(extracted.len(), 1);
            assert_eq!(extracted[0].0.value(), 10u64);
            assert_eq!(extracted[0].1.value(), blob_a.as_slice());
        }
        txn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let table = rtxn.open_table(TABLE).unwrap();
        assert!(table.get(&10u64).unwrap().is_none());
        let val = table.get(&20u64).unwrap().expect("key 20 must survive");
        assert_eq!(val.value(), blob_b.as_slice());
    }

    // ── backward compatibility ────────────────────────────────────────────────────

    #[test]
    fn uncompressed_db_readable_without_compression_flag() {
        let file = NamedTempFile::new().unwrap();
        let blob = json_blob(16 * 1024);

        // Write without compression
        {
            let db = Database::builder().create(file.path()).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut table = txn.open_table(TABLE).unwrap();
                table.insert(&7u64, blob.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }

        // Read without any knowledge of compression — must work (byte[1] == 0 → None algorithm)
        {
            let db = Database::builder().open(file.path()).unwrap();
            let rtxn = db.begin_read().unwrap();
            let table = rtxn.open_table(TABLE).unwrap();
            let guard = table.get(&7u64).unwrap().expect("key must exist");
            assert_eq!(guard.value(), blob.as_slice());
        }
    }
}
