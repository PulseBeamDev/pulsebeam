use alloc::vec::Vec;

use super::*;

const CLOCK: Clock<'static> = Clock {
    id: "camera",
    epoch: 7,
};

fn hex(text: &str) -> Result<Vec<u8>, &'static str> {
    if !text.len().is_multiple_of(2) {
        return Err("odd number of hex digits");
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = core::str::from_utf8(pair).map_err(|_| "not UTF-8")?;
            u8::from_str_radix(pair, 16).map_err(|_| "not hex")
        })
        .collect()
}

fn fixture(bytes: &[u8], header: Header<'_>, payload: &[u8]) -> Result<(), Error> {
    let clock = header.capture.map(|capture| capture.clock);
    let decoded = Record::decode(bytes, clock)?;
    assert_eq!(decoded.header(), header);
    assert_eq!(decoded.bytes(), payload);
    let created = Record::new(header, payload, clock)?;
    assert_eq!(created, decoded);
    let mut output = [0x5a; MAX_RECORD_BYTES + 1];
    let len = created.write(&mut output)?;
    assert_eq!(created.encoded_len(), bytes.len());
    assert_eq!(output.get(..len), Some(bytes));
    assert_eq!(output.get(len), Some(&0x5a));
    Ok(())
}

fn header(unit_seq: u64, index: u16, count: u16, unit_len: u32) -> Header<'static> {
    Header {
        unit_seq,
        index,
        count,
        unit_len,
        capture: None,
    }
}

#[test]
fn endpoint_descriptor_moves_out_of_record_and_back() -> Result<(), &'static str> {
    let bare = hex("50440100000000000000000000000001000000000000")?;
    let endpoint = hex("5044010400000000000000000000000100000000000003c00000")?;
    let record = Record::decode(&endpoint, None).map_err(|_| "endpoint decode")?;
    assert_eq!(record.dependency_descriptor(), Some(&[0xc0, 0, 0][..]));
    assert_eq!(
        Record::decode_without_descriptor(&endpoint, None),
        Err(Error::Flags)
    );
    assert_eq!(wire_identity(&endpoint), Err(Error::Flags));
    assert_eq!(
        Record::decode(&bare, None),
        Record::decode_without_descriptor(&bare, None)
    );
    let mut out = [0x5a; 27];
    let len = record
        .without_dependency_descriptor()
        .write(&mut out)
        .map_err(|_| "WAN payload")?;
    assert_eq!(out.get(..len), Some(bare.as_slice()));
    let len = Record::decode_without_descriptor(&bare, None)
        .and_then(|record| record.with_dependency_descriptor(&[0xc0, 0, 0]))
        .and_then(|record| record.write(&mut out))
        .map_err(|_| "reconstruct endpoint")?;
    assert_eq!(out.get(..len), Some(endpoint.as_slice()));
    assert_eq!(out.get(len), Some(&0x5a));
    Ok(())
}

#[test]
fn descriptor_and_clock_share_one_bounded_record() -> Result<(), Error> {
    let mut header = header(42, 0, 1, 1_024);
    header.capture = Some(Capture {
        clock: CLOCK,
        nanos: -4,
        uncertainty_ns: Some(12),
    });
    let payload = [0x61; FRAGMENT_BYTES];
    let descriptor = [0x15; MAX_DESCRIPTOR_BYTES];
    let record =
        Record::new(header, &payload, Some(CLOCK))?.with_dependency_descriptor(&descriptor)?;
    let mut out = [0; MAX_ENDPOINT_RECORD_BYTES];
    assert_eq!(record.write(&mut out)?, MAX_ENDPOINT_RECORD_BYTES);
    assert_eq!(out.get(3), Some(&7));
    assert_eq!(out.get(34), Some(&96));
    assert_eq!(Record::decode(&out, Some(CLOCK)), Ok(record));
    assert_eq!(Record::decode(&out, None), Err(Error::Clock));
    let mut canonical = [0; MAX_RECORD_BYTES];
    let split = split_endpoint_record(&out, &mut canonical)?;
    assert_eq!(split.descriptor, Some(descriptor.as_slice()));
    assert_eq!(split.identity, wire_identity(split.payload)?);
    assert_eq!(
        Record::decode_without_descriptor(split.payload, Some(CLOCK)),
        Ok(record.without_dependency_descriptor())
    );
    let mut restored = [0; MAX_ENDPOINT_RECORD_BYTES];
    assert_eq!(
        restore_endpoint_record(split.payload, split.descriptor, &mut restored)?,
        out
    );
    assert!(restore_endpoint_record(&out, split.descriptor, &mut restored).is_err());
    for (offset, byte, error) in [
        (3, 6, Error::Flags),
        (3, 15, Error::Flags),
        (34, 2, Error::DescriptorLength),
        (34, 97, Error::DescriptorLength),
        (34, 95, Error::Length),
    ] {
        let mut invalid = out;
        *invalid.get_mut(offset).ok_or(Error::Length)? = byte;
        assert_eq!(Record::decode(&invalid, Some(CLOCK)), Err(error));
    }
    assert_eq!(Record::decode(&out[..34], Some(CLOCK)), Err(Error::Length));
    assert_eq!(Record::decode(&out[..35], Some(CLOCK)), Err(Error::Length));
    Ok(())
}

#[test]
fn appendix_b_r0_through_r5() -> Result<(), &'static str> {
    let r0 = hex("50440100000000000000000000000001000000000000")?;
    let r1 = hex("50440100000000000000000100000001000300000003616263")?;
    let r2 = hex("5044010300000000000000020000000100010000000100000000075bcd15000186a0ff")?;
    let mut r3 = hex("50440100000000000000000300000002040000000401")?;
    r3.extend([0x61; 1_024]);
    let r4 = hex("5044010000000000000000030001000200010000040162")?;
    let r5 = hex("50440101000000000000000400000001000000000000ffffffffffffffff")?;
    assert_eq!(
        [r0.len(), r1.len(), r2.len(), r3.len(), r4.len(), r5.len()],
        [22, 25, 35, 1_046, 23, 30]
    );

    fixture(&r0, header(0, 0, 1, 0), b"").map_err(|_| "R0")?;
    fixture(&r1, header(1, 0, 1, 3), b"abc").map_err(|_| "R1")?;
    let captured = Header {
        capture: Some(Capture {
            clock: CLOCK,
            nanos: 123_456_789,
            uncertainty_ns: Some(100_000),
        }),
        ..header(2, 0, 1, 1)
    };
    fixture(&r2, captured, &[0xff]).map_err(|_| "R2")?;
    fixture(&r3, header(3, 0, 2, 1_025), &[b'a'; 1_024]).map_err(|_| "R3")?;
    fixture(&r4, header(3, 1, 2, 1_025), b"b").map_err(|_| "R4")?;
    fixture(
        &r5,
        Header {
            capture: Some(Capture {
                clock: CLOCK,
                nanos: -1,
                uncertainty_ns: None,
            }),
            ..header(4, 0, 1, 0)
        },
        b"",
    )
    .map_err(|_| "R5")?;

    assert_eq!(Record::decode(&r2, None), Err(Error::Clock));
    assert_eq!(
        wire_identity(&r2),
        Ok(FragmentIdentity {
            unit_seq: captured.unit_seq,
            index: 0,
            count: 1
        })
    );
    assert_eq!(
        Record::new(captured, &[0xff], Some(Clock { epoch: 8, ..CLOCK })),
        Err(Error::Clock)
    );
    let mut illegal_flags = r2.clone();
    *illegal_flags.get_mut(3).ok_or("R2 flags")? = 2;
    assert_eq!(
        Record::decode(&illegal_flags, Some(CLOCK)),
        Err(Error::Flags)
    );
    let mut trailing = r2.clone();
    trailing.push(0);
    assert_eq!(Record::decode(&trailing, Some(CLOCK)), Err(Error::Length));
    assert_eq!(wire_identity(&trailing), Err(Error::Length));
    for len in 0..r2.len() {
        let prefix = r2.get(..len).ok_or("prefix")?;
        assert!(Record::decode(prefix, Some(CLOCK)).is_err());
        assert!(wire_identity(prefix).is_err());
    }
    let mut invalid_index = r4.clone();
    *invalid_index.get_mut(13).ok_or("R4 index")? = 2;
    assert_eq!(
        Record::decode(&invalid_index, None),
        Err(Error::FragmentIndex)
    );
    let mut invalid_length = r4;
    *invalid_length.get_mut(21).ok_or("R4 unit length")? = 0;
    assert_eq!(
        Record::decode(&invalid_length, None),
        Err(Error::FragmentCount)
    );
    Ok(())
}

#[test]
fn appendix_a_data_payloads_have_matching_labels_markers_and_lengths() -> Result<(), &'static str> {
    // D1–D4 and N1 have exactly a 24-byte RTP envelope. Their canonical RTP
    // envelope and the D2 -> N1 rewrite are also tested by pulsebeam-core.
    let d1 = hex("50440100000000000000ffff00000001000000000000")?;
    let d2_n1 = hex("504401000000000000010000000000010005000000056162636465")?;
    let mut d3 = hex("50440100000000000001000100000002040000000401")?;
    d3.extend([0x61; 1_024]);
    let d4 = hex("5044010000000000000100010001000200010000040162")?;
    for (bytes, label, marker) in [
        (d1, 65_535, true),
        (d2_n1, 65_536, true),
        (d3, 65_537, false),
        (d4, 65_537, true),
    ] {
        let record = Record::decode(&bytes, None).map_err(|_| "Appendix A DATA payload")?;
        assert_eq!(record.header().unit_seq, label);
        assert_eq!(record.final_fragment(), marker);
    }
    Ok(())
}

#[test]
fn maximum_unit_and_metadata_round_trip_without_touching_short_output() -> Result<(), Error> {
    let payload = [0x42; 1_024];
    let mut output = [0x5a; MAX_RECORD_BYTES];
    assert_eq!(fragment_count(0), Ok(1));
    assert_eq!(fragment_count(1_024), Ok(1));
    assert_eq!(fragment_count(1_025), Ok(2));
    assert_eq!(fragment_count(MAX_UNIT_BYTES), Ok(64));
    assert_eq!(fragment_count(MAX_UNIT_BYTES + 1), Err(Error::UnitLength));
    for index in 0..MAX_FRAGMENTS {
        let header = Header {
            capture: Some(Capture {
                clock: CLOCK,
                nanos: i64::MIN,
                uncertainty_ns: Some(u32::MAX),
            }),
            ..header(u64::MAX, index, 64, 65_536)
        };
        let record = Record::new(header, &payload, Some(CLOCK))?;
        let mut short = [0x5a; MAX_RECORD_BYTES - 1];
        assert_eq!(record.write(&mut short), Err(Error::Length));
        assert_eq!(short, [0x5a; MAX_RECORD_BYTES - 1]);
        assert_eq!(record.write(&mut output), Ok(MAX_RECORD_BYTES));
        assert_eq!(Record::decode(&output, Some(CLOCK)), Ok(record));
    }
    Ok(())
}
