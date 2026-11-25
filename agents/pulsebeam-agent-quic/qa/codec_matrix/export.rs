use std::{
    env,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use pulsebeam_agent_quic::media::{Opus, PacketBudget};
use pulsebeam_rtc::{RtpExtensionForm, RtpHeader};
use serde_json::Value;
use std::time::Duration;
use wire_capture::{Cursor, PacketKind};

use super::{
    CASES, Case,
    filtered::{FAMILY_CASES, Fault, family_trace, replay_output, target_for},
};

fn emit(
    dir: &Path,
    name: &str,
    case: &Case,
    observed: &Value,
    target: u8,
    switch: Option<(usize, u8)>,
    fault: Fault<'_>,
) {
    let path = dir.join(format!("{}-{}-{name}.rtp", case.codec, case.mode));
    let mut output = BufWriter::new(File::create(path).unwrap());
    let mut packet = Vec::with_capacity(1200);
    let units = replay_output(
        case,
        observed,
        target,
        switch,
        fault,
        |sequence, timestamp, marker, payload, dd| {
            let header = RtpHeader {
                sequence: sequence as u16,
                timestamp,
                ssrc: 0x506c7363,
                payload_type: 120,
                marker,
            };
            assert!(
                header.write_packet(
                    RtpExtensionForm::Mixed,
                    std::iter::once((9, dd)),
                    payload,
                    false,
                    &mut packet
                ),
                "{} {} packet {sequence}",
                case.codec,
                case.mode
            );
            output
                .write_all(&u16::try_from(packet.len()).unwrap().to_be_bytes())
                .unwrap();
            output.write_all(&packet).unwrap();
        },
    );
    assert!(!units.is_empty(), "{} {} {name}", case.codec, case.mode);
    output.flush().unwrap();
}

fn emit_opus(dir: &Path, name: &str, wire: &[u8], index: &str) {
    let mut output = BufWriter::new(File::create(dir.join(format!("OPUS-{name}.rtp"))).unwrap());
    let mut cursor = Cursor::new(wire, index).unwrap();
    let packetizer = Opus::new(PacketBudget::new(1147, 0).unwrap());
    let mut wire = Vec::with_capacity(1147);
    let mut sequence = u32::from(u16::MAX) - 3;
    while let Some(packet) = cursor.next_due(Duration::from_secs(6)) {
        if packet.kind != PacketKind::Rtp {
            continue;
        }
        let timestamp = u32::from_be_bytes(packet.bytes[4..8].try_into().unwrap());
        let marker = packet.bytes[1] & 0x80 != 0;
        let packed = packetizer
            .packetize(sequence, timestamp, marker, &packet.bytes[12..])
            .unwrap();
        let header = RtpHeader {
            sequence: packed.sequence as u16,
            timestamp: packed.timestamp,
            ssrc: 0x506c7363,
            payload_type: 111,
            marker: packed.marker,
        };
        assert!(header.write_packet(
            RtpExtensionForm::Mixed,
            std::iter::empty(),
            packed.payload,
            false,
            &mut wire
        ));
        output
            .write_all(&(wire.len() as u16).to_be_bytes())
            .unwrap();
        output.write_all(&wire).unwrap();
        sequence += 1;
    }
    assert_eq!(sequence - (u32::from(u16::MAX) - 3), 300);
    output.flush().unwrap();
}

pub(super) fn run() {
    let args: Vec<_> = env::args_os().collect();
    let dir = Path::new(&args[1]);
    let only_codec = args.get(2).and_then(|s| s.to_str());
    let only_mode = args.get(3).and_then(|s| s.to_str());
    if only_codec.is_none() {
        macro_rules! opus_capture {
            ($stem:literal) => {
                emit_opus(
                    dir,
                    $stem,
                    include_bytes!(concat!(
                        env!("CODEC_FIXTURES"),
                        "/opus-v1/",
                        $stem,
                        "/wire.bin"
                    )),
                    include_str!(concat!(
                        env!("CODEC_FIXTURES"),
                        "/opus-v1/",
                        $stem,
                        "/wire.tsv"
                    )),
                );
            };
        }
        opus_capture!("quality_a0_48k_mono");
        opus_capture!("quality_a1_48k_mono");
    }
    for case in CASES {
        if only_codec.is_some_and(|codec| codec != case.codec)
            || only_mode.is_some_and(|mode| mode != case.mode)
        {
            continue;
        }
        let observed: Value = serde_json::from_str(case.observations).unwrap();
        if FAMILY_CASES.contains(&(case.codec, case.mode)) {
            let trace = family_trace(case, &observed);
            emit(
                dir,
                "switch",
                case,
                &observed,
                trace.base,
                Some((trace.restart_packet, trace.target)),
                Fault::None,
            );
            emit(
                dir,
                "loss",
                case,
                &observed,
                trace.target,
                None,
                Fault::Lost(trace.lost_packet),
            );
        }
        if let Some(targets) = observed["fixed_targets"].as_array() {
            for witness in targets {
                let spatial = witness["spatial"].as_u64().unwrap_or(0) as u8;
                let temporal = witness["temporal"].as_u64().unwrap() as u8;
                emit(
                    dir,
                    &format!("fixed-{spatial}-{temporal}"),
                    case,
                    &observed,
                    target_for(case, spatial, temporal),
                    None,
                    Fault::None,
                );
            }
        } else {
            assert_eq!(case.codec, "VP9");
            for spatial in [0, 1] {
                emit(
                    dir,
                    &format!("fixed-{spatial}-0"),
                    case,
                    &observed,
                    target_for(case, spatial, 0),
                    None,
                    Fault::None,
                );
            }
        }
    }
}
