//! Room codes: 6 characters, no look-alikes (0/O, 1/I/L), shown as `K7M-Q2X`.

pub const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
pub const LEN: usize = 6;

/// Builds a code from a random index source (`rand_index(n)` must return `0..n`).
pub fn generate(mut rand_index: impl FnMut(usize) -> usize) -> String {
    (0..LEN).map(|_| ALPHABET[rand_index(ALPHABET.len())] as char).collect()
}

/// Accepts user input like `k7m-q2x` or `K7M Q2X`; returns the canonical code.
pub fn normalize(input: &str) -> Option<String> {
    let s: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (s.len() == LEN && s.bytes().all(|b| ALPHABET.contains(&b))).then_some(s)
}

/// `K7MQ2X` → `K7M-Q2X`
pub fn display(code: &str) -> String {
    if code.len() == LEN {
        format!("{}-{}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_friendly_input() {
        assert_eq!(normalize("k7m-q2x").as_deref(), Some("K7MQ2X"));
        assert_eq!(normalize(" K7M Q2X ").as_deref(), Some("K7MQ2X"));
    }

    #[test]
    fn normalize_rejects_lookalikes_and_bad_length() {
        assert_eq!(normalize("K7M-Q2O"), None); // O is not in the alphabet
        assert_eq!(normalize("K7M-Q2"), None);
        assert_eq!(normalize("K7M-Q2XX"), None);
    }

    #[test]
    fn generate_uses_alphabet() {
        let mut i = 0;
        let code = generate(|n| {
            i += 7;
            i % n
        });
        assert_eq!(code.len(), LEN);
        assert!(normalize(&code).is_some());
        assert_eq!(display("K7MQ2X"), "K7M-Q2X");
    }
}
