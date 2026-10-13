//! CRC-32C (Castagnoli) for durable records, table-driven (slicing-by-8), no dependency.

const POLY: u32 = 0x82F6_3B78;

/// `TABLES[0]` is the byte-at-a-time table; `TABLES[k]` advances a byte through `k` more
/// zero bytes, so eight input bytes are folded per step ("slicing-by-8").
const TABLES: [[u32; 256]; 8] = {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    t
};

pub(crate) fn crc32c(data: &[u8]) -> u32 {
    let (chunks, tail) = data.as_chunks::<8>();
    let mut c = !0u32;
    for b in chunks {
        let lo = c ^ u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        c = TABLES[7][(lo & 0xFF) as usize]
            ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
            ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][b[4] as usize]
            ^ TABLES[2][b[5] as usize]
            ^ TABLES[1][b[6] as usize]
            ^ TABLES[0][b[7] as usize];
    }
    !tail.iter().fold(c, |c, &b| TABLES[0][((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8))
}

#[cfg(test)]
mod tests {
    use super::crc32c;

    // Check values from RFC 3720 appendix B.4 and the CRC catalogue.
    #[test]
    fn matches_standard_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn matches_rfc3720_zero_vector() {
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
    }

    fn bytewise(data: &[u8]) -> u32 {
        !data.iter().fold(!0u32, |c, &b| super::TABLES[0][((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8))
    }

    proptest::proptest! {
        #[test]
        fn slicing_matches_bytewise(data in proptest::collection::vec(proptest::num::u8::ANY, 0..100)) {
            proptest::prop_assert_eq!(crc32c(&data), bytewise(&data));
        }
    }
}
