use anyhow::{Context, Result};

/// Generate a random id: 128 bits from the system, as 32 lowercase hex
/// characters. The audit id (`review init`, and the first run of a project
/// without one) and the incident bundle suffix use it. Fallible so a missing
/// entropy source surfaces through the caller's error path, never a panic.
pub fn try_generate_short_id() -> Result<String> {
    let bytes = system_random_bytes::<16>()
        .context("failed to read system randomness from /dev/urandom")?;
    Ok(hex_id(&bytes))
}

/// Generate a v4 UUID for provisioning a fresh, persistable session ID.
pub fn generate_uuid() -> String {
    // Read from /proc/sys/kernel/random/uuid (Linux)
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| {
            // Fallback: generate a v4 UUID from random bytes
            let mut buf = system_random_bytes::<16>().unwrap_or_else(|error| {
                panic!("failed to read system randomness from /dev/urandom: {error}");
            });
            buf[6] = (buf[6] & 0x0f) | 0x40; // version 4
            buf[8] = (buf[8] & 0x3f) | 0x80; // variant 1
            format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                buf[0], buf[1], buf[2], buf[3],
                buf[4], buf[5],
                buf[6], buf[7],
                buf[8], buf[9],
                buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]
            )
        })
}

fn system_random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom").and_then(|mut file| {
        use std::io::Read;
        file.read_exact(&mut bytes)
    })?;
    Ok(bytes)
}

fn hex_id(bytes: &[u8]) -> String {
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    let mut id = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        id.push(HEX[usize::from(*byte >> 4)]);
        id.push(HEX[usize::from(*byte & 0x0f)]);
    }
    id
}
