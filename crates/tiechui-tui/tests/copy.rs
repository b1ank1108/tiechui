#![allow(clippy::unwrap_used)]

use base64::{Engine, engine::general_purpose::STANDARD};
use tiechui_tui::copy::encode;

fn decode(sequence: &str) -> String {
    let encoded = sequence
        .strip_prefix("\x1b]52;c;")
        .unwrap()
        .strip_suffix("\x1b\\")
        .unwrap();
    String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap()
}

#[test]
fn osc52_encodes_utf8_for_the_clipboard_with_st_termination() {
    let text = "  中文 e\u{301} 👩‍💻\nnext line";
    assert_eq!(decode(&encode(text, false).unwrap()), text);
    assert_eq!(encode("hello", false).unwrap(), "\x1b]52;c;aGVsbG8=\x1b\\");
}

#[test]
fn tmux_sends_plain_then_dcs_with_each_inner_escape_doubled() {
    assert_eq!(
        encode("hello", true).unwrap(),
        "\x1b]52;c;aGVsbG8=\x1b\\\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x1b\x1b\\\x1b\\"
    );
}

#[test]
fn budget_counts_utf8_bytes_before_base64_and_never_truncates() {
    for tmux in [false, true] {
        for text in ["a".repeat(100_000), "界".repeat(33_333) + "a"] {
            let sequence = encode(&text, tmux).unwrap();
            let plain = sequence.split("\x1bPtmux;").next().unwrap();
            assert_eq!(decode(plain), text);
            assert!(encode(&(text + "a"), tmux).is_none());
        }
        assert!(encode(&"界".repeat(33_334), tmux).is_none());
    }
}
