//! Defense-in-depth sanitization on TUI received strings (fr:11
//! §Defense-in-depth sanitization). Even though the server already sanitizes,
//! we strip again before storing or rendering.

use anstyle_parse::{DefaultCharAccumulator, Params, Parser, Perform};

/// Remove ANSI escape sequences (CSI / OSC / SGR / etc.) while preserving
/// printable bytes. Operates on &str; callers reject invalid UTF-8 upstream
/// (see `client::get_text`, which returns `ClientError::InvalidUtf8`).
pub fn ansi_strip(s: &str) -> String {
    struct Sink {
        out: String,
    }
    impl Perform for Sink {
        fn print(&mut self, c: char) {
            self.out.push(c);
        }
        fn execute(&mut self, byte: u8) {
            // Preserve newline + tab; drop other C0 controls + DEL.
            if byte == b'\n' || byte == b'\t' {
                self.out.push(byte as char);
            }
        }
        fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: u8) {}
        fn put(&mut self, _: u8) {}
        fn unhook(&mut self) {}
        fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {}
        fn csi_dispatch(&mut self, _: &Params, _: &[u8], _: bool, _: u8) {}
        fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {}
    }
    let mut parser = Parser::<DefaultCharAccumulator>::new();
    let mut sink = Sink {
        out: String::with_capacity(s.len()),
    };
    for &byte in s.as_bytes() {
        parser.advance(&mut sink, byte);
    }
    sink.out
}

/// Drop C0 control characters except \n and \t plus DEL (0x7F).
pub fn control_strip(s: &str) -> String {
    s.chars()
        .filter(|c| {
            let cp = *c as u32;
            !(cp < 0x20 && cp != 0x09 && cp != 0x0A) && cp != 0x7F
        })
        .collect()
}

/// Recursively `ansi_strip` + `control_strip` every string leaf inside a JSON
/// value in place. Non-string leaves are untouched.
pub fn clean_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            *s = control_strip(&ansi_strip(s));
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(clean_json),
        serde_json::Value::Object(map) => map.values_mut().for_each(clean_json),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi() {
        let s = "\x1b[31mhello\x1b[0m world";
        assert_eq!(ansi_strip(s), "hello world");
    }

    #[test]
    fn strips_osc_with_bel_terminator() {
        let s = "before\x1b]0;title\x07after";
        assert_eq!(ansi_strip(s), "beforeafter");
    }

    #[test]
    fn keeps_newlines_and_tabs() {
        let s = "a\nb\tc";
        assert_eq!(ansi_strip(s), "a\nb\tc");
    }

    #[test]
    fn ansi_strip_handles_utf8_boundary() {
        let s = "\u{1F600}\x1b[31mhi";
        assert_eq!(ansi_strip(s), "\u{1F600}hi");
    }

    #[test]
    fn control_strip_drops_bel_and_null() {
        let s = "a\x00b\x07c";
        assert_eq!(control_strip(s), "abc");
    }

    #[test]
    fn control_strip_keeps_newline_tab() {
        assert_eq!(control_strip("x\ny\tz"), "x\ny\tz");
    }

    #[test]
    fn control_strip_drops_del() {
        assert_eq!(control_strip("a\x7Fb"), "ab");
    }

    #[test]
    fn clean_json_walks_nested() {
        let mut v = serde_json::json!({
            "a": "\x1b[31mhi\x07",
            "b": [{"c": "\x00x"}, 1, null]
        });
        clean_json(&mut v);
        assert_eq!(v["a"], serde_json::Value::String("hi".into()));
        assert_eq!(v["b"][0]["c"], serde_json::Value::String("x".into()));
        assert_eq!(v["b"][1], serde_json::json!(1));
        assert_eq!(v["b"][2], serde_json::Value::Null);
    }
}
