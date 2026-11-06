#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::os::unix::fs::PermissionsExt;

use serde_json::Value;
use tiechui_tools::output::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, OutputAccumulator, Policy, STRUCTURED_MAX_BYTES,
    format_size, truncate,
};

#[test]
fn head_and_tail_match_fixtures() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/truncation.json")).unwrap();
    for case in cases {
        let policy = if case["policy"] == "head" {
            Policy::Head
        } else {
            Policy::Tail
        };
        let actual = truncate(
            case["text"].as_str().unwrap(),
            policy,
            case["maxLines"].as_u64().unwrap() as usize,
            case["maxBytes"].as_u64().unwrap() as usize,
        );
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            case["expected"],
            "{case}"
        );
    }
}

#[test]
fn notice_sizes_use_javascript_rounding() {
    assert_eq!(format_size(51456), "50.3KB");
    assert_eq!(format_size(1310720), "1.3MB");
}

#[test]
fn exact_limits_preserve_final_newline_and_count_no_phantom_line() {
    for text in [
        "a\n".repeat(DEFAULT_MAX_LINES),
        "x".repeat(DEFAULT_MAX_BYTES),
    ] {
        let mut output = OutputAccumulator::new();
        output.append(text.as_bytes()).unwrap();
        output.finish().unwrap();
        let snapshot = output.snapshot();
        assert!(!snapshot.truncation.truncated);
        assert_eq!(snapshot.truncation.content, text);
        assert!(snapshot.full_output_path.is_none());
        assert_eq!(
            output.full_output(STRUCTURED_MAX_BYTES).unwrap(),
            (text, false)
        );
    }
}

#[test]
fn streaming_decoder_handles_every_utf8_split_bom_and_invalid_sequences() {
    let text = "\u{feff}铁锤🐈\né\n";
    for split in 0..=text.len() {
        let mut output = OutputAccumulator::new();
        output.append(&text.as_bytes()[..split]).unwrap();
        output.append(&text.as_bytes()[split..]).unwrap();
        output.finish().unwrap();
        assert_eq!(output.snapshot().truncation.content, "铁锤🐈\né\n");
        assert_eq!(
            output.full_output(STRUCTURED_MAX_BYTES).unwrap().0,
            "铁锤🐈\né\n"
        );
    }
    let mut output = OutputAccumulator::new();
    for byte in b"a\xff\xf0\x9f\x90\x88\xe2\x82" {
        output.append(&[*byte]).unwrap();
    }
    output.finish().unwrap();
    assert_eq!(output.snapshot().truncation.content, "a�🐈�");
    assert_eq!(output.full_output(STRUCTURED_MAX_BYTES).unwrap().0, "a�🐈�");
    assert!(output.append(b"late").is_err());
    output.finish().unwrap();
}

#[test]
fn line_spill_is_private_complete_and_has_notice() {
    let text = "a\n".repeat(DEFAULT_MAX_LINES + 1);
    let mut output = OutputAccumulator::new();
    for chunk in text.as_bytes().chunks(31) {
        output.append(chunk).unwrap();
    }
    output.finish().unwrap();
    let snapshot = output.snapshot();
    let path = snapshot.full_output_path.as_ref().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), text.as_bytes());
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(snapshot.truncation.total_lines, 2001);
    assert_eq!(snapshot.truncation.output_lines, 2000);
    assert_eq!(
        snapshot.text("(no output)"),
        format!(
            "{}\n\n[Showing lines 2-2001 of 2001. Full output: {}]",
            "a\n".repeat(1999) + "a",
            path.display()
        )
    );
    assert_eq!(
        output.full_output(STRUCTURED_MAX_BYTES).unwrap(),
        (text, false)
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn byte_spill_notices_cover_complete_and_partial_lines() {
    for (text, notice, output_bytes) in [
        (
            format!("{}\nlast\n", "x".repeat(DEFAULT_MAX_BYTES)),
            "Showing lines 2-2 of 2 (50.0KB limit)".to_string(),
            4,
        ),
        (
            "🐈".repeat(DEFAULT_MAX_BYTES + 1),
            "Showing last 50.0KB of line 1 (line is 200.0KB)".to_string(),
            DEFAULT_MAX_BYTES,
        ),
    ] {
        let mut output = OutputAccumulator::new();
        for chunk in text.as_bytes().chunks(17) {
            output.append(chunk).unwrap();
        }
        output.finish().unwrap();
        let snapshot = output.snapshot();
        let path = snapshot.full_output_path.as_ref().unwrap();
        assert_eq!(snapshot.truncation.output_bytes, output_bytes);
        assert!(
            snapshot
                .text("")
                .ends_with(&format!("\n\n[{notice}. Full output: {}]", path.display()))
        );
        assert_eq!(std::fs::read(path).unwrap(), text.as_bytes());
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn structured_output_keeps_head_tail_and_skips_split_codepoints() {
    let text = "🐈".repeat(STRUCTURED_MAX_BYTES / 4 + 2);
    let mut output = OutputAccumulator::new();
    output.append(text.as_bytes()).unwrap();
    output.finish().unwrap();
    let (full, truncated) = output.full_output(STRUCTURED_MAX_BYTES - 2).unwrap();
    assert!(truncated);
    assert_eq!(
        full,
        format!(
            "{}\n\n[... 10 bytes omitted ...]\n\n{}",
            "🐈".repeat(131071),
            "🐈".repeat(131071)
        )
    );
    let path = output.snapshot().full_output_path.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn spill_preserves_invalid_bytes_and_decoded_totals() {
    let raw = vec![0xff; DEFAULT_MAX_BYTES / 3 + 1];
    let mut output = OutputAccumulator::new();
    output.append(&raw).unwrap();
    output.finish().unwrap();
    let snapshot = output.snapshot();
    assert_eq!(snapshot.truncation.total_bytes, raw.len() * 3);
    assert!(snapshot.truncation.last_line_partial);
    let path = snapshot.full_output_path.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), raw);
    std::fs::remove_file(path).unwrap();
}
