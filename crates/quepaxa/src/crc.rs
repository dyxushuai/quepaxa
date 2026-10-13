//! CRC-32C (Castagnoli) for durable records, table-driven, no dependency.

const POLY: u32 = 0x82F6_3B78;

const TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

pub(crate) fn crc32c(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, &b| {
        TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8)
    })
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
}
