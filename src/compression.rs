/// Compression algorithm to use for large values stored in a table.
///
/// When compression is enabled, values whose serialised size exceeds one base page (≥ 4 KiB by
/// default) are compressed before writing. If the compressed bytes fit into a smaller buddy-
/// allocator order, fewer pages are allocated on disk, directly reducing database file size.
///
/// Values that are already small (single-page leaf), or that compress poorly (ratio < 25%),
/// are stored uncompressed without extra overhead.
///
/// # On-disk representation
///
/// The compression algorithm is encoded in byte `[1]` of the leaf page header (previously
/// reserved/padding). Reading a page with an unrecognised compression byte is treated as
/// no compression for forward-compatibility. Existing databases (byte `[1]` == 0) are
/// unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompressionAlgorithm {
    /// No compression (default). Values are stored verbatim.
    #[default]
    None,
    /// LZ4 compression via [`lz4_flex`](https://crates.io/crates/lz4_flex).
    ///
    /// Fastest decompression (~5 GB/s), moderate compression ratio (~2:1 on JSON/text).
    /// Best choice for read-heavy workloads.
    ///
    /// Requires the `compression` Cargo feature.
    #[cfg(feature = "compression")]
    Lz4,
}

// ── Flag bytes stored in leaf page byte[1] ────────────────────────────────────

pub(crate) const COMPRESSION_NONE: u8 = 0x00;
pub(crate) const COMPRESSION_LZ4: u8 = 0x01;
/// Mask covering the compression bits in byte[1] (low nibble).
pub(crate) const COMPRESSION_MASK: u8 = 0x0F;

/// Minimum compression ratio required to actually store data compressed.
/// Value must compress to ≤ 75 % of original size, otherwise it is stored raw.
const MIN_RATIO_NUMERATOR: usize = 3;
const MIN_RATIO_DENOMINATOR: usize = 4;

impl CompressionAlgorithm {
    /// Returns the flag byte written into leaf page `byte[1]`.
    pub(crate) fn flag_byte(self) -> u8 {
        match self {
            CompressionAlgorithm::None => COMPRESSION_NONE,
            #[cfg(feature = "compression")]
            CompressionAlgorithm::Lz4 => COMPRESSION_LZ4,
        }
    }

    /// Decodes a compression algorithm from the flag nibble in leaf page `byte[1]`.
    /// Unknown values are treated as `None` for forward-compatibility.
    pub(crate) fn from_flag_byte(byte: u8) -> Self {
        match byte & COMPRESSION_MASK {
            COMPRESSION_NONE => CompressionAlgorithm::None,
            #[cfg(feature = "compression")]
            COMPRESSION_LZ4 => CompressionAlgorithm::Lz4,
            // Unknown algorithm – do not decompress; caller will get raw bytes.
            _ => CompressionAlgorithm::None,
        }
    }

    /// Returns `true` if this is a compressing algorithm (i.e. not `None`).
    pub(crate) fn is_active(self) -> bool {
        self != CompressionAlgorithm::None
    }

    /// Compress `data`, returning `Some(compressed_bytes)` only when compression saves at
    /// least 25 % of space. Returns `None` to indicate the value should be stored verbatim.
    ///
    /// `lz4_flex::compress_prepend_size` prepends the uncompressed length as a little-endian
    /// `u32`, so `decompress` does not need a separate length field.
    pub(crate) fn compress(self, data: &[u8]) -> Option<Vec<u8>> {
        match self {
            CompressionAlgorithm::None => None,
            #[cfg(feature = "compression")]
            CompressionAlgorithm::Lz4 => {
                let compressed = lz4_flex::compress_prepend_size(data);
                // Only worthwhile if we actually save a meaningful amount.
                if compressed.len() * MIN_RATIO_DENOMINATOR
                    < data.len() * MIN_RATIO_NUMERATOR
                {
                    Some(compressed)
                } else {
                    None
                }
            }
        }
    }

    /// Decompress bytes that were previously compressed with this algorithm.
    ///
    /// # Panics
    /// Panics if `data` is corrupt. This indicates database corruption and cannot be
    /// recovered from at this layer.
    pub(crate) fn decompress(self, data: &[u8]) -> Vec<u8> {
        match self {
            CompressionAlgorithm::None => {
                // Should never be called on uncompressed data; the caller checks the flag.
                unreachable!("decompress called but compression algorithm is None")
            }
            #[cfg(feature = "compression")]
            CompressionAlgorithm::Lz4 => lz4_flex::decompress_size_prepended(data)
                .expect("lz4 decompression failed: leaf page data is corrupt"),
        }
    }
}
