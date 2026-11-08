use super::{
    delta_decode, delta_encode, vle_decode, vle_encode, zigzag_decode,
    zigzag_encode,
};

#[test]
fn delta_encode_manual() {
    assert_eq!(delta_encode(0, 0), 0);
    assert_eq!(delta_encode(5, 2), 3);
    assert_eq!(delta_encode(2, 5), -3);
}

#[test]
fn delta_decode_manual() {
    assert_eq!(delta_decode(0, 0), 0);
    assert_eq!(delta_decode(5, 2), 7);
    assert_eq!(delta_decode(-5, 2), -3);
}

#[test]
fn vle_roundtrip() {
    for x in [0, 1, 127, 128, 300, u64::MAX] {
        let mut buf = [0u8; 10];
        let n = vle_encode(x, &mut buf);
        assert_eq!(vle_decode(&buf[..n]), (x, n));
    }
}

#[test]
fn zigzag_roundtrip() {
    for x in [0, 1, -1, i64::MIN, i64::MAX] {
        assert_eq!(zigzag_decode(zigzag_encode(x)), x);
    }
}
