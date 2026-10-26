//! PIN hashing (spec 4.1): `pbkdf2_sha256$<iterations>$<salt hex>$<dk hex>`,
//! PBKDF2-HMAC-SHA256 over the PIN's UTF-8 bytes, 16-byte salt, 32-byte key. Hashes written
//! by the Python app verify unchanged.

use crate::config::hex;

pub const PBKDF2_ITERATIONS: u32 = 200_000;
const DK_LEN: usize = 32;

fn derive(pin: &str, salt: &[u8], iterations: u32) -> [u8; DK_LEN] {
    let mut dk = [0u8; DK_LEN];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pin.as_bytes(), salt, iterations, &mut dk);
    dk
}

/// Hash a PIN with a fresh random salt.
pub fn hash_pin(pin: &str, iterations: u32) -> String {
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).expect("OS random source");
    hash_pin_with_salt(pin, &salt, iterations)
}

pub fn hash_pin_with_salt(pin: &str, salt: &[u8], iterations: u32) -> String {
    let dk = derive(pin, salt, iterations);
    format!("pbkdf2_sha256${}${}${}", iterations, hex(salt), hex(&dk))
}

/// `bytes.fromhex`: pairs of hex digits (either case), spaces between bytes allowed.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let mut i = 0;
    let val = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        if b[i] == b' ' {
            i += 1;
            continue;
        }
        let hi = val(b[i])?;
        let lo = val(*b.get(i + 1)?)?;
        out.push(hi << 4 | lo);
        i += 2;
    }
    Some(out)
}

/// Constant-time comparison (`hmac.compare_digest`); different lengths compare unequal.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    std::hint::black_box(d) == 0
}

/// Verify `pin` against a stored hash. Any malformed hash is simply false.
pub fn verify_pin(pin: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2_sha256" {
        return false;
    }
    // Python int(): surrounding whitespace and a + sign are fine.
    let Ok(iters) = parts[1].trim().trim_start_matches('+').parse::<u32>() else { return false };
    if iters == 0 {
        return false; // Python's pbkdf2_hmac raises here; refusing is the safe equivalent.
    }
    let (Some(salt), Some(expected)) = (from_hex(parts[2]), from_hex(parts[3])) else {
        return false;
    };
    ct_eq(&derive(pin, &salt, iters), &expected)
}

/// `--set-pin` format: 4-6 ASCII digits.
pub fn valid_pin_format(pin: &str) -> bool {
    (4..=6).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    // py: test_logic.TestPin.test_hash_verify
    #[test]
    fn hash_verify() {
        let h = hash_pin("1234", 1000);
        assert!(h.starts_with("pbkdf2_sha256$1000$"));
        assert!(verify_pin("1234", &h));
        assert!(!verify_pin("1235", &h));
        assert!(!verify_pin("", &h));
    }

    // py: test_logic.TestPin.test_default_iterations_and_salt
    #[test]
    fn default_iterations_and_salt() {
        let h1 = hash_pin("1234", PBKDF2_ITERATIONS);
        let h2 = hash_pin("1234", PBKDF2_ITERATIONS);
        assert!(h1.contains("$200000$"));
        assert_ne!(h1, h2);
        assert_eq!(h1.split('$').nth(2).unwrap().len(), 32);
    }

    /// A hash computed by Python's hashlib verifies (and is reproduced) byte for byte.
    #[test]
    fn python_hash_verifies() {
        let py = "pbkdf2_sha256$1000$000102030405060708090a0b0c0d0e0f$\
                  2240ea2a22754a3c4610356ef8e4d7acf25af2ac3d2d11866de97a8b7812c52c";
        let salt: Vec<u8> = (0u8..16).collect();
        assert_eq!(hash_pin_with_salt("1234", &salt, 1000), py);
        assert!(verify_pin("1234", py));
        assert!(verify_pin("1234", &py.to_uppercase().replacen("PBKDF2_SHA256", "pbkdf2_sha256", 1)));
    }

    // py: test_logic.TestPin.test_garbage_hash
    #[test]
    fn garbage_hash() {
        for bad in ["", "x", "md5$1$aa$bb", "pbkdf2_sha256$x$aa$bb", "pbkdf2_sha256$10$zz$bb",
                    "pbkdf2_sha256$1000$00", "pbkdf2_sha256$1000$00$00$00", "pbkdf2_sha256$0$00$00"] {
            assert!(!verify_pin("1234", bad), "{bad}");
        }
    }

    // py: test_logic.TestPin.test_format
    #[test]
    fn format() {
        assert!(valid_pin_format("1234"));
        assert!(valid_pin_format("741852"));
        for bad in ["123", "12a4", "", "1234567", "\u{0661}\u{0662}\u{0663}\u{0664}"] {
            assert!(!valid_pin_format(bad), "{bad}");
        }
    }
}
