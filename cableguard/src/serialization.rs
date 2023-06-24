#[derive(Debug)]
pub(crate) struct KeyBytes(pub [u8; 32]);

// CG: I can't see how this improves the handling for keys,
// strong candidate for removal
pub (crate) fn keybytes_to_hex_string(key_bytes: &KeyBytes) -> String {
    let bytes = &key_bytes.0;
    let hex_digits: Vec<String> = bytes.iter()
        .map(|byte| format!("{:02x}", byte))
        .collect();
    hex_digits.join("")
}

impl std::str::FromStr for KeyBytes {
    type Err = &'static str;

    // CG From Hex or base64 to KeyBytes ~ [u8;32] 
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut internal = [0u8; 32];

        match s.len() {
            64 => {
                // Try to parse as Hex
                for i in 0..32 {
                    internal[i] = u8::from_str_radix(&s[i * 2..=i * 2 + 1], 16)
                        .map_err(|_| "Illegal character in key")?;
                }
            }
            43 | 44 => {
                // Try to parse as base64
                if let Ok(decoded_key) = base64::decode(s) {
                    if decoded_key.len() == internal.len() {
                        internal[..].copy_from_slice(&decoded_key);
                    } else {
                        return Err("Illegal character in key");
                    }
                }
            }
            _ => return Err("Illegal key size"),
        }

        Ok(KeyBytes(internal))
    }
}
