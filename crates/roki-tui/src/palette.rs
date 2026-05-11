//! Terminal palette detection (fr:11 §Terminal compatibility).
//!
//! Detection order, first match wins:
//! 1. $COLORTERM ∈ {truecolor, 24bit} → Rgb24
//! 2. $TERM matches *-256color           → IndexedAnsi256
//! 3. otherwise                           → IndexedAnsi16

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    Rgb24,
    IndexedAnsi256,
    IndexedAnsi16,
}

impl Palette {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rgb24 => "rgb24",
            Self::IndexedAnsi256 => "indexed_ansi256",
            Self::IndexedAnsi16 => "indexed_ansi16",
        }
    }
}

pub trait EnvProbe {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct ProcessEnv;
impl EnvProbe for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

pub fn detect() -> Palette {
    detect_with(&ProcessEnv)
}

pub fn detect_with(env: &dyn EnvProbe) -> Palette {
    if let Some(v) = env.get("COLORTERM") {
        let v = v.to_ascii_lowercase();
        if v == "truecolor" || v == "24bit" {
            return Palette::Rgb24;
        }
    }
    if let Some(t) = env.get("TERM") {
        if t.ends_with("-256color") {
            return Palette::IndexedAnsi256;
        }
    }
    Palette::IndexedAnsi16
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapEnv(HashMap<&'static str, &'static str>);
    impl EnvProbe for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).map(|s| (*s).to_string())
        }
    }

    fn env(pairs: &[(&'static str, &'static str)]) -> MapEnv {
        MapEnv(pairs.iter().copied().collect())
    }

    #[test]
    fn colorterm_truecolor_wins() {
        assert_eq!(
            detect_with(&env(&[("COLORTERM", "truecolor"), ("TERM", "xterm")])),
            Palette::Rgb24
        );
    }

    #[test]
    fn term_256color_falls_back_to_indexed() {
        assert_eq!(
            detect_with(&env(&[("TERM", "xterm-256color")])),
            Palette::IndexedAnsi256
        );
    }

    #[test]
    fn dumb_falls_back_to_16() {
        assert_eq!(detect_with(&env(&[("TERM", "dumb")])), Palette::IndexedAnsi16);
    }

    #[test]
    fn empty_env_falls_back_to_16() {
        assert_eq!(detect_with(&env(&[])), Palette::IndexedAnsi16);
    }

    #[test]
    fn colorterm_case_insensitive() {
        assert_eq!(
            detect_with(&env(&[("COLORTERM", "TrueColor")])),
            Palette::Rgb24
        );
    }
}
