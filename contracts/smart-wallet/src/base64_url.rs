// Unpadded base64url encoder into a fixed-size destination buffer.
//
// Ported from Go's encoding/base64 (BSD-3-Clause), the same port used by the
// reference implementation this contract's __check_auth logic is adapted
// from (leighmcculloch/soroban-webauthn) — see lib.rs module docs.

const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn encode(dst: &mut [u8], src: &[u8]) {
    let mut di: usize = 0;
    let mut si: usize = 0;
    let n = (src.len() / 3) * 3;
    while si < n {
        let val = (src[si] as usize) << 16 | (src[si + 1] as usize) << 8 | (src[si + 2] as usize);
        dst[di] = ALPHABET[val >> 18 & 0x3F];
        dst[di + 1] = ALPHABET[val >> 12 & 0x3F];
        dst[di + 2] = ALPHABET[val >> 6 & 0x3F];
        dst[di + 3] = ALPHABET[val & 0x3F];
        si += 3;
        di += 4;
    }

    let remain = src.len() - si;
    if remain == 0 {
        return;
    }

    let mut val = (src[si] as usize) << 16;
    if remain == 2 {
        val |= (src[si + 1] as usize) << 8;
    }

    dst[di] = ALPHABET[val >> 18 & 0x3F];
    dst[di + 1] = ALPHABET[val >> 12 & 0x3F];

    if remain == 2 {
        dst[di + 2] = ALPHABET[val >> 6 & 0x3F];
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn encodes_32_bytes_to_43_chars_no_padding() {
        let src = [0u8; 32];
        let mut dst = [0u8; 43];
        encode(&mut dst, &src);
        assert_eq!(&dst, b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    }

    #[test]
    fn matches_known_vector() {
        let src = b"hello world";
        let mut dst = [0u8; 15]; // ceil(11 * 4 / 3) = 15 chars, no padding
        encode(&mut dst, src);
        assert_eq!(&dst, b"aGVsbG8gd29ybGQ");
    }
}
