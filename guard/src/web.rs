// Default HTTP(S) ports supported by the shared CDN, including the
// nonstandard ports. The nftables redirect and proxy use the same list.
pub const HTTP_PORTS: &[u16] = &[80, 2052, 2082, 2086, 2095, 8080, 8880];
pub const HTTPS_PORTS: &[u16] = &[443, 2053, 2083, 2087, 2096, 8443];

pub fn ports() -> impl Iterator<Item = u16> {
    HTTP_PORTS.iter().chain(HTTPS_PORTS).copied()
}

pub fn is_web_port(port: u16) -> bool {
    ports().any(|candidate| candidate == port)
}

// Rustls exposes the outer server name but not unknown ClientHello
// extensions. Walk the ClientHello extension list to recognize ECH,
// including when the hello was split across multiple TLS records.
pub fn has_ech(mut records: &[u8]) -> Option<bool> {
    let mut handshake = Vec::new();
    while !records.is_empty() {
        let header = records.get(..5)?;
        if header[0] != 22 {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
        handshake.extend_from_slice(records.get(5..5 + len)?);
        records = records.get(5 + len..)?;
    }
    let header = handshake.get(..4)?;
    if header[0] != 1 {
        return None;
    }
    let length = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
    let mut hello = handshake.get(4 + 34..4 + length)?;
    field(&mut hello, 1)?; // Legacy session ID
    field(&mut hello, 2)?; // Cipher suites
    field(&mut hello, 1)?; // Compression methods
    if hello.is_empty() {
        return Some(false);
    }
    let mut extensions = field(&mut hello, 2)?;
    while !extensions.is_empty() {
        let id = extensions.get(..2)?;
        extensions = extensions.get(2..)?;
        field(&mut extensions, 2)?;
        if id == [0xfe, 0x0d] {
            return Some(true);
        }
    }
    Some(false)
}

fn field<'a>(input: &mut &'a [u8], width: usize) -> Option<&'a [u8]> {
    let prefix = input.get(..width)?;
    let len = prefix
        .iter()
        .fold(0usize, |len, byte| (len << 8) + usize::from(*byte));
    let field = input.get(width..width + len)?;
    *input = input.get(width + len..)?;
    Some(field)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragmented_hello(extension: [u8; 2]) -> Vec<u8> {
        let mut body = vec![0; 34];
        body.extend_from_slice(&[0, 0, 2, 0x13, 0x01, 1, 0, 0, 4]);
        body.extend_from_slice(&extension);
        body.extend_from_slice(&[0, 0]);
        let mut handshake = vec![1, 0, 0, body.len() as u8];
        handshake.extend_from_slice(&body);
        let mut records = Vec::new();
        for payload in [&handshake[..41], &handshake[41..]] {
            records.extend_from_slice(&[22, 3, 3, 0, payload.len() as u8]);
            records.extend_from_slice(payload);
        }
        records
    }

    #[test]
    fn sees_ech_in_a_fragmented_hello_without_confusing_other_extensions() {
        assert_eq!(has_ech(&fragmented_hello([0xfe, 0x0d])), Some(true));
        assert_eq!(has_ech(&fragmented_hello([0x00, 0x0d])), Some(false));
        assert_eq!(has_ech(&[22, 3, 3, 0, 20, 1]), None);
    }
}
