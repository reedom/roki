use serde_json::Value;

/// ANSI-strip + HTML-escape.
pub fn clean_text(input: &str) -> String {
    let stripped = strip_ansi(input);
    html_escape::encode_text(&stripped).into_owned()
}

/// Same as [`clean_text`] but tolerates non-UTF-8 bytes by replacing them
/// with U+FFFD. Returns the field name when a replacement happened, so the
/// caller can log the offending field.
pub fn clean_text_or_placeholder(
    field_name: &'static str,
    raw: &[u8],
) -> (String, Option<&'static str>) {
    match std::str::from_utf8(raw) {
        Ok(s) => (clean_text(s), None),
        Err(_) => {
            let s: String = String::from_utf8_lossy(raw).into_owned();
            (clean_text(&s), Some(field_name))
        }
    }
}

/// Apply [`clean_text`] to every string leaf of a JSON value in place.
pub fn clean_json(value: &mut Value) {
    match value {
        Value::String(s) => *s = clean_text(s),
        Value::Array(arr) => arr.iter_mut().for_each(clean_json),
        Value::Object(map) => map.iter_mut().for_each(|(_, v)| clean_json(v)),
        _ => {}
    }
}

fn strip_ansi(input: &str) -> String {
    struct Stripper {
        out: String,
    }
    impl vte::Perform for Stripper {
        fn print(&mut self, c: char) {
            self.out.push(c);
        }
        fn execute(&mut self, b: u8) {
            if matches!(b, b'\n' | b'\t' | b'\r') {
                self.out.push(b as char);
            }
        }
        fn put(&mut self, _b: u8) {}
        fn unhook(&mut self) {}
        fn osc_dispatch(&mut self, _params: &[&[u8]], _bell: bool) {}
        fn csi_dispatch(
            &mut self,
            _params: &vte::Params,
            _intermediates: &[u8],
            _ignore: bool,
            _action: char,
        ) {
        }
        fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
        fn hook(
            &mut self,
            _params: &vte::Params,
            _intermediates: &[u8],
            _ignore: bool,
            _action: char,
        ) {
        }
    }
    let mut perf = Stripper {
        out: String::with_capacity(input.len()),
    };
    let mut parser = vte::Parser::new();
    for byte in input.bytes() {
        parser.advance(&mut perf, byte);
    }
    perf.out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_ansi_color_codes() {
        let s = "\x1b[31mred\x1b[0m";
        assert_eq!(clean_text(s), "red");
    }

    #[test]
    fn html_escapes_brackets() {
        assert_eq!(clean_text("<script>"), "&lt;script&gt;");
    }

    #[test]
    fn preserves_newlines_and_tabs() {
        assert_eq!(clean_text("a\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn json_walker_cleans_string_leaves_only() {
        let mut v = json!({
            "a": "<x>",
            "b": [1, "\x1b[31mred"],
            "c": {"d": "ok"},
        });
        clean_json(&mut v);
        assert_eq!(
            v,
            json!({
                "a": "&lt;x&gt;",
                "b": [1, "red"],
                "c": {"d": "ok"},
            })
        );
    }

    #[test]
    fn invalid_utf8_returns_placeholder_marker() {
        let raw = b"abc\xff\xfe";
        let (out, marker) = clean_text_or_placeholder("title", raw);
        assert_eq!(marker, Some("title"));
        assert!(out.contains("abc"));
    }
}
