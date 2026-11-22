use std::io::Write;

use flate2::{Compression, write::ZlibEncoder};
use gcv::bgcode::{DecodeLimits, decode_bgcode, decode_bgcode_with_limits};

// The binary fixtures encode this text using libbgcode's MeatPack encoder,
// heatshrink2, or zlib, independently of the decoder under test.
const REFERENCE: &str = include_str!("fixtures/bgcode/reference.gcode");

fn header(version: u32, crc: bool) -> Vec<u8> {
    let mut out = b"GCDE".to_vec();
    out.extend(version.to_le_bytes());
    out.extend(u16::from(crc).to_le_bytes());
    out
}

fn block(
    kind: u16,
    compression: u16,
    raw_size: usize,
    params: &[u8],
    payload: &[u8],
    crc: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(kind.to_le_bytes());
    out.extend(compression.to_le_bytes());
    out.extend((raw_size as u32).to_le_bytes());
    if compression != 0 {
        out.extend((payload.len() as u32).to_le_bytes());
    }
    out.extend(params);
    out.extend(payload);
    if crc {
        out.extend(crc32fast::hash(&out).to_le_bytes());
    }
    out
}

fn file(compression: u16, encoding: u16, raw_size: usize, payload: &[u8]) -> Vec<u8> {
    let mut out = header(1, true);
    out.extend(block(
        1,
        compression,
        raw_size,
        &encoding.to_le_bytes(),
        payload,
        true,
    ));
    out
}

fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn independent_reference_fixtures_decode_all_compression_modes() {
    let fixtures: [&[u8]; 5] = [
        include_bytes!("fixtures/bgcode/raw-v1.bgcode"),
        include_bytes!("fixtures/bgcode/deflate-v1.bgcode"),
        include_bytes!("fixtures/bgcode/heatshrink11-meatpack-v1.bgcode"),
        include_bytes!("fixtures/bgcode/heatshrink12-meatpack-v1.bgcode"),
        include_bytes!("fixtures/bgcode/heatshrink12-meatpack-v2.bgcode"),
    ];
    for fixture in fixtures {
        let text = decode_bgcode(fixture).unwrap();
        assert!(text.starts_with("; bed_shape=0x0,250x0,250x210,0x210\n"));
        let gcode = text.lines().skip(2).collect::<Vec<_>>().join("\n");
        assert_eq!(squash(&gcode), squash(REFERENCE));
    }
}

#[test]
fn meatpack_without_comments_matches_reference_encoder() {
    let text = decode_bgcode(include_bytes!(
        "fixtures/bgcode/heatshrink12-no-comments-v1.bgcode"
    ))
    .unwrap();
    let expected = REFERENCE
        .lines()
        .filter(|line| !line.starts_with(';'))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        squash(&text.lines().skip(2).collect::<Vec<_>>().join("\n")),
        squash(&expected)
    );
}

#[test]
fn no_checksum_and_multiple_blocks_preserve_command_boundaries() {
    let mut bytes = header(1, false);
    bytes.extend(block(1, 0, 6, &[0, 0], b"G1 X10", false));
    bytes.extend(block(1, 0, 5, &[0, 0], b" Y20\n", false));
    assert_eq!(decode_bgcode(&bytes).unwrap(), "G1 X10 Y20\n");
}

#[test]
fn empty_gcode_block_is_valid() {
    assert_eq!(decode_bgcode(&file(0, 0, 0, b"")).unwrap(), "");
    // This is the form emitted by libbgcode's empty-block writer.
    assert_eq!(decode_bgcode(&file(1, 0, 0, b"")).unwrap(), "");
    assert_eq!(decode_bgcode(&file(2, 0, 0, b"")).unwrap(), "");
    assert_eq!(decode_bgcode(&file(3, 0, 0, b"")).unwrap(), "");
}

#[test]
fn every_truncated_reference_file_is_rejected() {
    for bytes in [
        include_bytes!("fixtures/bgcode/raw-v1.bgcode").as_slice(),
        include_bytes!("fixtures/bgcode/heatshrink12-meatpack-v1.bgcode").as_slice(),
    ] {
        for cut in 0..bytes.len() {
            assert!(
                decode_bgcode(&bytes[..cut]).is_err(),
                "accepted truncation at {cut}"
            );
        }
    }
}

#[test]
fn crc_detects_corruption_in_metadata_and_gcode() {
    let original = include_bytes!("fixtures/bgcode/raw-v1.bgcode");
    for position in [30, original.len() - 5, original.len() - 1] {
        let mut bytes = original.to_vec();
        bytes[position] ^= 0x01;
        assert!(decode_bgcode(&bytes).unwrap_err().message.contains("CRC32"));
    }
}

#[test]
fn unknown_header_fields_and_block_parameters_are_rejected() {
    let original = file(0, 0, 3, b"G1\n");
    for (pos, value, message) in [
        (0, b'X', "magic"),
        (4, 3, "version"),
        (8, 2, "checksum"),
        (10, 6, "block type"),
        (12, 4, "compression"),
    ] {
        let mut bytes = original.clone();
        bytes[pos] = value;
        assert!(decode_bgcode(&bytes).unwrap_err().message.contains(message));
    }
    assert!(
        decode_bgcode(&file(0, 3, 3, b"G1\n"))
            .unwrap_err()
            .message
            .contains("encoding")
    );
}

#[test]
fn input_sizes_and_block_counts_are_bounded_before_allocation() {
    let bytes = file(0, 0, 3, b"G1\n");
    let limits = DecodeLimits {
        max_block_bytes: 2,
        ..DecodeLimits::default()
    };
    assert!(
        decode_bgcode_with_limits(&bytes, limits)
            .unwrap_err()
            .message
            .contains("size limit")
    );
    let limits = DecodeLimits {
        max_total_bytes: 2,
        ..DecodeLimits::default()
    };
    assert!(
        decode_bgcode_with_limits(&bytes, limits)
            .unwrap_err()
            .message
            .contains("size limit")
    );
    let limits = DecodeLimits {
        max_blocks: 0,
        ..DecodeLimits::default()
    };
    assert!(
        decode_bgcode_with_limits(&bytes, limits)
            .unwrap_err()
            .message
            .contains("too many")
    );
    let mut huge = header(1, true);
    huge.extend([1, 0, 0, 0]);
    huge.extend(u32::MAX.to_le_bytes());
    assert!(
        decode_bgcode(&huge)
            .unwrap_err()
            .message
            .contains("size limit")
    );
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn deflate_requires_exact_size_complete_stream_and_no_trailing_bytes() {
    let payload = zlib(b"G1 X20 Y20\n");
    for size in [0, 10, 12, 20] {
        assert!(decode_bgcode(&file(1, 0, size, &payload)).is_err());
    }
    assert!(decode_bgcode(&file(1, 0, 11, &payload[..payload.len() - 1])).is_err());
    let mut trailing = payload.clone();
    trailing.push(0);
    assert!(decode_bgcode(&file(1, 0, 11, &trailing)).is_err());
    assert_eq!(
        decode_bgcode(&file(1, 0, 11, &payload)).unwrap(),
        "G1 X20 Y20\n"
    );
    assert_eq!(decode_bgcode(&file(1, 0, 0, &zlib(b""))).unwrap(), "");
}

fn bits(tokens: &[(usize, usize)]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut count = 0;
    for &(value, width) in tokens {
        for shift in (0..width).rev() {
            if count % 8 == 0 {
                result.push(0);
            }
            let index = result.len() - 1;
            result[index] |= (((value >> shift) & 1) as u8) << (7 - count % 8);
            count += 1;
        }
    }
    result
}

#[test]
fn heatshrink_overlapping_copy_and_zero_initialized_window() {
    for (mode, width) in [(2, 11), (3, 12)] {
        // Literal 'A', then copy four bytes from distance one; the copy overlaps.
        let packed = bits(&[(1, 1), (b'A' as usize, 8), (0, 1), (0, width), (3, 4)]);
        assert_eq!(decode_bgcode(&file(mode, 0, 5, &packed)).unwrap(), "AAAAA");
        assert!(
            decode_bgcode(&file(mode, 0, 4, &packed))
                .unwrap_err()
                .message
                .contains("declared size")
        );
        // A back-reference into the initial ring is legal and yields zero bytes.
        let zeros = bits(&[(0, 1), (0, width), (2, 4)]);
        assert_eq!(
            decode_bgcode(&file(mode, 0, 3, &zeros)).unwrap().as_bytes(),
            &[0, 0, 0]
        );
    }
}

#[test]
fn heatshrink_rejects_trailing_tokens_nonzero_padding_and_truncation() {
    let packed = bits(&[(1, 1), (b'G' as usize, 8)]);
    assert_eq!(decode_bgcode(&file(3, 0, 1, &packed)).unwrap(), "G");
    let mut trailing = packed.clone();
    trailing.push(0);
    assert!(decode_bgcode(&file(3, 0, 1, &trailing)).is_err());
    let mut padding = packed.clone();
    padding[1] |= 1;
    assert!(decode_bgcode(&file(3, 0, 1, &padding)).is_err());
    assert!(decode_bgcode(&file(3, 0, 2, &packed)).is_err());
    assert!(decode_bgcode(&file(3, 0, 1, &packed[..1])).is_err());
}

#[test]
fn heatshrink_matches_can_cross_the_start_of_output_at_any_bit_alignment() {
    for (mode, width) in [(2, 11), (3, 12)] {
        for literals in 1..=8 {
            let mut tokens = vec![(0x141, 9); literals]; // literal A
            // Read two zeroes before the literal prefix, then repeat that pattern.
            tokens.extend([(literals + 1, width + 1), (15, 4)]);
            let pattern = [vec![0; 2], vec![b'A'; literals]].concat();
            let mut expected = vec![b'A'; literals];
            expected.extend(pattern.iter().copied().cycle().take(16));
            let packed = bits(&tokens);
            let text = decode_bgcode(&file(mode, 0, expected.len(), &packed)).unwrap();
            assert_eq!(text.as_bytes(), expected);
            assert!(
                decode_bgcode(&file(mode, 0, expected.len(), &packed[..packed.len() - 1])).is_err()
            );
        }
    }
}

#[test]
fn meatpack_escapes_nibble_order_and_end_of_line_padding() {
    let packed = [
        255, 255, 251, 0x1d, 0xfe, b'Y', 0x6f, b'A', 0xff, b'B', b'C', 0x0c,
    ];
    assert_eq!(
        decode_bgcode(&file(0, 2, packed.len(), &packed)).unwrap(),
        "G1XYA6BC\n"
    );
}

#[test]
fn meatpack_state_commands_and_utf8_comments() {
    let mut packed = vec![
        255, 255, 251, 255, 255, 247, 0x1b, 255, 255, 246, 0x1b, 255, 255, 248, 255, 255, 250,
    ];
    packed.extend("; café\n".as_bytes());
    packed.extend([255, 255, 251, 0x1d, 0xcc, 255, 255, 249]);
    packed.extend(b"G90\n");
    assert_eq!(
        decode_bgcode(&file(0, 2, packed.len(), &packed)).unwrap(),
        "E1 1; café\nG1\nG90\n"
    );
}

#[test]
fn malformed_meatpack_is_rejected() {
    for payload in [
        vec![255, 255],
        vec![255, 255, 251, 0xff],
        vec![255, 255, 251, 0x1f],
        vec![255, 255, 251, 0x1f, 255, 255, 250],
        vec![255, 255, 252],
    ] {
        assert!(decode_bgcode(&file(0, 2, payload.len(), &payload)).is_err());
    }
}

#[test]
fn raw_text_must_be_utf8() {
    let err = decode_bgcode(&file(0, 0, 1, &[255])).unwrap_err();
    assert!(err.message.contains("UTF-8"));
}

#[test]
fn metadata_is_commented_and_cannot_execute_commands() {
    let mut bytes = header(1, true);
    let metadata = b"bed_x=250\nG92 X999\n";
    bytes.extend(block(2, 0, metadata.len(), &[0, 0], metadata, true));
    bytes.extend(block(1, 0, 1, &[0, 0], b"G", true));
    let json = br#"{"printer":{"bed_shape":"0x0,250x0,250x210,0x210","max_print_height":220},"note":"test\nG92 E999"}"#;
    bytes.extend(block(3, 0, json.len(), &[1, 0], json, true));
    bytes.extend(block(1, 0, 2, &[0, 0], b"1\n", true));
    let text = decode_bgcode(&bytes).unwrap();
    assert!(text.contains("; bed_shape = 0x0,250x0,250x210,0x210\n"));
    assert!(text.contains("; G92 X999\n"));
    assert!(text.contains("; G92 E999\n"));
    assert_eq!(
        text.lines()
            .filter(|l| !l.starts_with(';'))
            .collect::<Vec<_>>(),
        ["G1"]
    );
}

#[test]
fn thumbnail_content_is_validated_and_not_emitted() {
    let mut bytes = header(1, true);
    let thumbnail = b"fake image bytes: G92 E1000\n";
    bytes.extend(block(
        5,
        1,
        thumbnail.len(),
        &[0, 0, 16, 0, 16, 0],
        &zlib(thumbnail),
        true,
    ));
    bytes.extend(block(1, 0, 3, &[0, 0], b"G1\n", true));
    assert_eq!(decode_bgcode(&bytes).unwrap(), "G1\n");
    bytes[30] ^= 1;
    assert!(decode_bgcode(&bytes).unwrap_err().message.contains("CRC32"));
}

#[test]
fn parallel_binary_blocks_preserve_text_and_report_the_first_error() {
    let mut bytes = header(1, false);
    let thumbnail = vec![0; 512 * 1024];
    bytes.extend(block(
        5,
        0,
        thumbnail.len(),
        &[0, 0, 16, 0, 16, 0],
        &thumbnail,
        false,
    ));
    let append = |dst: &mut Vec<u8>, kind, raw: &[u8]| {
        dst.extend(block(kind, 1, raw.len(), &[0, 0], &zlib(raw), false))
    };
    append(&mut bytes, 1, b"G1 X1");
    append(&mut bytes, 2, b"printer=sample\n");
    append(&mut bytes, 1, b"0\n");
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    assert_eq!(
        pool.install(|| decode_bgcode(&bytes)).unwrap(),
        "; printer=sample\nG1 X10\n"
    );

    let offset = bytes.len();
    append(&mut bytes, 1, &[255]); // Valid compression, invalid G-code text.
    bytes.extend(block(1, 2, 4, &[0, 0], &[], false)); // Invalid compression.
    bytes.push(255); // Incomplete header.
    let error = pool.install(|| decode_bgcode(&bytes)).unwrap_err();
    assert_eq!(error.offset, offset);
    assert!(error.message.contains("UTF-8"));
}

#[test]
fn meatpack_text_budget_can_run_out_before_a_bad_command() {
    let mut bytes = header(1, false);
    let prefix = vec![b' '; 512 * 1024];
    bytes.extend(block(1, 0, prefix.len(), &[0, 0], &prefix, false));
    let offset = bytes.len();
    let mut packed = vec![255, 255, 251];
    packed.extend([0x11; 40]);
    packed.extend([255, 255, 0]);
    bytes.extend(block(1, 0, packed.len(), &[2, 0], &packed, false));
    bytes.extend(block(1, 0, 1, &[2, 0], b"G", false));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    for (limit, expected) in [
        (prefix.len() + packed.len() + 1, "size limit"),
        (usize::MAX, "unsupported MeatPack command"),
    ] {
        let error = pool
            .install(|| {
                decode_bgcode_with_limits(
                    &bytes,
                    DecodeLimits {
                        max_total_bytes: limit,
                        ..Default::default()
                    },
                )
            })
            .unwrap_err();
        assert_eq!(error.offset, offset);
        assert!(error.message.contains(expected));
    }
}

#[test]
fn no_data_blocks_is_an_error() {
    assert!(
        decode_bgcode(&header(1, true))
            .unwrap_err()
            .message
            .contains("no G-code")
    );
}
