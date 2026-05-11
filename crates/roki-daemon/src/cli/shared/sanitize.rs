//! Terminal-output sanitization: ANSI strip + control-char strip.
//! Re-uses the VTE walker in [`crate::api::sanitize::strip_ansi`].

pub fn strip_for_terminal(input: &str) -> String {
    let ansi_stripped = crate::api::sanitize::strip_ansi(input);
    ansi_stripped
        .chars()
        .filter(|c| !is_disallowed_control(*c))
        .collect()
}

fn is_disallowed_control(c: char) -> bool {
    let cp = c as u32;
    // Keep tab, LF, CR; drop other C0 / DEL / C1 control codes.
    if matches!(c, '\t' | '\n' | '\r') {
        return false;
    }
    (cp < 0x20) || (0x7f..=0x9f).contains(&cp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_for_terminal("\x1b[31mok\x1b[0m"), "ok");
    }

    #[test]
    fn preserves_newline_tab_cr() {
        assert_eq!(strip_for_terminal("a\nb\tc\rd"), "a\nb\tc\rd");
    }

    #[test]
    fn drops_c0_controls() {
        assert_eq!(strip_for_terminal("\x07bell\x00null"), "bellnull");
    }
}
