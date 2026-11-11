//! EME (ECB-Mix-ECB) wide-block encryption, as used by rclone crypt for file names.
//!
//! Port of <https://github.com/rfjakob/eme> (Halevi & Rogaway, 2003). It is deterministic:
//! the same name always encrypts to the same ciphertext, which is what lets a listing be
//! matched back to a path.

use aes::Aes256;
use aes::cipher::{BlockDecrypt, BlockEncrypt, generic_array::GenericArray};

const BLOCK: usize = 16;
/// EME can transform between 1 and 128 AES blocks (16 to 2048 bytes).
pub const MAX_BLOCKS: usize = 16 * 8;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Encrypt,
    Decrypt,
}

fn mult_by_two(block: &mut [u8; BLOCK]) {
    let input = *block;
    block[0] = input[0].wrapping_mul(2) ^ (135 & (0u8.wrapping_sub(input[15] >> 7)));
    for j in 1..BLOCK {
        block[j] = input[j].wrapping_mul(2).wrapping_add(input[j - 1] >> 7);
    }
}

fn xor_into(out: &mut [u8], other: &[u8]) {
    for (a, b) in out.iter_mut().zip(other) {
        *a ^= b;
    }
}

fn aes_apply(block: &mut [u8], direction: Direction, cipher: &Aes256) {
    let block = GenericArray::from_mut_slice(block);
    match direction {
        Direction::Encrypt => cipher.encrypt_block(block),
        Direction::Decrypt => cipher.decrypt_block(block),
    }
}

fn tabulate_l(cipher: &Aes256, m: usize) -> Vec<[u8; BLOCK]> {
    let mut li = [0u8; BLOCK];
    cipher.encrypt_block(GenericArray::from_mut_slice(&mut li));
    let mut table = Vec::with_capacity(m);
    for _ in 0..m {
        mult_by_two(&mut li);
        table.push(li);
    }
    table
}

/// Encrypts or decrypts `data` (a multiple of 16 bytes, 1..=128 blocks) with tweak `tweak`.
///
/// Panics if the length preconditions are violated; callers validate lengths first.
pub fn transform(cipher: &Aes256, tweak: &[u8; BLOCK], data: &[u8], direction: Direction) -> Vec<u8> {
    assert!(data.len() % BLOCK == 0, "EME input must be a multiple of 16 bytes");
    let m = data.len() / BLOCK;
    assert!((1..=MAX_BLOCKS).contains(&m), "EME operates on 1 to 128 blocks");

    let l_table = tabulate_l(cipher, m);
    let mut c = data.to_vec();

    for j in 0..m {
        let chunk = &mut c[j * BLOCK..(j + 1) * BLOCK];
        xor_into(chunk, &l_table[j]);
        aes_apply(chunk, direction, cipher);
    }

    let mut mp = [0u8; BLOCK];
    mp.copy_from_slice(&c[0..BLOCK]);
    xor_into(&mut mp, tweak);
    for j in 1..m {
        xor_into(&mut mp, &c[j * BLOCK..(j + 1) * BLOCK]);
    }

    let mut mc = mp;
    aes_apply(&mut mc, direction, cipher);

    let mut mm = mp;
    xor_into(&mut mm, &mc);

    for j in 1..m {
        mult_by_two(&mut mm);
        xor_into(&mut c[j * BLOCK..(j + 1) * BLOCK], &mm);
    }

    let mut ccc1 = mc;
    xor_into(&mut ccc1, tweak);
    for j in 1..m {
        xor_into(&mut ccc1, &c[j * BLOCK..(j + 1) * BLOCK]);
    }
    c[0..BLOCK].copy_from_slice(&ccc1);

    for j in 0..m {
        let chunk = &mut c[j * BLOCK..(j + 1) * BLOCK];
        aes_apply(chunk, direction, cipher);
        xor_into(chunk, &l_table[j]);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::KeyInit;

    #[test]
    fn roundtrip_various_lengths() {
        let cipher = Aes256::new(GenericArray::from_slice(&[7u8; 32]));
        let tweak = [3u8; 16];
        for blocks in [1usize, 2, 5, 128] {
            let data: Vec<u8> = (0..blocks * 16).map(|i| i as u8).collect();
            let enc = transform(&cipher, &tweak, &data, Direction::Encrypt);
            assert_ne!(enc, data);
            let dec = transform(&cipher, &tweak, &enc, Direction::Decrypt);
            assert_eq!(dec, data);
        }
    }
}
