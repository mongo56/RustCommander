//! User commands (`[[user_commands]]` in `config.toml`): placeholder
//! expansion in the command line and its splitting into program and arguments.
//!
//! Placeholders: `%P` the active panel's folder, `%T` the other panel's,
//! `%N` the cursor entry's name, `%F` its full path, `%S` the selected full
//! paths (the cursor entry's when nothing is selected), `%%` a `%`. Values
//! are put in double quotes when they are empty, contain white space or end
//! in a backslash (written `"C:\\"`, so the closing quote isn't escaped for
//! Windows' argument parsing, see [`split_args`]). Unknown `%x` stay as they are.

/// What the placeholders stand for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vars {
    /// `%P`
    pub active: String,
    /// `%T`
    pub other: String,
    /// `%N`: empty when there is no cursor entry.
    pub name: String,
    /// `%F`: empty when there is no cursor entry.
    pub file: String,
    /// `%S`: the selected full paths; empty = use `file`.
    pub selected: Vec<String>,
}

/// Quotes `s` for a command line when needed (see the module docs).
pub fn quote(s: &str) -> String {
    let plain = !s.is_empty() && !s.ends_with('\\') && !s.chars().any(char::is_whitespace);
    if plain {
        return s.to_owned();
    }
    let body = s.trim_end_matches('\\');
    let slashes = s.len() - body.len();
    format!("\"{body}{}\"", "\\".repeat(slashes * 2))
}

/// The placeholder letters at `template` in order, `%%` and unknown ones left out.
pub fn used(template: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(l @ ('P' | 'T' | 'N' | 'F' | 'S')) = chars.next() {
                out.push(l);
            }
        }
    }
    out
}

/// `template` with the placeholders replaced.
pub fn expand(template: &str, vars: &Vars) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some('P') => {
                chars.next();
                out.push_str(&quote(&vars.active));
            }
            Some('T') => {
                chars.next();
                out.push_str(&quote(&vars.other));
            }
            Some('N') => {
                chars.next();
                out.push_str(&quote(&vars.name));
            }
            Some('F') => {
                chars.next();
                out.push_str(&quote(&vars.file));
            }
            Some('S') => {
                chars.next();
                if vars.selected.is_empty() {
                    out.push_str(&quote(&vars.file));
                } else {
                    let list: Vec<String> = vars.selected.iter().map(|p| quote(p)).collect();
                    out.push_str(&list.join(" "));
                }
            }
            _ => out.push('%'),
        }
    }
    out
}

/// Splits a command line into words the way Windows' `CommandLineToArgvW`
/// does: white space separates, double quotes group (and are removed),
/// backslashes are literal unless they come right before a quote: `2n` of
/// them and a quote give `n` backslashes and the quote groups, `2n+1` give
/// `n` backslashes and a literal quote. `""` is an empty word.
pub fn split_args(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let mut n = 1;
                while chars.peek() == Some(&'\\') {
                    chars.next();
                    n += 1;
                }
                started = true;
                if chars.peek() == Some(&'"') {
                    word.extend(std::iter::repeat_n('\\', n / 2));
                    if n % 2 == 1 {
                        chars.next();
                        word.push('"');
                    }
                } else {
                    word.extend(std::iter::repeat_n('\\', n));
                }
            }
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        Vars {
            active: r"C:\My Files".into(),
            other: r"D:\other".into(),
            name: "a b.txt".into(),
            file: r"C:\My Files\a b.txt".into(),
            selected: vec![],
        }
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(quote(r"C:\x\y.txt"), r"C:\x\y.txt");
        assert_eq!(quote(r"C:\x y\z"), r#""C:\x y\z""#);
        assert_eq!(quote(""), r#""""#);
    }

    #[test]
    fn trailing_backslash_is_doubled_inside_quotes() {
        assert_eq!(quote(r"C:\"), r#""C:\\""#);
        assert_eq!(quote(r"D:\My Files\"), r#""D:\My Files\\""#);
        let line = expand("tool %P", &Vars { active: r"C:\".into(), ..Vars::default() });
        assert_eq!(line, r#"tool "C:\\""#);
        // ... and splits back into the original path.
        assert_eq!(split_args(&line), ["tool", r"C:\"]);
    }

    #[test]
    fn expands_placeholders() {
        let v = vars();
        assert_eq!(expand("x %P %T", &v), r#"x "C:\My Files" D:\other"#);
        assert_eq!(expand("open %N %F", &v), r#"open "a b.txt" "C:\My Files\a b.txt""#);
        assert_eq!(expand("100%% done %%P", &v), "100% done %P");
        assert_eq!(expand("a%x %q 5%", &v), "a%x %q 5%");
        assert_eq!(expand("%", &v), "%");
    }

    #[test]
    fn selection_falls_back_to_the_cursor_entry() {
        let mut v = vars();
        assert_eq!(expand("z %S", &v), r#"z "C:\My Files\a b.txt""#);
        v.selected = vec![r"C:\My Files\one.txt".into(), r"C:\My Files\two".into(), r"C:\f\3".into()];
        assert_eq!(expand("z %S", &v), r#"z "C:\My Files\one.txt" "C:\My Files\two" C:\f\3"#);
        // Nothing at all: one empty argument.
        assert_eq!(expand("z %S", &Vars::default()), r#"z """#);
    }

    #[test]
    fn lists_the_placeholders_used() {
        assert_eq!(used("a %P %% %F %x %S %"), ['P', 'F', 'S']);
        assert!(used("plain").is_empty());
    }

    #[test]
    fn splits_like_windows() {
        assert_eq!(split_args(r#""C:\Program Files\x.exe" -a  "b c""#), [r"C:\Program Files\x.exe", "-a", "b c"]);
        assert_eq!(split_args(r#"x "" y"#), ["x", "", "y"]);
        assert_eq!(split_args(r#"x "C:\a b\\" y"#), ["x", r"C:\a b\", "y"]);
        assert_eq!(split_args(r#"x \"q\" C:\d\e"#), ["x", r#""q""#, r"C:\d\e"]);
        assert!(split_args("   ").is_empty());
    }
}
