use crossterm::event::{KeyCode, KeyModifiers};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct PrefixKey {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl PrefixKey {
    pub fn matches(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        code == self.code && modifiers.contains(self.modifiers)
    }

    pub fn label(&self) -> String {
        let key_part = match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            _ => "?".to_string(),
        };
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            format!("Ctrl-{key_part}")
        } else {
            key_part
        }
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

fn config_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .filter(|p| p.is_absolute())
        .or_else(|| dirs_home().map(|h| h.join(".config")))?;
    Some(base.join("claws").join("config.toml"))
}

fn dirs_home() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }
}

fn parse_prefix(s: &str) -> Option<PrefixKey> {
    let s = s.trim().to_lowercase();
    let parts: Vec<&str> = s.split('-').collect();

    if parts.len() == 2 && parts[0] == "ctrl" {
        let code = match parts[1] {
            "space" => KeyCode::Char(' '),
            c if c.len() == 1 => KeyCode::Char(c.chars().next().unwrap()),
            _ => return None,
        };
        Some(PrefixKey {
            code,
            modifiers: KeyModifiers::CONTROL,
        })
    } else {
        None
    }
}

pub fn load() -> Config {
    let path = match config_path() {
        Some(p) => p,
        None => return Config::default(),
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
        assert!(pk.modifiers.contains(KeyModifiers::CONTROL));
        assert_eq!(pk.label(), "Ctrl-Space");
    }

    #[test]
    fn parse_ctrl_a() {
        let pk = parse_prefix("ctrl-a").unwrap();
        assert_eq!(pk.code, KeyCode::Char('a'));
        assert!(pk.modifiers.contains(KeyModifiers::CONTROL));
        assert_eq!(pk.label(), "Ctrl-a");
    }

    #[test]
    fn parse_ctrl_backslash() {
        let pk = parse_prefix("ctrl-\\").unwrap();
        assert_eq!(pk.code, KeyCode::Char('\\'));
        assert!(pk.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn parse_case_insensitive() {
        let pk = parse_prefix("Ctrl-Space").unwrap();
        assert_eq!(pk.code, KeyCode::Char(' '));
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert!(parse_prefix("space").is_none());
        assert!(parse_prefix("alt-a").is_none());
        assert!(parse_prefix("ctrl-").is_none());
        assert!(parse_prefix("ctrl-ab").is_none());
    }

    #[test]
    fn prefix_key_matches() {
        let pk = PrefixKey::default();
        assert!(pk.matches(KeyCode::Char(' '), KeyModifiers::CONTROL));
        assert!(!pk.matches(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!pk.matches(KeyCode::Char(' '), KeyModifiers::NONE));
    }

    #[test]
    fn default_config_has_ctrl_space() {
        let cfg = Config::default();
        assert_eq!(cfg.prefix.code, KeyCode::Char(' '));
        assert!(cfg.prefix.modifiers.contains(KeyModifiers::CONTROL));
    }
}
