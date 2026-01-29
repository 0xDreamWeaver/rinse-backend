//! Soulseek peer connection obfuscation
//!
//! Obfuscation is used by some Soulseek clients (like SoulseekQt) to make
//! P2P traffic harder for ISPs to identify and throttle.
//!
//! The algorithm:
//! 1. First 4 bytes of obfuscated message are a random key
//! 2. For each subsequent 4-byte block:
//!    - Rotate the key right by 31 bits (equivalent to rotate left by 1)
//!    - XOR the block with the rotated key
//!    - Use the rotated key for the next block

use rand::Rng;

/// Rotate a u32 right by n bits
#[inline]
fn rotate_right(value: u32, n: u32) -> u32 {
    value.rotate_right(n)
}

/// Obfuscate a message using the Soulseek obfuscation algorithm
/// Returns the obfuscated message with the 4-byte key prepended
pub fn obfuscate(data: &[u8]) -> Vec<u8> {
    let mut rng = rand::thread_rng();
    let initial_key: u32 = rng.gen();

    let mut result = Vec::with_capacity(4 + data.len());

    // Prepend the key (little-endian)
    result.extend_from_slice(&initial_key.to_le_bytes());

    // Process the data in 4-byte blocks
    let mut key = initial_key;
    for chunk in data.chunks(4) {
        // Rotate key right by 31 bits
        key = rotate_right(key, 31);

        // XOR each byte with corresponding key byte
        let key_bytes = key.to_le_bytes();
        for (i, &byte) in chunk.iter().enumerate() {
            result.push(byte ^ key_bytes[i]);
        }
    }

    result
}

/// Deobfuscate a message using the Soulseek obfuscation algorithm
/// The first 4 bytes of the input are the key
/// Returns the deobfuscated message (without the key prefix)
pub fn deobfuscate(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 {
        return None;
    }

    // Extract the key from the first 4 bytes
    let initial_key = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);

    let encrypted_data = &data[4..];
    let mut result = Vec::with_capacity(encrypted_data.len());

    // Process the data in 4-byte blocks
    let mut key = initial_key;
    for chunk in encrypted_data.chunks(4) {
        // Rotate key right by 31 bits
        key = rotate_right(key, 31);

        // XOR each byte with corresponding key byte
        let key_bytes = key.to_le_bytes();
        for (i, &byte) in chunk.iter().enumerate() {
            result.push(byte ^ key_bytes[i]);
        }
    }

    Some(result)
}

/// Check if data appears to be obfuscated
/// This is a heuristic - we try to deobfuscate and check if the result
/// looks like a valid peer init message
pub fn is_likely_obfuscated(data: &[u8]) -> bool {
    if data.len() < 9 {
        return false;
    }

    // If the first byte is 0 (PierceFirewall) or 1 (PeerInit), it's not obfuscated
    if data[0] == 0 || data[0] == 1 {
        // Check if it looks like a valid length prefix
        if data.len() >= 4 {
            let potential_length = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            // Valid lengths for peer init messages are typically < 1000
            if potential_length > 0 && potential_length < 1000 {
                return false; // Likely a valid length-prefixed non-obfuscated message
            }
        }
    }

    // Try to deobfuscate and check if result looks valid
    if let Some(deobfuscated) = deobfuscate(data) {
        if deobfuscated.len() >= 5 {
            // Check if deobfuscated data looks like a length-prefixed message
            let length = u32::from_le_bytes([
                deobfuscated[0],
                deobfuscated[1],
                deobfuscated[2],
                deobfuscated[3],
            ]);

            // Valid message lengths are reasonable
            if length > 0 && length < 10_000_000 {
                // Check if the 5th byte (type) is valid (0 or 1)
                let msg_type = deobfuscated[4];
                if msg_type == 0 || msg_type == 1 {
                    return true;
                }
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_obfuscate_deobfuscate_roundtrip() {
        let original = b"Hello, World! This is a test message.";

        let obfuscated = obfuscate(original);
        assert_eq!(obfuscated.len(), 4 + original.len());

        let deobfuscated = deobfuscate(&obfuscated).unwrap();
        assert_eq!(deobfuscated, original);
    }

    #[test]
    fn test_pierce_firewall_obfuscation() {
        // Simulate a PierceFirewall message: length=5, type=0, token=12345
        let mut message = Vec::new();
        message.extend_from_slice(&5u32.to_le_bytes()); // length
        message.push(0); // type
        message.extend_from_slice(&12345u32.to_le_bytes()); // token

        let obfuscated = obfuscate(&message);
        let deobfuscated = deobfuscate(&obfuscated).unwrap();

        assert_eq!(deobfuscated, message);
    }

    #[test]
    fn test_deobfuscate_short_data() {
        assert!(deobfuscate(&[1, 2, 3]).is_none());
    }
}
