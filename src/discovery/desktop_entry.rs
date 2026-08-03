use std::collections::BTreeMap;

use thiserror::Error;

/// Parsed values from the main `[Desktop Entry]` group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DesktopEntry {
    pub entry_type: Option<String>,
    pub names: LocalizedValue,
    pub generic_names: LocalizedValue,
    pub exec: Option<String>,
    pub icon: Option<String>,
    pub hidden: bool,
    pub no_display: bool,
    pub terminal: bool,
    pub try_exec: Option<String>,
    pub only_show_in: Vec<String>,
    pub not_show_in: Vec<String>,
    pub categories: Vec<String>,
    pub startup_wm_class: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LocalizedValue {
    unlocalized: Option<String>,
    localized: BTreeMap<String, String>,
}

impl LocalizedValue {
    pub fn resolve(&self, locale: Option<&str>) -> Option<String> {
        for candidate in locale_candidates(locale) {
            if let Some(value) = self.localized.get(&candidate) {
                return Some(value.clone());
            }
            if let Some((_key, value)) = self
                .localized
                .iter()
                .find(|(key, _value)| key.eq_ignore_ascii_case(&candidate))
            {
                return Some(value.clone());
            }
        }
        self.unlocalized.clone()
    }
}

/// Syntax or supported-value error in a desktop entry.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum DesktopEntryParseError {
    /// No main desktop-entry group was present.
    #[error("missing [Desktop Entry] group")]
    MissingDesktopEntryGroup,
    /// The main group appeared more than once.
    #[error("[Desktop Entry] group appears more than once")]
    DuplicateDesktopEntryGroup,
    /// A line in the main group was not a key/value pair.
    #[error("line {line} in [Desktop Entry] is not a key/value pair")]
    MalformedLine { line: usize },
    /// A supported key was repeated.
    #[error("line {line} repeats desktop-entry key `{key}`")]
    DuplicateKey { line: usize, key: String },
    /// A boolean used syntax other than lowercase `true` or `false`.
    #[error("key `{key}` has invalid boolean value `{value}`")]
    InvalidBoolean { key: String, value: String },
    /// A string used an unsupported escape sequence.
    #[error("key `{key}` contains unsupported escape `\\{escape}`")]
    InvalidEscape { key: String, escape: char },
    /// A string ended with a bare backslash.
    #[error("key `{key}` ends with an incomplete escape")]
    IncompleteEscape { key: String },
}

/// Parse only the main `[Desktop Entry]` group.
///
/// Desktop action groups and unknown keys are ignored. Supported string
/// escapes are `\\s`, `\\n`, `\\t`, `\\r`, and `\\\\`. Duplicate supported
/// keys, malformed supported booleans, and malformed lines in the main group
/// reject the entry rather than silently guessing.
pub(crate) fn parse_desktop_entry(input: &str) -> Result<DesktopEntry, DesktopEntryParseError> {
    let mut values = BTreeMap::<String, String>::new();
    let mut localized_names = BTreeMap::<String, String>::new();
    let mut localized_generic_names = BTreeMap::<String, String>::new();
    let mut in_main_group = false;
    let mut saw_main_group = false;

    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let group = &line[1..line.len() - 1];
            if group == "Desktop Entry" {
                if saw_main_group {
                    return Err(DesktopEntryParseError::DuplicateDesktopEntryGroup);
                }
                saw_main_group = true;
                in_main_group = true;
            } else {
                in_main_group = false;
            }
            continue;
        }
        if !in_main_group {
            continue;
        }

        let (key, value) = line
            .split_once('=')
            .ok_or(DesktopEntryParseError::MalformedLine { line: line_number })?;
        let key = key.trim();
        let value = value.trim();
        if let Some(locale) = localized_key(key, "Name") {
            insert_unique(&mut localized_names, locale, value, line_number)?;
        } else if let Some(locale) = localized_key(key, "GenericName") {
            insert_unique(&mut localized_generic_names, locale, value, line_number)?;
        } else if is_supported_key(key) {
            insert_unique(&mut values, key, value, line_number)?;
        }
    }

    if !saw_main_group {
        return Err(DesktopEntryParseError::MissingDesktopEntryGroup);
    }

    let names = parse_localized("Name", values.remove("Name"), localized_names)?;
    let generic_names = parse_localized(
        "GenericName",
        values.remove("GenericName"),
        localized_generic_names,
    )?;

    Ok(DesktopEntry {
        entry_type: optional_string("Type", values.remove("Type"))?,
        names,
        generic_names,
        exec: nonempty_raw(values.remove("Exec")),
        icon: optional_string("Icon", values.remove("Icon"))?,
        hidden: parse_boolean("Hidden", values.remove("Hidden"))?,
        no_display: parse_boolean("NoDisplay", values.remove("NoDisplay"))?,
        terminal: parse_boolean("Terminal", values.remove("Terminal"))?,
        try_exec: optional_string("TryExec", values.remove("TryExec"))?,
        only_show_in: parse_list("OnlyShowIn", values.remove("OnlyShowIn"))?,
        not_show_in: parse_list("NotShowIn", values.remove("NotShowIn"))?,
        categories: parse_list("Categories", values.remove("Categories"))?,
        startup_wm_class: optional_string("StartupWMClass", values.remove("StartupWMClass"))?,
    })
}

fn localized_key<'a>(key: &'a str, base: &str) -> Option<&'a str> {
    let suffix = key.strip_prefix(base)?;
    suffix
        .strip_prefix('[')
        .and_then(|locale| locale.strip_suffix(']'))
        .filter(|locale| !locale.is_empty())
}

fn is_supported_key(key: &str) -> bool {
    matches!(
        key,
        "Type"
            | "Name"
            | "GenericName"
            | "Exec"
            | "Icon"
            | "Hidden"
            | "NoDisplay"
            | "Terminal"
            | "TryExec"
            | "OnlyShowIn"
            | "NotShowIn"
            | "Categories"
            | "StartupWMClass"
    )
}

fn insert_unique(
    values: &mut BTreeMap<String, String>,
    key: &str,
    value: &str,
    line: usize,
) -> Result<(), DesktopEntryParseError> {
    if values.insert(key.to_owned(), value.to_owned()).is_some() {
        return Err(DesktopEntryParseError::DuplicateKey {
            line,
            key: key.to_owned(),
        });
    }
    Ok(())
}

fn parse_localized(
    key: &str,
    unlocalized: Option<String>,
    localized: BTreeMap<String, String>,
) -> Result<LocalizedValue, DesktopEntryParseError> {
    let unlocalized = unlocalized
        .map(|value| unescape(key, &value))
        .transpose()?
        .filter(|value| !value.is_empty());
    let localized = localized
        .into_iter()
        .map(|(locale, value)| unescape(key, &value).map(|value| (locale, value)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    Ok(LocalizedValue {
        unlocalized,
        localized,
    })
}

fn optional_string(
    key: &str,
    value: Option<String>,
) -> Result<Option<String>, DesktopEntryParseError> {
    value
        .map(|value| unescape(key, &value))
        .transpose()
        .map(|value| value.filter(|value| !value.is_empty()))
}

fn nonempty_raw(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn parse_boolean(key: &str, value: Option<String>) -> Result<bool, DesktopEntryParseError> {
    match value.as_deref() {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(value) => Err(DesktopEntryParseError::InvalidBoolean {
            key: key.to_owned(),
            value: value.to_owned(),
        }),
    }
}

fn parse_list(key: &str, value: Option<String>) -> Result<Vec<String>, DesktopEntryParseError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let value = unescape(key, &value)?;
    Ok(value
        .split(';')
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect())
}

fn unescape(key: &str, value: &str) -> Result<String, DesktopEntryParseError> {
    let mut result = String::new();
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            result.push(character);
            continue;
        }
        let escaped =
            characters
                .next()
                .ok_or_else(|| DesktopEntryParseError::IncompleteEscape {
                    key: key.to_owned(),
                })?;
        match escaped {
            's' => result.push(' '),
            'n' => result.push('\n'),
            't' => result.push('\t'),
            'r' => result.push('\r'),
            '\\' => result.push('\\'),
            escape => {
                return Err(DesktopEntryParseError::InvalidEscape {
                    key: key.to_owned(),
                    escape,
                });
            }
        }
    }
    Ok(result)
}

fn locale_candidates(locale: Option<&str>) -> Vec<String> {
    let Some(locale) = locale.map(str::trim).filter(|locale| !locale.is_empty()) else {
        return Vec::new();
    };
    if locale.eq_ignore_ascii_case("C") || locale.eq_ignore_ascii_case("POSIX") {
        return Vec::new();
    }

    let without_encoding = match (locale.find('.'), locale.find('@')) {
        (Some(dot), Some(modifier)) if dot < modifier => {
            format!("{}{}", &locale[..dot], &locale[modifier..])
        }
        (Some(dot), _) => locale[..dot].to_owned(),
        _ => locale.to_owned(),
    };
    let mut candidates = vec![without_encoding.clone()];
    let without_modifier = without_encoding
        .split_once('@')
        .map_or(without_encoding.as_str(), |(base, _modifier)| base);
    if without_modifier != without_encoding {
        candidates.push(without_modifier.to_owned());
    }
    let language = without_modifier
        .split_once('_')
        .map_or(without_modifier, |(language, _territory)| language);
    if !candidates.iter().any(|candidate| candidate == language) {
        candidates.push(language.to_owned());
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::{parse_desktop_entry, DesktopEntryParseError};

    #[test]
    fn parses_main_group_and_ignores_actions_and_unknown_keys() {
        let entry = parse_desktop_entry(
            "# comment\n[Desktop Entry]\nType=Application\nName=Example\\sApp\nExec=example %U\nTerminal=true\nCategories=Utility;Network;\nUnknown=value\n\n[Desktop Action New]\nName=Wrong\nExec=wrong\n",
        )
        .unwrap_or_else(|error| panic!("parse valid entry: {error}"));

        assert_eq!(entry.entry_type.as_deref(), Some("Application"));
        assert_eq!(entry.names.resolve(None).as_deref(), Some("Example App"));
        assert_eq!(entry.exec.as_deref(), Some("example %U"));
        assert!(entry.terminal);
        assert_eq!(entry.categories, ["Utility", "Network"]);
    }

    #[test]
    fn resolves_specific_then_language_then_unlocalized_name() {
        let entry = parse_desktop_entry(
            "[Desktop Entry]\nName=Default\nName[fr]=Français\nName[fr_CA]=Québécois\nExec=example\n",
        )
        .unwrap_or_else(|error| panic!("parse localized entry: {error}"));

        assert_eq!(
            entry.names.resolve(Some("fr_CA.UTF-8")).as_deref(),
            Some("Québécois")
        );
        assert_eq!(
            entry.names.resolve(Some("fr_FR.UTF-8")).as_deref(),
            Some("Français")
        );
        assert_eq!(
            entry.names.resolve(Some("de_DE.UTF-8")).as_deref(),
            Some("Default")
        );
    }

    #[test]
    fn rejects_malformed_supported_values_without_panicking() {
        assert!(matches!(
            parse_desktop_entry("[Desktop Entry]\nName=Example\nExec=example\nHidden=True\n"),
            Err(DesktopEntryParseError::InvalidBoolean { .. })
        ));
        assert!(matches!(
            parse_desktop_entry("[Desktop Entry]\nName=Bad\\qName\nExec=example\n"),
            Err(DesktopEntryParseError::InvalidEscape { .. })
        ));
        assert!(matches!(
            parse_desktop_entry("[Desktop Entry]\nName=One\nName=Two\nExec=example\n"),
            Err(DesktopEntryParseError::DuplicateKey { .. })
        ));
    }
}
