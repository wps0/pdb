use std::cmp::max;
#[cfg(test)]
#[path = "encodings_tests.rs"]
mod tests;

pub fn zigzag_encode(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

pub fn zigzag_decode(n: u64) -> i64 {
    (n >> 1) as i64 ^ -((n & 1) as i64)
}

pub fn vle_encode(n: u64, buf: &mut [u8]) -> usize {
    let bytes = max((64 - n.leading_zeros()).div_ceil(7) as usize, 1);
    for i in 0..bytes {
        let shift = 7 * (bytes - i - 1);
        buf[i] = (n >> shift) as u8 | 0x80;
    }
    buf[bytes - 1] ^= 0x80;
    bytes
}

pub fn vle_decode(buf: &[u8]) -> (u64, usize) {
    let mut n = 0;
    let mut i = 0;
    let mut has_next = true;
    while has_next && i < buf.len() {
        n <<= 7;
        n += (buf[i] & 0x7f) as u64;
        has_next = (buf[i] & 0x80) > 0;
        i += 1;
    }
    assert!(!has_next);
    (n, i)
}

pub fn vle_decode_from_stream(
    stream: &mut impl std::io::Read,
) -> std::io::Result<u64> {
    let mut n = 0;
    let mut has_next = true;
    let mut byte = [0u8; 1];
    while has_next {
        stream.read_exact(&mut byte)?;
        n <<= 7;
        n += (byte[0] & 0x7f) as u64;
        has_next = byte[0] & 0x80 != 0;
    }
    Ok(n)
}

pub fn delta_encode(n: i64, last_n: i64) -> i64 {
    n.wrapping_sub(last_n)
}

pub fn delta_decode(n: i64, last_n: i64) -> i64 {
    n.wrapping_add(last_n)
}
