use roki_tui::palette::{detect_with, EnvProbe, Palette};

struct DumbEnv;
impl EnvProbe for DumbEnv {
    fn get(&self, key: &str) -> Option<String> {
        if key == "TERM" { Some("dumb".into()) } else { None }
    }
}

#[test]
fn dumb_terminal_falls_back_to_16() {
    assert_eq!(detect_with(&DumbEnv), Palette::IndexedAnsi16);
}
