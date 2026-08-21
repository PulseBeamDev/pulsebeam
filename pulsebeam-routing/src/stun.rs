//! Minimal STUN parsing: is this STUN, and if so what is the USERNAME
//! attribute's server ufrag (the token before `:`).
//!
//! Ported byte-for-byte from `pulsebeam/src/shard/demux.rs`'s `mod ice`. Every
//! read is bounds-checked via `slice::get`, every advance is checked against
//! the declared STUN message length before it happens, and the traversal is
//! bounded by that declared length (a `u16`, so at most 65535 bytes of
//! attributes) — no unbounded loop.

pub const MAGIC_COOKIE: u32 = 0x2112_A442;

const MIN_STUN_HEADER_SIZE: usize = 20;
const MAGIC_COOKIE_BYTES: [u8; 4] = MAGIC_COOKIE.to_be_bytes();
const ATTRIBUTE_HEADER_SIZE: usize = 4;
const USERNAME_ATTRIBUTE_TYPE: u16 = 0x0006;

pub fn is_stun(buf: &[u8]) -> bool {
    if buf.len() < MIN_STUN_HEADER_SIZE {
        return false;
    }
    let Some(&first) = buf.first() else {
        return false;
    };
    if first & 0b1100_0000 != 0 {
        return false;
    }
    buf.get(4..8) == Some(MAGIC_COOKIE_BYTES.as_slice())
}

/// The USERNAME attribute's first token (before `:`), i.e. the server ufrag.
pub fn server_ufrag(buf: &[u8]) -> Option<&[u8]> {
    first_token(find_username_slice(buf)?, b':')
}

fn find_username_slice(data: &[u8]) -> Option<&[u8]> {
    if data.len() < MIN_STUN_HEADER_SIZE {
        return None;
    }
    if data.first()? & 0b1100_0000 != 0 {
        return None;
    }
    if data.get(4..8) != Some(MAGIC_COOKIE_BYTES.as_slice()) {
        return None;
    }

    let message_length = usize::from(u16::from_be_bytes([*data.get(2)?, *data.get(3)?]));
    let expected_total_len = MIN_STUN_HEADER_SIZE.checked_add(message_length)?;
    if data.len() < expected_total_len || message_length == 0 {
        return None;
    }

    let mut current_pos = MIN_STUN_HEADER_SIZE;
    let attributes_end = expected_total_len;

    while current_pos < attributes_end {
        if current_pos.checked_add(ATTRIBUTE_HEADER_SIZE)? > attributes_end {
            return None;
        }

        let attr_type = u16::from_be_bytes([
            *data.get(current_pos)?,
            *data.get(current_pos.saturating_add(1))?,
        ]);
        let attr_value_len = usize::from(u16::from_be_bytes([
            *data.get(current_pos.saturating_add(2))?,
            *data.get(current_pos.saturating_add(3))?,
        ]));

        let value_pos = current_pos.saturating_add(ATTRIBUTE_HEADER_SIZE);
        let end_of_value = value_pos.checked_add(attr_value_len)?;
        if end_of_value > attributes_end {
            return None;
        }

        if attr_type == USERNAME_ATTRIBUTE_TYPE {
            return data.get(value_pos..end_of_value);
        }

        let padded_len = attr_value_len.saturating_add(3) & !3;
        let next_pos = value_pos.checked_add(padded_len)?;
        if next_pos > attributes_end {
            return None;
        }
        current_pos = next_pos;
    }

    None
}

fn first_token(input: &[u8], delimiter: u8) -> Option<&[u8]> {
    let mut i = 0usize;
    while i < input.len() {
        if input.get(i) == Some(&delimiter) {
            return input.get(..i);
        }
        i = i.checked_add(1)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const BINDING_REQUEST: u16 = 0x0001;
    const DUMMY_TX_ID: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    const PRIORITY_ATTRIBUTE_TYPE: u16 = 0x0024;

    fn build_stun_message(msg_type: u16, tx_id: [u8; 12], attributes: &[(u16, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&msg_type.to_be_bytes());
        buf.extend_from_slice(&[0u8; 2]);
        buf.extend_from_slice(&MAGIC_COOKIE_BYTES);
        buf.extend_from_slice(&tx_id);
        assert_eq!(buf.len(), MIN_STUN_HEADER_SIZE);

        let mut total_attr_len: usize = 0;
        for (attr_type, attr_value) in attributes {
            let attr_value_len = attr_value.len();
            buf.extend_from_slice(&attr_type.to_be_bytes());
            buf.extend_from_slice(&u16::try_from(attr_value_len).unwrap().to_be_bytes());
            buf.extend_from_slice(attr_value);
            let padded_len = (attr_value_len + 3) & !3;
            let padding_len = padded_len - attr_value_len;
            buf.extend_from_slice(&std::vec![0u8; padding_len]);
            total_attr_len += ATTRIBUTE_HEADER_SIZE + padded_len;
        }

        buf[2..4].copy_from_slice(&u16::try_from(total_attr_len).unwrap().to_be_bytes());
        buf
    }

    fn stun_with_username(value: &[u8]) -> Vec<u8> {
        build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[(USERNAME_ATTRIBUTE_TYPE, value)],
        )
    }

    #[test]
    fn is_stun_accepts_minimal_header() {
        let msg = build_stun_message(BINDING_REQUEST, DUMMY_TX_ID, &[]);
        assert!(is_stun(&msg));
    }

    #[test]
    fn is_stun_rejects_short_buffer() {
        assert!(!is_stun(&[0u8; MIN_STUN_HEADER_SIZE - 1]));
        assert!(!is_stun(&[]));
    }

    #[test]
    fn is_stun_rejects_bad_magic_cookie() {
        let mut msg = build_stun_message(BINDING_REQUEST, DUMMY_TX_ID, &[]);
        msg[7] = msg[7].wrapping_add(1);
        assert!(!is_stun(&msg));
    }

    #[test]
    fn is_stun_rejects_bad_message_type_high_bits() {
        let mut msg = build_stun_message(BINDING_REQUEST, DUMMY_TX_ID, &[]);
        msg[0] |= 0b1100_0000;
        assert!(!is_stun(&msg));
    }

    #[test]
    fn finds_username_at_minimum_length() {
        let msg = stun_with_username(b"ab:cd");
        assert_eq!(server_ufrag(&msg), Some(b"ab".as_slice()));
    }

    #[test]
    fn finds_username_split_across_colon() {
        let msg = stun_with_username(b"server-ufrag:client-ufrag");
        assert_eq!(server_ufrag(&msg), Some(b"server-ufrag".as_slice()));
    }

    #[test]
    fn username_without_colon_yields_no_token() {
        let msg = stun_with_username(b"no-colon-here");
        assert_eq!(server_ufrag(&msg), None);
    }

    #[test]
    fn finds_username_with_padded_attribute_before_it() {
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[
                (PRIORITY_ATTRIBUTE_TYPE, b"a".as_slice()),
                (USERNAME_ATTRIBUTE_TYPE, b"srv:cli".as_slice()),
            ],
        );
        assert_eq!(server_ufrag(&msg), Some(b"srv".as_slice()));
    }

    #[test]
    fn finds_username_at_max_reasonable_length() {
        let long_username = std::vec![b'a'; 512];
        let mut value = long_username.clone();
        value.push(b':');
        value.extend_from_slice(b"client");
        let msg = stun_with_username(&value);
        assert_eq!(server_ufrag(&msg), Some(long_username.as_slice()));
    }

    #[test]
    fn rejects_non_stun_payload() {
        let payload = std::vec![0xAAu8; 64];
        assert!(!is_stun(&payload));
        assert_eq!(server_ufrag(&payload), None);
    }

    #[test]
    fn rejects_truncated_packet() {
        let msg = stun_with_username(b"srv:cli");
        for len in 0..msg.len() {
            assert_eq!(
                server_ufrag(&msg[..len]),
                None,
                "len {len} should not yield a username"
            );
        }
    }

    #[test]
    fn rejects_declared_length_exceeding_buffer() {
        let mut msg = stun_with_username(b"srv:cli");
        let declared = u16::try_from(msg.len() - MIN_STUN_HEADER_SIZE + 1).unwrap();
        msg[2..4].copy_from_slice(&declared.to_be_bytes());
        assert_eq!(server_ufrag(&msg), None);
    }

    #[test]
    fn rejects_zero_declared_length_with_trailing_bytes() {
        let mut msg = stun_with_username(b"srv:cli");
        msg[2..4].copy_from_slice(&0u16.to_be_bytes());
        assert_eq!(server_ufrag(&msg), None);
    }

    #[test]
    fn rejects_attribute_header_cut_off_by_declared_length() {
        let mut msg = stun_with_username(b"srv:cli");
        msg[2..4].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(server_ufrag(&msg), None);
    }

    #[test]
    fn finds_username_when_its_padded_value_lands_exactly_on_the_boundary() {
        // "srv:c" is 5 bytes, padded to 8 — the attribute's end lands exactly
        // on `attributes_end` with no slack, exercising the boundary check.
        let msg = stun_with_username(b"srv:c");
        assert_eq!(server_ufrag(&msg), Some(b"srv".as_slice()));
    }

    #[test]
    fn missing_username_attribute_yields_none() {
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[(PRIORITY_ATTRIBUTE_TYPE, b"abcd".as_slice())],
        );
        assert_eq!(server_ufrag(&msg), None);
    }
}
