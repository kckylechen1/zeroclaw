//! Small SHA-256/HMAC implementation (FIPS 180-4 and RFC 2104), without crates.
//! This is not a certified cryptographic module or a formal side-channel proof.

const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (word, bytes) in w[..16].iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    }
    for i in 16..64 {
        let x = w[i - 15];
        let y = w[i - 2];
        let s0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
        let s1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (word, addition) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *word = word.wrapping_add(addition);
    }
}

// Hash parts without allocating or concatenating the caller's message.
fn sha256_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut state = INITIAL;
    let mut block = [0u8; 64];
    let mut used = 0;
    let mut byte_len = 0u64;
    for &part in parts {
        // Only bounded protocol/store fields reach this primitive. Encode the
        // length modulo 2^64 without adding a production panic path.
        byte_len = byte_len.wrapping_add(part.len() as u64);
        let mut remaining = part;
        while !remaining.is_empty() {
            let count = (64 - used).min(remaining.len());
            block[used..used + count].copy_from_slice(&remaining[..count]);
            used += count;
            remaining = &remaining[count..];
            if used == 64 {
                compress(&mut state, &block);
                used = 0;
            }
        }
    }
    let bit_len = byte_len.wrapping_mul(8);
    block[used] = 0x80;
    used += 1;
    block[used..].fill(0);
    if used > 56 {
        compress(&mut state, &block);
        block.fill(0);
    }
    block[56..].copy_from_slice(&bit_len.to_be_bytes());
    compress(&mut state, &block);
    let mut digest = [0u8; 32];
    for (bytes, word) in digest.chunks_exact_mut(4).zip(state) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    sha256_parts(&[data])
}

pub fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        key_block[..32].copy_from_slice(&sha256(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for i in 0..64 {
        inner_pad[i] ^= key_block[i];
        outer_pad[i] ^= key_block[i];
    }
    let inner = sha256_parts(&[&inner_pad, data]);
    sha256_parts(&[&outer_pad, &inner])
}

/// Accumulate every byte difference, with no data-dependent early exit.
/// Rust/LLVM do not promise a formal constant-time machine-code guarantee.
pub fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut difference = 0u8;
    for i in 0..32 {
        difference |= a[i] ^ b[i];
    }
    difference == 0
}

pub fn hex(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::new();
    for &byte in data {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Accept exactly 64 ASCII lowercase hexadecimal characters; no normalization.
pub fn unhex32(s: &str) -> Result<[u8; 32], &'static str> {
    fn nibble(byte: u8) -> Result<u8, &'static str> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err("expected 64 lowercase hexadecimal characters"),
        }
    }
    if s.len() != 64 {
        return Err("expected 64 lowercase hexadecimal characters");
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(s.as_bytes().chunks_exact(2)) {
        *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(out)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn complete_random32(bytes: [u8; 32], written: isize) -> Result<[u8; 32], &'static str> {
    if written == 32 {
        Ok(bytes)
    } else {
        Err("OS randomness unavailable or incomplete")
    }
}

#[cfg(target_os = "linux")]
pub fn random32() -> Result<[u8; 32], &'static str> {
    unsafe extern "C" {
        fn getrandom(buffer: *mut std::ffi::c_void, length: usize, flags: u32) -> isize;
    }
    const GRND_NONBLOCK: u32 = 0x0001;
    let mut bytes = [0u8; 32];
    // SAFETY: bytes is writable for precisely 32 bytes throughout this call;
    // the declared argument/return types match Linux libc's getrandom ABI.
    let written = unsafe { getrandom(bytes.as_mut_ptr().cast(), bytes.len(), GRND_NONBLOCK) };
    // Includes short reads and all errors (EAGAIN, EINTR, ENOSYS, ...).
    // There is no blocking retry, weaker fallback, or partial-byte return.
    complete_random32(bytes, written)
}

#[cfg(target_os = "macos")]
pub fn random32() -> Result<[u8; 32], &'static str> {
    unsafe extern "C" {
        fn getentropy(buffer: *mut std::ffi::c_void, length: usize) -> std::ffi::c_int;
    }
    let mut bytes = [0u8; 32];
    // SAFETY: bytes is writable for the requested 32 bytes (below the 256-byte
    // OS limit); the declaration matches macOS libc's getentropy ABI.
    let result = unsafe { getentropy(bytes.as_mut_ptr().cast(), bytes.len()) };
    complete_random32(bytes, if result == 0 { 32 } else { -1 })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn random32() -> Result<[u8; 32], &'static str> {
    Err("OS randomness unsupported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_standard_vectors() {
        for (input, expected) in [
            (
                &b""[..],
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                &b"abc"[..],
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                &b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"[..],
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ] {
            assert_eq!(hex(&sha256(input)), expected);
        }
        assert_eq!(
            hex(&sha256(&vec![b'a'; 1_000_000])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn sha256_padding_boundaries() {
        // Independently derived using Python hashlib.sha256(bytes(range(n))).
        for (length, expected) in [
            (
                1,
                "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
            ),
            (
                55,
                "463eb28e72f82e0a96c0a4cc53690c571281131f672aa229e0d45ae59b598b59",
            ),
            (
                56,
                "da2ae4d6b36748f2a318f23e7ab1dfdf45acdc9d049bd80e59de82a60895f562",
            ),
            (
                57,
                "2fe741af801cc238602ac0ec6a7b0c3a8a87c7fc7d7f02a3fe03d1c12eac4d8f",
            ),
            (
                63,
                "29af2686fd53374a36b0846694cc342177e428d1647515f078784d69cdb9e488",
            ),
            (
                64,
                "fdeab9acf3710362bd2658cdc9a29e8f9c757fcf9811603a8c447cd1d9151108",
            ),
            (
                65,
                "4bfd2c8b6f1eec7a2afeb48b934ee4b2694182027e6d0fc075074f2fabb31781",
            ),
            (
                119,
                "da18797ed7c3a777f0847f429724a2d8cd5138e6ed2895c3fa1a6d39d18f7ec6",
            ),
            (
                120,
                "f52b23db1fbb6ded89ef42a23ce0c8922c45f25c50b568a93bf1c075420bbb7c",
            ),
            (
                127,
                "92ca0fa6651ee2f97b884b7246a562fa71250fedefe5ebf270d31c546bfea976",
            ),
            (
                128,
                "471fb943aa23c511f6f72f8d1652d9c880cfa392ad80503120547703e56a2be5",
            ),
            (
                129,
                "5099c6a56203f9687f7d33f4bfdf576d31dc91f6b695ecea38b2770c87631135",
            ),
        ] {
            let input: Vec<u8> = (0..length).map(|i| i as u8).collect();
            assert_eq!(hex(&sha256(&input)), expected, "length {length}");
            for split in 0..=length {
                assert_eq!(
                    hex(&sha256_parts(&[&input[..split], &[], &input[split..]])),
                    expected,
                    "length {length}, split {split}"
                );
            }
        }
    }

    #[test]
    fn hmac_rfc4231_cases_1_through_7() {
        let cases: Vec<(Vec<u8>, Vec<u8>, &str)> = vec![
            (vec![0x0b; 20], b"Hi There".to_vec(),
             "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"),
            (b"Jefe".to_vec(), b"what do ya want for nothing?".to_vec(),
             "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"),
            (vec![0xaa; 20], vec![0xdd; 50],
             "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"),
            ((1..=25).collect(), vec![0xcd; 50],
             "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b"),
            (vec![0x0c; 20], b"Test With Truncation".to_vec(),
             "a3b6167473100ee06e0c796c2955552b"),
            (vec![0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
             "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"),
            (vec![0xaa; 131], b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.".to_vec(),
             "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"),
        ];
        for (i, (key, input, expected)) in cases.iter().enumerate() {
            let actual = hex(&hmac(key, input));
            // RFC 4231 case 5 specifies only the leftmost 128 bits.
            assert_eq!(&actual[..expected.len()], *expected, "RFC case {}", i + 1);
        }
    }

    #[test]
    fn hmac_empty_key_and_message() {
        assert_eq!(
            hex(&hmac(b"", b"")),
            "b613679a0814d9ec772f95d778c35fc5ff1697c493715653c6c712144292c5ad"
        );
    }

    #[test]
    fn hmac_key_block_boundaries() {
        // Independent Python hmac/hashlib vectors with binary key and message.
        let input: Vec<u8> = (0..129).collect();
        for (length, expected) in [
            (
                0,
                "c3ad09df1d5ade409091a67f747147417c38c0b15efaca8c195e7f76a3cbc519",
            ),
            (
                1,
                "c3ad09df1d5ade409091a67f747147417c38c0b15efaca8c195e7f76a3cbc519",
            ),
            (
                63,
                "f2d68b260aebd26969043402f5d297f7fe973800125c44f8fa63d3e59c100500",
            ),
            (
                64,
                "335e54597fb47ebfcffcaaa3a1711076bc8952532d562bf31c9bd3101de46dfa",
            ),
            (
                65,
                "dd6235fb28670d7f90d31c390b2c15014ae42adeabbc338d3a15cffe8e1f9103",
            ),
            (
                128,
                "d22cb69946f02a62f15a2036618bb99be839fbf67a2c3bee1f259d5aee8621e3",
            ),
            (
                129,
                "dcda197ed27793c0f0cfa338dba299dc4f9d287311e511f6e1d166f1e431201f",
            ),
        ] {
            let key: Vec<u8> = (0..length).map(|i| i as u8).collect();
            assert_eq!(hex(&hmac(&key, &input)), expected, "key length {length}");
        }
    }

    #[test]
    fn fixed_size_comparison_checks_every_byte() {
        let a = [0xa5; 32];
        assert!(ct_eq(&a, &a));
        for position in 0..32 {
            for bit in 0..8 {
                let mut b = a;
                b[position] ^= 1 << bit;
                assert!(!ct_eq(&a, &b));
                assert!(!ct_eq(&b, &a));
            }
        }
    }

    #[test]
    fn hex_encoding_and_strict_decoding() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0, 1, 15, 16, 128, 254, 255]), "00010f1080feff");
        for byte in 0..=255 {
            let bytes = [byte; 32];
            assert_eq!(unhex32(&hex(&bytes)), Ok(bytes));
        }
        for invalid in ["", "0", &"0".repeat(63), &"0".repeat(65), &"é".repeat(32)] {
            assert!(unhex32(invalid).is_err());
        }
        for position in 0..64 {
            for invalid in [b'A', b'F', b'g', b' ', b'\n', b':', b'\0', b'-'] {
                let mut bytes = [b'0'; 64];
                bytes[position] = invalid;
                assert!(unhex32(std::str::from_utf8(&bytes).unwrap()).is_err());
            }
        }
    }

    #[test]
    fn random_result_fails_closed_for_errors_and_short_reads() {
        // libc represents EAGAIN and other errors as -1. No failure result
        // returns even a prefix of this synthetic buffer to the caller.
        let bytes = [0xa5; 32];
        for written in -1..32 {
            assert!(complete_random32(bytes, written).is_err());
        }
        assert!(complete_random32(bytes, 33).is_err());
        assert_eq!(complete_random32(bytes, 32), Ok(bytes));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn os_randomness_smoke() {
        // Do not print generated bytes or claim statistical quality from this.
        assert!(random32().is_ok());
    }
}
