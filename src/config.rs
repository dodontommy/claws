use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Clone, Debug)]
pub struct PrefixKey {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl PrefixKey {
    pub fn matches(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        // Match exact modifier set so that, e.g., a `ctrl-alt-x` config
        // doesn't accidentally fire on a plain `ctrl-x` keystroke.
        code == self.code && modifiers == self.modifiers
    }

    pub fn label(&self) -> String {
        let key_part = match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            _ => "?".to_string(),
        };
        let mut prefix = String::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            prefix.push_str("Ctrl-");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            prefix.push_str("Alt-");
        }
        format!("{prefix}{key_part}")
    }
}

impl Default for PrefixKey {
    fn default() -> Self {
        Self {
            code: KeyCode::Char(' '),
            modifiers: KeyModifiers::CONTROL,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub prefix: PrefixKey,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix: PrefixKey::default(),
        }
    }
}

/// Parse a prefix-key string of the form `<mod>-...-<key>`, where each
/// `<mod>` is one of `ctrl` or `alt` (each at most once, any order) and
/// `<key>` is either a single char or the literal `space`. At least one
/// modifier is required — bare keys like `space` or `a` would collide
/// with normal typing in attached mode and aren't useful as a prefix.
fn parse_prefix(s: &str) -> Option<PrefixKey> {
    let lower = s.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    let parts: Vec<&str> = lower.split('-').collect();
    if parts.len() < 2 {
        return None;
    }

    let (mods, key) = parts.split_at(parts.len() - 1);
    let key = key[0];

    let mut modifiers = KeyModifiers::NONE;
    for m in mods {
        match *m {
            "ctrl" if !modifiers.contains(KeyModifiers::CONTROL) => {
                modifiers |= KeyModifiers::CONTROL;
            }
            "alt" if !modifiers.contains(KeyModifiers::ALT) => {
                modifiers |= KeyModifiers::ALT;
            }
            // Unknown modifier, duplicate modifier, or empty token (e.g.
            // from `ctrl--a`).
            _ => return None,
        }
    }
    if modifiers.is_empty() {
        return None;
    }

    let code = match key {
        "space" => KeyCode::Char(' '),
        c if c.chars().count() == 1 => KeyCode::Char(c.chars().next().unwrap()),
        _ => return None,
    };

    Some(PrefixKey { code, modifiers })
}

pub fn load() -> Config {
    let path = match crate::paths::config_file() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "could not resolve config dir; using defaults");
            return Config::default();
        }
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Config::default(),
    };

    let table: toml::Table = match content.parse() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "failed to parse config");
            return Config::default();
        }
    };

    let mut config = Config::default();

    if let Some(keys) = table.get("keys").and_then(|v| v.as_table()) {
        if let Some(prefix_str) = keys.get("prefix").and_then(|v| v.as_str()) {
            match parse_prefix(prefix_str) {
                Some(pk) => config.prefix = pk,
                None => {
                    tracing::warn!(
                        value = prefix_str,
                        "invalid prefix key in config; using default (ctrl-space)"
                    );
                }
            }
        }
    }

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ctrl_space() {
        let pk = parse_prefix("ctrl-space").unwrap();
        assert_eq!(pk.code, KeyCode::Char(' '));
        assert_eq!(pk.modifiers, KeyModifiers::CONTROL);
        assert_eq!(pk.label(), "Ctrl-Space");
    }

    #[test]
    fn parse_ctrl_a() {
        let pk = parse_prefix("ctrl-a").unwrap();
        assert_eq!(pk.code, KeyCode::Char('a'));
        assert_eq!(pk.modifiers, KeyModifiers::CONTROL);
        assert_eq!(pk.label(), "Ctrl-a");
    }

    #[test]
    fn parse_ctrl_backslash() {
        let pk = parse_prefix("ctrl-\\").unwrap();
        assert_eq!(pk.code, KeyCode::Char('\\'));
        assert_eq!(pk.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_case_insensitive() {
        let pk = parse_prefix("Ctrl-Space").unwrap();
        assert_eq!(pk.code, KeyCode::Char(' '));
    }

    #[test]
    fn parse_alt_a() {
        let pk = parse_prefix("alt-a").unwrap();
        assert_eq!(pk.code, KeyCode::Char('a'));
        assert_eq!(pk.modifiers, KeyModifiers::ALT);
        assert_eq!(pk.label(), "Alt-a");
    }

    #[test]
    fn parse_alt_space() {
        let pk = parse_prefix("alt-space").unwrap();
        assert_eq!(pk.code, KeyCode::Char(' '));
        assert_eq!(pk.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parse_ctrl_alt_x() {
        let pk = parse_prefix("ctrl-alt-x").unwrap();
        assert_eq!(pk.code, KeyCode::Char('x'));
        assert_eq!(pk.modifiers, KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(pk.label(), "Ctrl-Alt-x");
    }

    #[test]
    fn parse_alt_ctrl_x_order_independent() {
        let pk = parse_prefix("alt-ctrl-x").unwrap();
        assert_eq!(pk.modifiers, KeyModifiers::CONTROL | KeyModifiers::ALT);
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert!(parse_prefix("space").is_none(), "bare key without modifier");
        assert!(parse_prefix("a").is_none(), "bare char without modifier");
        assert!(parse_prefix("shift-a").is_none(), "shift not supported");
        assert!(parse_prefix("ctrl-").is_none(), "trailing dash");
        assert!(parse_prefix("ctrl-ab").is_none(), "multi-char key");
        assert!(parse_prefix("ctrl-ctrl-a").is_none(), "duplicate modifier");
        assert!(parse_prefix("").is_none(), "empty");
        assert!(parse_prefix("ctrl--a").is_none(), "empty modifier token");
    }

    #[test]
    fn prefix_key_matches() {
        let pk = PrefixKey::default();
        assert!(pk.matches(KeyCode::Char(' '), KeyModifiers::CONTROL));
        assert!(!pk.matches(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!pk.matches(KeyCode::Char(' '), KeyModifiers::NONE));
    }

    #[test]
    fn ctrl_alt_doesnt_match_ctrl_alone() {
        // Modifier matching is exact: a `ctrl-alt-x` config must not fire
        // on a plain `ctrl-x` keystroke (which would be ambiguous and
        // surprising for a user who set the more specific binding).
        let pk = parse_prefix("ctrl-alt-x").unwrap();
        assert!(!pk.matches(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(pk.matches(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
    }

    #[test]
    fn default_config_has_ctrl_space() {
        let cfg = Config::default();
        assert_eq!(cfg.prefix.code, KeyCode::Char(' '));
        assert!(cfg.prefix.modifiers.contains(KeyModifiers::CONTROL));
    }
}
