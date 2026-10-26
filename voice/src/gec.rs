//! Token `Sec-MS-GEC` **v2** del servei Read Aloud de Microsoft Edge.
//!
//! Des del 2026-10-10 Microsoft només accepta tokens v2 (el "edge-tts" 7.3.1
//! els implementa a `drm.py`). El token v2 és `<cs64(16 hex)>-<sha256(64 hex)>`
//! on el SHA-256 és una variant amb un "schedule" inicial fixat per Microsoft
//! (no és sha256 estàndard) i el CS64 és un hash de 64 bits propi de MS.
//!
//! Port literal de rany2/edge-tts `drm.py` (7.3.1), traduït a Rust i validat
//! contra vectors generats pel codi original (tests a baix).

// ---------------------------------------------------------------------------
// Constants (mirall exacte de drm.py)
// ---------------------------------------------------------------------------

const U32_MASK: u32 = u32::MAX;

const SHA256_INITIAL: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB,
    0x5BE0CD19,
];

const SHA256_STATE: [u32; 8] = [
    0xFD9FBEA2, 0x91243CE7, 0xBE499094, 0xCD2E1E0A, 0x6C006F49, 0x04BD35AB, 0xFEB0A7E4,
    0x03EFFBD2,
];

const SHA256_SCHEDULE: [u32; 14] = [
    0xEB24BF9E, 0x1558691F, 0xA0C09EE2, 0x0CFA32BA, 0xA622EE8C, 0xD1DD9491, 0xC0DD25A0,
    0xF9E70AB6, 0xF0C31573, 0x7058CE4E, 0x7188E606, 0x00E5AB33, 0xD258B060, 0x72766564,
];

const SHA256_ROUND_CONSTANTS: [u32; 64] = [
    0x428A2F98, 0x71374491, 0xB5C0FBCF, 0xE9B5DBA5, 0x3956C25B, 0x59F111F1, 0x923F82A4,
    0xAB1C5ED5, 0xD807AA98, 0x12835B01, 0x243185BE, 0x550C7DC3, 0x72BE5D74, 0x80DEB1FE,
    0x9BDC06A7, 0xC19BF174, 0xE49B69C1, 0xEFBE4786, 0x0FC19DC6, 0x240CA1CC, 0x2DE92C6F,
    0x4A7484AA, 0x5CB0A9DC, 0x76F988DA, 0x983E5152, 0xA831C66D, 0xB00327C8, 0xBF597FC7,
    0xC6E00BF3, 0xD5A79147, 0x06CA6351, 0x14292967, 0x27B70A85, 0x2E1B2138, 0x4D2C6DFC,
    0x53380D13, 0x650A7354, 0x766A0ABB, 0x81C2C92E, 0x92722C85, 0xA2BFE8A1, 0xA81A664B,
    0xC24B8B70, 0xC76C51A3, 0xD192E819, 0xD6990624, 0xF40E3585, 0x106AA070, 0x19A4C116,
    0x1E376C08, 0x2748774C, 0x34B0BCB5, 0x391C0CB3, 0x4ED8AA4A, 0x5B9CCA4F, 0x682E6FF3,
    0x748F82EE, 0x78A5636F, 0x84C87814, 0x8CC70208, 0x90BEFFFA, 0xA4506CEB, 0xBEF9A3F7,
    0xC67178F2,
];

const SHA256_ADJUSTMENT: u32 = 0xC9DC06A7;
const SHA256_BLOCK_CONSTANTS: [u32; 64] = {
    let mut c = SHA256_ROUND_CONSTANTS;
    c[14] = SHA256_ADJUSTMENT;
    c
};

const SHA256_SIGMA0_ADJUSTMENT: u32 = 0x059C0B80;
const SHA256_SIGMA1_ADJUSTMENT: u32 = 0x000B92C0;

const CS64_COEFFICIENTS: [(u32, u32); 14] = [
    (0xE82E91B1, 0xB32700D0),
    (0x44DE24C6, 0x88DFF51D),
    (0x3BEB65BC, 0x29514038),
    (0xC89C5933, 0xB328CBDF),
    (0x8D1EC211, 0xD405B4D5),
    (0x314EF5D5, 0x26E55F3C),
    (0xE92491B1, 0x607801C6),
    (0xF0A5B9D5, 0x82B2C1FB),
    (0x06064CF3, 0xFA51A3DA),
    (0xDA79FA60, 0x207CC464),
    (0xBED20560, 0x3C3C74F3),
    (0x89F9C9F8, 0xD2D14B61),
    (0x35FA38C6, 0xBB04A2C1),
    (0xCB49E3C6, 0xC5626167),
];

const CS64_FACTOR: u32 = 0x82893933;
const CS64_ADJUSTMENT0: u32 = 0xF2F90B1A;
const CS64_ADJUSTMENT1: u32 = 0x0B1A0000;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rol16(value: u32) -> u32 {
    value.rotate_left(16)
}

fn words_le4_even(data: &[u8]) -> Vec<u32> {
    let pad = (4 - (data.len() % 4)) % 4;
    let mut v = data.to_vec();
    v.extend(std::iter::repeat(0u8).take(pad));
    let mut out: Vec<u32> = v
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    if out.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// σ0 del SHA-256 (retorna directament els 32 bits vàlids).
fn sigma0(value: u32) -> u32 {
    (value >> 7) ^ (value << 25) ^ (value >> 18) ^ (value << 14) ^ (value >> 3)
}

/// σ1 del SHA-256.
fn sigma1(value: u32) -> u32 {
    (value >> 17) ^ (value << 15) ^ (value >> 19) ^ (value << 13) ^ (value >> 10)
}

#[inline]
fn ch(e: u32, f: u32, g: u32) -> u32 {
    (e & f) ^ (!e & g)
}

#[inline]
fn maj(a: u32, b: u32, c: u32) -> u32 {
    (a & b) ^ (a & c) ^ (b & c)
}

#[inline]
fn bsig0(a: u32) -> u32 {
    a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22)
}

#[inline]
fn bsig1(e: u32) -> u32 {
    e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25)
}

/// Una ronda SHA-256 (equivalent mod 2^32 al codi "desenrotllat" del drm.py).
#[inline]
fn sha256_round(state: &mut [u32; 8], w: u32, k: u32) {
    let a = state[0];
    let b = state[1];
    let c = state[2];
    let d = state[3];
    let e = state[4];
    let f = state[5];
    let g = state[6];
    let h = state[7];
    let t1 = h
        .wrapping_add(bsig1(e))
        .wrapping_add(ch(e, f, g))
        .wrapping_add(k)
        .wrapping_add(w);
    let t2 = bsig0(a).wrapping_add(maj(a, b, c));
    state[7] = g;
    state[6] = f;
    state[5] = e;
    state[4] = d.wrapping_add(t1);
    state[3] = c;
    state[2] = b;
    state[1] = a;
    state[0] = t1.wrapping_add(t2);
}

// ---------------------------------------------------------------------------
// SHA-256 (variant de Microsoft) — mirall de _sha256_digest / _sha256_init
// ---------------------------------------------------------------------------

fn expand_sha256_schedule(words: &mut Vec<u32>, first_word: usize) {
    for index in first_word..64 {
        let x = words[index - 2];
        let y = words[index - 15];
        let s1 = sigma1(x);
        let s0 = sigma0(y);
        words.push(s1.wrapping_add(words[index - 7]).wrapping_add(s0).wrapping_add(words[index - 16]));
    }
}

fn sha256_rounds(
    mut state: [u32; 8],
    words: &[u32],
    first_round: usize,
    block_constants: bool,
) -> [u32; 8] {
    let constants: &[u32; 64] = if block_constants {
        &SHA256_BLOCK_CONSTANTS
    } else {
        &SHA256_ROUND_CONSTANTS
    };
    for index in first_round..64 {
        sha256_round(&mut state, words[index], constants[index]);
    }
    state
}

fn sha256_init(data: &[u8]) -> [u32; 8] {
    debug_assert!(data.len() >= 7);
    let mut words: Vec<u32> = vec![0; 14];
    words.push(u32::from_be_bytes([data[0], data[1], data[2], 0]) >> 8); // 3 bytes
    words.push(u32::from_be_bytes([data[3], data[4], data[5], data[6]])); // 4 bytes

    for index in 16..30 {
        let mut value = SHA256_SCHEDULE[index - 16];
        if index - 2 >= 14 {
            value = value.wrapping_add(if index == 16 {
                sigma1(words[index - 2]) ^ SHA256_SIGMA1_ADJUSTMENT
            } else {
                sigma1(words[index - 2])
            });
        }
        if index - 7 >= 14 {
            value = value.wrapping_add(words[index - 7]);
        }
        if index - 15 >= 14 {
            value = value.wrapping_add(if index == 29 {
                sigma0(words[index - 15]) ^ SHA256_SIGMA0_ADJUSTMENT
            } else {
                sigma0(words[index - 15])
            });
        }
        words.push(value);
    }

    let w30 = sigma1(words[28])
        .wrapping_add(words[23])
        .wrapping_add(sigma0(words[15]))
        .wrapping_add(words[14])
        .wrapping_add(SHA256_ADJUSTMENT)
        .wrapping_sub(SHA256_ROUND_CONSTANTS[14]);
    words.push(w30);

    expand_sha256_schedule(&mut words, 31);

    let working = sha256_rounds(SHA256_STATE, &words, 14, true);
    let mut out = [0u32; 8];
    for i in 0..8 {
        out[i] = SHA256_INITIAL[i].wrapping_add(working[i]);
    }
    out
}

fn sha256_digest(data: &[u8]) -> Vec<u8> {
    assert!(data.len() >= 7, "Token salt must contain at least seven bytes");
    let length = 57 + data.len();
    let mut padded = data[7..].to_vec();
    padded.push(0x80u8);
    let pad_len = 55usize.wrapping_sub(length) % 64;
    padded.extend(std::iter::repeat(0u8).take(pad_len));
    let bit_len = (length as u64) * 8;
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = sha256_init(&data[..7]);
    for block in padded.chunks_exact(64) {
        let mut words: Vec<u32> = block
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        expand_sha256_schedule(&mut words, 16);
        let working = sha256_rounds(state, &words, 0, false);
        for i in 0..8 {
            state[i] = state[i].wrapping_add(working[i]);
        }
    }

    let mut out = Vec::with_capacity(32);
    for w in state {
        out.extend_from_slice(&w.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// CS64 (hash de Microsoft) — mirall de _cs64_digest
// ---------------------------------------------------------------------------

fn cs64_digest(data: &[u8], digest: &[u8]) -> (u32, u32) {
    let key_a = (u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]])) | 1;
    let key_b = (u32::from_le_bytes([digest[4], digest[5], digest[6], digest[7]])) | 1;

    let factors = (
        key_a.wrapping_mul(CS64_FACTOR),
        key_b.wrapping_mul(CS64_FACTOR),
    );

    let mut terms: Vec<u32> = Vec::with_capacity(64);
    let mut offsets: Vec<u32> = Vec::with_capacity(64);
    for (index, (coefficient, _)) in CS64_COEFFICIENTS.iter().enumerate() {
        terms.push(coefficient.wrapping_mul(if index % 2 == 0 {
            factors.0
        } else {
            factors.1
        }));
    }
    for (_, offset) in CS64_COEFFICIENTS.iter() {
        offsets.push(*offset);
    }

    let mut padded = vec![0u8];
    padded.extend_from_slice(data);
    for (index, word) in words_le4_even(&padded).into_iter().enumerate() {
        let mut term = word.wrapping_mul(if index % 2 == 0 { key_a } else { key_b });
        let mut offset = rol16(word).wrapping_mul(if index % 2 == 0 {
            0xEF0569FB
        } else {
            0xC31713DB
        });
        if index == 0 {
            term = term.wrapping_add(CS64_ADJUSTMENT0.wrapping_mul(factors.0));
            offset = offset.wrapping_add(CS64_ADJUSTMENT1);
        }
        terms.push(term);
        offsets.push(offset);
    }

    let mut swap_acc: u32 = 0;
    let mut swap_last: u32 = 0;
    let mut state: u32 = 0;
    let mut acc: u32 = 0;
    for index in (0..terms.len()).step_by(2) {
        let x = terms[index];
        let y = terms[index + 1];
        let mut t = rol16(offsets[index].wrapping_add(x));
        let mut q = t.wrapping_mul(0x76F50F34);
        swap_acc = swap_acc
            .wrapping_add(rol16(q).wrapping_mul(0xC3EFEA97))
            .wrapping_add(q);
        t = rol16(offsets[index + 1].wrapping_add(y));
        q = t.wrapping_mul(0x1881CE3C);
        swap_last = rol16(q).wrapping_mul(0x35BD1EC9).wrapping_add(q);
        swap_acc = swap_acc.wrapping_add(swap_last);

        state = state.wrapping_mul(key_a).wrapping_add(x);
        state = rol16(state).wrapping_mul(0xCF98B111);
        state = rol16(state).wrapping_mul(0x87085B9F);
        state = rol16(state).wrapping_mul(0x12CEB96D);
        state = rol16(state).wrapping_mul(0x257E1D83);
        acc = acc.wrapping_add(state);
        t = state.wrapping_mul(key_b).wrapping_add(y);
        t = rol16(t).wrapping_mul(0xA27416F5);
        t = rol16(t).wrapping_mul(0xD38396FF);
        state = rol16(rol16(t).wrapping_mul(0x7C932B89)).wrapping_mul(0xBFA49F69);
        acc = acc.wrapping_add(state);
    }

    (swap_last ^ state, swap_acc ^ acc)
}

// ---------------------------------------------------------------------------
// API pública
// ---------------------------------------------------------------------------

fn token_hash(data: &[u8]) -> (String, String) {
    let digest = sha256_digest(data);
    let (cs64a, cs64b) = cs64_digest(data, &digest);
    let mut cs = String::with_capacity(16);
    for b in [cs64a.to_le_bytes(), cs64b.to_le_bytes()].concat() {
        cs.push_str(&format!("{b:02X}"));
    }
    let sh: String = digest.iter().map(|b| format!("{b:02X}")).collect();
    (cs, sh)
}

const WIN_EPOCH: f64 = 11_644_473_600.0;

/// Genera el token v2 per a l'instanti de temps `unix` (testejable).
pub fn generate_sec_ms_gec_v2_at(unix: f64, trusted_token: &str) -> String {
    let mut ticks = unix + WIN_EPOCH;
    ticks -= ticks % 300.0;
    ticks *= 10_000_000.0;
    let str_to_hash = format!("{ticks:.0}{trusted_token}");
    let (cs64, sha256) = token_hash(str_to_hash.as_bytes());
    format!("{cs64}-{sha256}")
}

/// Genera el token `Sec-MS-GEC` v2 amb el temps actual.
pub fn generate_sec_ms_gec_v2() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    generate_sec_ms_gec_v2_at(unix, "6A5AA1D4EAFF4E9FB37E23D68491D6F4")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors generats amb edge-tts 7.3.1 (drm.py) a Python.
    #[test]
    fn digest_matches_reference() {
        // _sha256_digest(b"1234567example")
        let d = sha256_digest(b"1234567example");
        let hex: String = d.iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(
            hex,
            "68966D0EB87FA0F310273B420A2F056541B1611327012F5D8EEFF55A5F91B547"
        );
    }

    #[test]
    fn cs64_matches_reference() {
        let d = sha256_digest(b"1234567example");
        let (a, b) = cs64_digest(b"1234567example", &d);
        let mut hex = String::new();
        for byte in [a.to_le_bytes(), b.to_le_bytes()].concat() {
            hex.push_str(&format!("{byte:02X}"));
        }
        assert_eq!(hex, "9E4C7724B83177B8");
    }

    #[test]
    fn token_v2_matches_reference() {
        assert_eq!(
            generate_sec_ms_gec_v2_at(3216499200.0, "6A5AA1D4EAFF4E9FB37E23D68491D6F4"),
            "68FF6D9DA9D15B1E-D6D1E99E213026676348E98BA5DAB898656439044419C363832F0DB3DBAFD627"
        );
        assert_eq!(
            generate_sec_ms_gec_v2_at(1735689600.0, "6A5AA1D4EAFF4E9FB37E23D68491D6F4"),
            "54404AFE67C9B44A-E72E51D8B4F5F75E9A96B7E20D7877227455BC310433BD30C817CD54A1D9313E"
        );
        assert_eq!(
            generate_sec_ms_gec_v2_at(1790000000.0, "6A5AA1D4EAFF4E9FB37E23D68491D6F4"),
            "D4216A7D3DE0C94F-91D741832FD084A110FDAD52BE2FFA569C6EA5F7D06A9429380A398738F4559A"
        );
    }
}
