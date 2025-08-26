use std::convert::TryInto;

// --- Constants defined by RFC 5389 ---
const MIN_STUN_HEADER_SIZE: usize = 20;
const MAGIC_COOKIE: u32 = 0x2112A442;
// Pre-calculate byte representation for faster comparison
const MAGIC_COOKIE_BYTES: [u8; 4] = MAGIC_COOKIE.to_be_bytes();
const ATTRIBUTE_HEADER_SIZE: usize = 4; // Type (2 bytes) + Length (2 bytes)
const USERNAME_ATTRIBUTE_TYPE: u16 = 0x0006;

/// STUN parser focused *only* on extracting the USERNAME attribute value.
///
/// It performs minimal validation necessary to safely find the attribute.
/// It returns a slice `&[u8]` referencing the username within the input `data`
/// buffer, achieving zero-copy.
///
/// Returns `Some(&[u8])` if the USERNAME attribute is found, `None` otherwise
/// (due to parsing errors, malformed data, or the attribute not being present).
///
/// # Arguments
///
/// * `data`: A byte slice representing the potential STUN message.
///
/// # Performance Considerations
///
/// *   **Zero-Copy:** Returns `&[u8]`.
/// *   **Minimal Checks:** Skips validation of Transaction ID, other attributes, etc.
/// *   **No Allocations:** Does not allocate memory on the heap.
/// *   **Inline Hint:** Suggests inlining for potential performance gains in tight loops.
#[inline]
pub fn find_stun_username_slice(data: &[u8]) -> Option<&[u8]> {
    // 1. Check minimum length for STUN header
    if data.len() < MIN_STUN_HEADER_SIZE {
        return None;
    }

    // 2. Check Message Type indicator (first two bits must be 00)
    if data[0] & 0b1100_0000 != 0 {
        return None;
    }

    // 3. Check Magic Cookie (bytes 4-7)
    // We know data.len() >= 20, so slicing data[4..8] is safe.
    if data[4..8] != MAGIC_COOKIE_BYTES {
        return None;
    }

    // 4. Read Message Length (bytes 2-3) - Big Endian
    // This is the length of the attributes *only*.
    // Performance: from_be_bytes is efficient. try_into().unwrap() is safe
    // because we checked data.len() >= 20.
    let message_length = u16::from_be_bytes(
        data[2..4].try_into().unwrap(), // Safe slice and unwrap
    ) as usize;

    // 5. Validate overall length
    // The total expected size (header + attributes) must not exceed the buffer size.
    let expected_total_len = MIN_STUN_HEADER_SIZE.checked_add(message_length)?;
    if data.len() < expected_total_len {
        // Not enough data in the buffer according to the header's length field.
        return None;
    }

    // Optimization: If message_length is 0, no attributes exist.
    if message_length == 0 {
        return None;
    }

    // --- Attribute Parsing ---
    let mut current_pos = MIN_STUN_HEADER_SIZE;
    // Define the boundary for attribute data based *only* on the declared message_length.
    // We trust message_length (after the data.len() check above) to define the attribute region.
    let attributes_end = expected_total_len; // MIN_STUN_HEADER_SIZE + message_length;

    while current_pos < attributes_end {
        // Check if there's enough space for the attribute header (Type + Length)
        // Need at least 4 bytes remaining *within the declared attribute region*.
        if current_pos.checked_add(ATTRIBUTE_HEADER_SIZE)? > attributes_end {
            // Malformed: Not enough space left for an attribute header
            // This could happen if the last attribute's padding calculation was wrong
            // or if message_length implied space but the data buffer was actually shorter
            // (although the earlier `data.len() < expected_total_len` check should catch the latter)
            // or if the message_length is non-zero but doesn't even cover a single attribute header.
            return None;
        }

        // Read Attribute Type (Big Endian)
        // Slicing is safe due to the check above.
        let attr_type = u16::from_be_bytes(
            data[current_pos..current_pos + 2].try_into().unwrap(), // Safe
        );

        // Read Attribute Value Length (Big Endian)
        // Slicing is safe due to the check above.
        let attr_value_len = u16::from_be_bytes(
            data[current_pos + 2..current_pos + 4].try_into().unwrap(), // Safe
        ) as usize;

        // Calculate start position of the attribute value
        let value_pos = current_pos + ATTRIBUTE_HEADER_SIZE;

        // Check if the attribute value (based on its *own* length field) fits
        // within the bounds defined by the *message* length field.
        let end_of_value = value_pos.checked_add(attr_value_len)?;
        if end_of_value > attributes_end {
            // Malformed: Attribute claims to be longer than the remaining message length allows
            return None;
        }

        // *** Check if this is the USERNAME attribute ***
        if attr_type == USERNAME_ATTRIBUTE_TYPE {
            // Found it! Return a slice pointing to the value.
            // Slicing is safe because we checked:
            // value_pos + attr_value_len <= attributes_end <= data.len()
            return Some(&data[value_pos..end_of_value]);
        }

        // Move to the next attribute.
        // Value must be padded to a multiple of 4 bytes.
        // Performance: Bitwise trick for padding calculation is fast.
        let padded_len = (attr_value_len + 3) & !3;
        // Check for overflow before updating current_pos
        let next_pos = value_pos.checked_add(padded_len)?;

        // This check is crucial: ensure moving to the next attribute position,
        // considering padding, does not exceed the declared attribute boundary.
        // If next_pos == attributes_end, the loop condition `while current_pos < attributes_end`
        // will handle termination correctly on the next iteration.
        // If next_pos > attributes_end, it means padding calculation resulted in exceeding
        // the allowed space, indicating a malformed message.
        if next_pos > attributes_end {
            return None; // Padding pushed beyond declared message length
        }
        current_pos = next_pos;

        // The loop condition `while current_pos < attributes_end` inherently checks
        // if we have landed exactly on the boundary or gone past it.
    }

    // If the loop finishes, we either processed all attributes correctly
    // without finding USERNAME, or the message was potentially malformed
    // in a way that didn't trigger earlier checks but caused the loop
    // to terminate (e.g., current_pos landing exactly on attributes_end after processing
    // the last attribute).
    // In either case, USERNAME was not found.
    None
}

/// Convenience wrapper around `find_stun_username_slice` that attempts
/// to convert the resulting byte slice into a UTF-8 `&str`.
///
/// Returns `Some(&str)` if the USERNAME attribute is found and contains valid
/// UTF-8 data, `None` otherwise.
#[inline]
pub fn parse_stun_username_str(data: &[u8]) -> Option<&str> {
    find_stun_username_slice(data)
        // Attempt UTF-8 conversion. .ok() converts Result<_, Utf8Error> to Option<_>
        .and_then(|slice| std::str::from_utf8(slice).ok())
}

/// Similar to `parse_stun_username_str`, but it only takes the remote ufrag.
///
/// https://datatracker.ietf.org/doc/html/rfc8445#section-7.2.2
///
/// Stun binding from L to R. The username is formatted as "<R's ufrag>:<L's ufrag>"
///
/// Returns `Some(&str)` if the USERNAME attribute is found, returns <R's ufrag>
#[inline]
pub fn parse_stun_remote_ufrag(data: &[u8]) -> Option<&str> {
    find_stun_username_slice(data)
        .and_then(|slice| first_token(slice, b':'))
        .and_then(|raw| std::str::from_utf8(raw).ok())
}

#[inline]
fn first_token(input: &[u8], delimiter: u8) -> Option<&[u8]> {
    for (i, &b) in input.iter().enumerate() {
        if b == delimiter {
            return Some(&input[..i]);
        }
    }
    None
}

// --- Robust Test Suite ---
#[cfg(test)]
mod tests {
    use super::*;

    const BINDING_REQUEST: u16 = 0x0001;
    const DUMMY_TX_ID: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    const MESSAGE_INTEGRITY_ATTRIBUTE_TYPE: u16 = 0x0008;
    const PRIORITY_ATTRIBUTE_TYPE: u16 = 0x0024;

    /// Helper to build a STUN message for testing.
    fn build_stun_message(
        msg_type: u16,
        tx_id: [u8; 12],
        attributes: &[(u16, &[u8])], // List of (Type, Value)
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128); // Pre-allocate some space

        // Write header (placeholder length first)
        buf.extend_from_slice(&msg_type.to_be_bytes());
        buf.extend_from_slice(&[0u8; 2]); // Placeholder for length
        buf.extend_from_slice(&MAGIC_COOKIE_BYTES);
        buf.extend_from_slice(&tx_id);

        assert_eq!(buf.len(), MIN_STUN_HEADER_SIZE);

        // Add attributes and calculate total length
        let mut total_attr_len: usize = 0;
        for (attr_type, attr_value) in attributes {
            let attr_value_len = attr_value.len();
            // Check for unreasonable attribute length during test construction
            assert!(
                attr_value_len <= u16::MAX as usize,
                "Test attribute too long"
            );

            buf.extend_from_slice(&attr_type.to_be_bytes());
            buf.extend_from_slice(&(attr_value_len as u16).to_be_bytes());
            buf.extend_from_slice(attr_value);

            // Add padding
            let padded_len = (attr_value_len + 3) & !3;
            let padding_len = padded_len - attr_value_len;
            buf.extend_from_slice(&vec![0u8; padding_len]);

            total_attr_len += ATTRIBUTE_HEADER_SIZE + padded_len;
        }

        // Check for unreasonable total length during test construction
        assert!(
            total_attr_len <= u16::MAX as usize,
            "Total attribute length too long for test"
        );

        // Write the actual message length (attribute part only)
        buf[2..4].copy_from_slice(&(total_attr_len as u16).to_be_bytes());

        buf
    }

    // --- Happy Path Tests ---

    #[test]
    fn test_valid_username_first() {
        let username = b"user1";
        let priority_val = 0x6e0001ff_u32.to_be_bytes();
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[
                (USERNAME_ATTRIBUTE_TYPE, username),
                (PRIORITY_ATTRIBUTE_TYPE, &priority_val),
            ],
        );
        assert_eq!(find_stun_username_slice(&msg), Some(username as &[u8]));
        assert_eq!(parse_stun_username_str(&msg), Some("user1"));
    }

    #[test]
    fn test_valid_username_last() {
        let username = b"user2";
        let priority_val = 0x7f000000_u32.to_be_bytes();
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[
                (PRIORITY_ATTRIBUTE_TYPE, &priority_val),
                (USERNAME_ATTRIBUTE_TYPE, username),
            ],
        );
        assert_eq!(find_stun_username_slice(&msg), Some(username as &[u8]));
        assert_eq!(parse_stun_username_str(&msg), Some("user2"));
    }

    #[test]
    fn test_valid_username_middle() {
        let username = b"user3-middle"; // Length 12 (multiple of 4)
        let priority_val = 0x12345678_u32.to_be_bytes();
        let integrity_val = [0u8; 20]; // SHA1 HMAC size
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[
                (PRIORITY_ATTRIBUTE_TYPE, &priority_val),
                (USERNAME_ATTRIBUTE_TYPE, username),
                (MESSAGE_INTEGRITY_ATTRIBUTE_TYPE, &integrity_val),
            ],
        );
        assert_eq!(find_stun_username_slice(&msg), Some(username as &[u8]));
        assert_eq!(parse_stun_username_str(&msg), Some("user3-middle"));
    }

    #[test]
    fn test_valid_username_needs_padding() {
        let username = b"user4pad"; // Length 8 (multiple of 4) - correct
        let username_needs_pad = b"user5pad"; // Length 9 (needs 3 bytes padding)
        let priority_val = 0xabcdef01_u32.to_be_bytes();
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[
                (PRIORITY_ATTRIBUTE_TYPE, &priority_val),
                (USERNAME_ATTRIBUTE_TYPE, username_needs_pad), // This one needs padding
                (USERNAME_ATTRIBUTE_TYPE, username), // This one doesn't, testing search after padded one
            ],
        );
        // Should find the *first* username attribute encountered
        assert_eq!(
            find_stun_username_slice(&msg),
            Some(username_needs_pad as &[u8])
        );
        assert_eq!(parse_stun_username_str(&msg), Some("user5pad"));
    }

    #[test]
    fn test_valid_username_only_attribute() {
        let username = b"only-user"; // 9 bytes -> pads to 12
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[(USERNAME_ATTRIBUTE_TYPE, username)],
        );
        assert_eq!(find_stun_username_slice(&msg), Some(username as &[u8]));
        assert_eq!(parse_stun_username_str(&msg), Some("only-user"));
    }

    #[test]
    fn test_zero_length_username() {
        let username = b"";
        let priority_val = 0x11223344_u32.to_be_bytes();
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[
                (USERNAME_ATTRIBUTE_TYPE, username), // Zero length username
                (PRIORITY_ATTRIBUTE_TYPE, &priority_val),
            ],
        );
        assert_eq!(find_stun_username_slice(&msg), Some(username as &[u8]));
        assert_eq!(parse_stun_username_str(&msg), Some(""));
    }

    // --- Missing/Absent Username Tests ---

    #[test]
    fn test_no_username_attribute() {
        let priority_val = 0x1_u32.to_be_bytes();
        let integrity_val = [1u8; 20];
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[
                (PRIORITY_ATTRIBUTE_TYPE, &priority_val),
                (MESSAGE_INTEGRITY_ATTRIBUTE_TYPE, &integrity_val),
            ],
        );
        assert_eq!(find_stun_username_slice(&msg), None);
        assert_eq!(parse_stun_username_str(&msg), None);
    }

    #[test]
    fn test_no_attributes_at_all() {
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[], // Empty attribute list
        );
        // Header should indicate length 0
        assert_eq!(&msg[2..4], &[0u8, 0u8]);
        assert_eq!(msg.len(), MIN_STUN_HEADER_SIZE);
        assert_eq!(find_stun_username_slice(&msg), None);
        assert_eq!(parse_stun_username_str(&msg), None);
    }

    // --- Malformed Header / Basic Checks Failures ---

    #[test]
    fn test_too_short_for_header() {
        let data = &[0u8; MIN_STUN_HEADER_SIZE - 1];
        assert_eq!(find_stun_username_slice(data), None);
    }

    #[test]
    fn test_invalid_message_type_bits() {
        let mut msg = build_stun_message(BINDING_REQUEST, DUMMY_TX_ID, &[]);
        msg[0] = 0b0100_0000; // Set first bit
        assert_eq!(find_stun_username_slice(&msg), None);
        msg[0] = 0b1000_0000; // Set second bit
        assert_eq!(find_stun_username_slice(&msg), None);
        msg[0] = 0b1100_0000; // Set both bits
        assert_eq!(find_stun_username_slice(&msg), None);
    }

    #[test]
    fn test_invalid_magic_cookie() {
        let mut msg = build_stun_message(BINDING_REQUEST, DUMMY_TX_ID, &[]);
        msg[4] = 0xFF; // Corrupt first byte of cookie
        assert_eq!(find_stun_username_slice(&msg), None);
        msg[4] = MAGIC_COOKIE_BYTES[0]; // Restore
        msg[7] = 0xEE; // Corrupt last byte of cookie
        assert_eq!(find_stun_username_slice(&msg), None);
    }

    #[test]
    fn test_message_length_exceeds_data_length() {
        let username = b"test";
        let mut msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[(USERNAME_ATTRIBUTE_TYPE, username)], // Length should be 4+4=8
        );
        // Manually set message length to something larger than actual data size
        let declared_len: u16 = (msg.len() - MIN_STUN_HEADER_SIZE + 1) as u16;
        msg[2..4].copy_from_slice(&declared_len.to_be_bytes());

        assert_eq!(find_stun_username_slice(&msg), None);
    }

    #[test]
    fn test_message_length_zero_but_data_has_attrs() {
        let username = b"test";
        // Build correctly first
        let mut msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[(USERNAME_ATTRIBUTE_TYPE, username)],
        );
        // Manually set message length to 0
        msg[2..4].copy_from_slice(&0u16.to_be_bytes());
        // Parser should exit early based on length field
        assert_eq!(find_stun_username_slice(&msg), None);
    }

    // --- Malformed Attributes / Attribute Parsing Failures ---

    #[test]
    fn test_message_length_cuts_off_attr_header() {
        // Goal: message_length allows the *first* attribute completely,
        // but is too short to hold the *header* of the second attribute (USERNAME).

        // Attribute 1: PRIORITY (non-target)
        let priority_val = &[1, 2, 3, 4]; // Len 4, padded len 4. Total size 4+4=8.
        // Attribute 2: USERNAME (target)
        let username = b"test"; // Len 4, padded len 4. Total size 4+4=8.

        // Total correct attr len = 8 + 8 = 16.
        let mut msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[
                (PRIORITY_ATTRIBUTE_TYPE, priority_val), // First attribute (processed)
                (USERNAME_ATTRIBUTE_TYPE, username),     // Second attribute (header cut off)
            ],
        );
        // Sanity check length
        assert_eq!(u16::from_be_bytes(msg[2..4].try_into().unwrap()), 16);
        assert_eq!(msg.len(), MIN_STUN_HEADER_SIZE + 16); // 36 bytes total

        // Set message length to cut off the *header* of the second attribute.
        // First attribute (PRIORITY) occupies bytes 20..28 (Header 20..24, Value 24..28).
        // It needs 8 bytes.
        // The next attribute header would start at index 28 and needs 4 bytes (up to index 32).
        // If message_length is set such that attributes_end is < 32, the check should fail
        // when trying to read the second header.
        // Let's set message_length to 10 (covers first attr [8 bytes] + 2 bytes).
        // This means attributes_end = 20 + 10 = 30.
        let bad_len: u16 = 10;
        msg[2..4].copy_from_slice(&bad_len.to_be_bytes());

        // --- Expected Trace ---
        // 1. Parse PRIORITY attribute (current_pos=20). Fits (value ends at 28 <= 30). Not USERNAME.
        // 2. Calculate next_pos = 28. Update current_pos = 28.
        // 3. Loop again (current_pos=28). Check if enough space for next header:
        //    current_pos(28) + ATTRIBUTE_HEADER_SIZE(4) = 32.
        //    Is 32 > attributes_end(30)? YES. -> Return None.

        // The assertion should now pass.
        assert_eq!(
            find_stun_username_slice(&msg),
            None,
            "Parser should return None because message_length cuts off the second attribute's header"
        );

        // Optional: Also test with truncated buffer (up to declared length)
        assert_eq!(
            find_stun_username_slice(&msg[..MIN_STUN_HEADER_SIZE + bad_len as usize]),
            None,
            "Parser should return None even when buffer is pre-truncated"
        );
    }

    #[test]
    fn test_attribute_length_exceeds_message_boundary() {
        let username = b"user"; // Len 4, padded len 4. Attr size = 4+4=8
        // Build correctly first
        let mut msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[(USERNAME_ATTRIBUTE_TYPE, username)],
        );
        // Sanity check
        assert_eq!(u16::from_be_bytes(msg[2..4].try_into().unwrap()), 8); // 4(type/len) + 4(value) = 8
        assert_eq!(msg.len(), MIN_STUN_HEADER_SIZE + 8);

        // Manually change the *attribute's* length field to claim more than allowed
        // The message length is 8. Attribute region is bytes 20..28.
        // Attribute header is at 20..24. Value starts at 24.
        // If attr claims length 5, end_of_value = 24 + 5 = 29. attributes_end = 28.
        // 29 > 28, so should fail.
        msg[22..24].copy_from_slice(&5u16.to_be_bytes()); // Set attr length to 5

        assert_eq!(find_stun_username_slice(&msg), None);
    }

    #[test]
    fn test_attribute_value_plus_padding_exceeds_boundary() {
        // Purpose: Test that the parser returns None when the calculated position
        //          for the *next* attribute (after accounting for the current
        //          attribute's value padding) exceeds the boundary set by the
        //          message header's length field.

        // To test this specific check, the USERNAME attribute cannot be found
        // *before* the attribute whose padding causes the failure.
        // Therefore, we use a placeholder attribute first.

        // Attribute 1: Placeholder (e.g., SOFTWARE)
        const SOFTWARE_ATTRIBUTE_TYPE: u16 = 0x8022; // Example placeholder type
        let placeholder_val = b"NonUsernameData"; // Length 15, pads to 16. Total size = 4 + 16 = 20 bytes.

        // Attribute 2: The one whose padding should cause failure (e.g., PRIORITY)
        let priority_val = &[1u8, 2, 3]; // Length 3, pads to 4. Total size = 4 + 4 = 8 bytes.

        // Build message with placeholder first, then priority.
        // Correct total attribute length = 20 + 8 = 28 bytes.
        let mut msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[
                (SOFTWARE_ATTRIBUTE_TYPE, placeholder_val),
                (PRIORITY_ATTRIBUTE_TYPE, priority_val),
            ],
        );
        // Sanity check that the message was built with the correct length initially
        assert_eq!(
            u16::from_be_bytes(msg[2..4].try_into().unwrap()),
            28,
            "Initial build length mismatch"
        );
        assert_eq!(
            msg.len(),
            MIN_STUN_HEADER_SIZE + 28,
            "Initial buffer size mismatch"
        ); // Total size 48

        // --- Modify message length to trigger the failure condition ---
        // Trace execution to determine required bad_len:
        // attributes_start = 20.
        // Process Placeholder: current=20. value_pos=24. end_of_value=24+15=39. Not USERNAME.
        //   padded_len = 16. next_pos = 24 + 16 = 40. Set current_pos = 40.
        // Process Priority: current=40. value_pos=44. end_of_value=44+3=47. Not USERNAME.
        //   padded_len = 4. next_pos = 44 + 4 = 48.
        // We need the padding check `if next_pos > attributes_end` to trigger for the PRIORITY attribute.
        // So, we need `48 > attributes_end`. Let's set `attributes_end = 47`.
        // `attributes_end = attributes_start + message_length`
        // `47 = 20 + message_length` => `message_length` needs to be 27.
        let bad_len: u16 = 27;
        msg[2..4].copy_from_slice(&bad_len.to_be_bytes());
        // Now attributes_end = 20 + 27 = 47.

        // --- Execute and Assert ---
        // Expected trace with attributes_end = 47:
        // Process Placeholder: current=20. value_pos=24. end_of_value=39. (39<=47 OK). Not USERNAME.
        //   padded_len=16. next_pos=40. (40<=47 OK). current_pos=40.
        // Process Priority: current=40. value_pos=44. end_of_value=44+3=47. (47<=47 OK). Not USERNAME.
        //   padded_len=4. next_pos=48.
        //   Check padding boundary: if next_pos(48) > attributes_end(47) -> TRUE. Return None.
        assert_eq!(
            find_stun_username_slice(&msg),
            None,
            "Parser should return None because PRIORITY padding exceeds reduced message length (expected attributes_end=47)"
        );

        // Optional: Also test with the explicitly truncated buffer
        assert_eq!(
            find_stun_username_slice(&msg[..MIN_STUN_HEADER_SIZE + bad_len as usize]),
            None,
            "Parser should also return None on pre-truncated slice (expected attributes_end=47)"
        );
    }

    #[test]
    fn test_malformed_attribute_data_but_username_found_first() {
        // Ensure that if the USERNAME is found, we don't parse further potentially
        // malformed attributes.
        let username = b"find-me-first";
        let priority_val = &[1, 2, 3, 4, 5, 6, 7]; // Valid value for this test
        // Build correctly initially
        let mut msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[
                (USERNAME_ATTRIBUTE_TYPE, username), // Length 13 -> pads to 16. Size 4+16=20.
                (PRIORITY_ATTRIBUTE_TYPE, priority_val), // Length 7 -> pads to 8. Size 4+8=12.
            ], // Total length = 20+12 = 32
        );
        // Sanity check
        assert_eq!(u16::from_be_bytes(msg[2..4].try_into().unwrap()), 32);
        assert_eq!(msg.len(), MIN_STUN_HEADER_SIZE + 32);

        // Now, corrupt the *length* field of the second attribute (PRIORITY)
        // First attr ends at 20 + 20 = 40. Second header starts at 40. Its length is at 42..44.
        // Let's set its length to be huge, exceeding the message boundary.
        msg[42..44].copy_from_slice(&1000u16.to_be_bytes());

        // The parser should find the USERNAME attribute and return immediately,
        // *without* hitting the error when parsing the second attribute.
        assert_eq!(find_stun_username_slice(&msg), Some(username as &[u8]));
    }

    // --- UTF-8 Handling (`parse_stun_username_str`) ---

    #[test]
    fn test_valid_utf8_username_str() {
        let username = "gültig€".as_bytes(); // Valid UTF-8 string
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[(USERNAME_ATTRIBUTE_TYPE, username)],
        );
        assert_eq!(find_stun_username_slice(&msg), Some(username)); // Slice check
        assert_eq!(parse_stun_username_str(&msg), Some("gültig€")); // String check
    }

    #[test]
    fn test_invalid_utf8_username_str() {
        let invalid_utf8_bytes = &[0x80, 0xFF]; // Invalid UTF-8 sequence
        let msg = build_stun_message(
            BINDING_REQUEST,
            DUMMY_TX_ID,
            &[(USERNAME_ATTRIBUTE_TYPE, invalid_utf8_bytes)],
        );
        // The slice should still be found correctly
        assert_eq!(
            find_stun_username_slice(&msg),
            Some(invalid_utf8_bytes as &[u8])
        );
        // But converting to &str should fail
        assert_eq!(parse_stun_username_str(&msg), None);
    }

    #[test]
    fn test_ice_binding_from_raw() {
        let packet_hex = "000100502112a4426943746c7a422f4d706e594800060009447172673a35504c4d000000c0570004000003e7802a0008e2e197300acfe8da00250000002400046e001eff00080014b610b03b8165bb4c317192054e00c73afb204dd480280004a953f217";
        let packet_raw = hex::decode(packet_hex).unwrap();

        assert_eq!(parse_stun_username_str(&packet_raw), Some("Dqrg:5PLM"));
        assert_eq!(parse_stun_remote_ufrag(&packet_raw), Some("Dqrg"));
    }

    #[test]
    fn test_first_token() {
        assert_eq!(first_token(b"asd", b':'), None);
        assert_eq!(first_token(b"asd:", b':'), Some(b"asd".as_slice()));
        assert_eq!(first_token(b"asd:bcd", b':'), Some(b"asd".as_slice()));
    }
}
