use std::path::Path;

use thiserror::Error;

/// A safely tokenized desktop-entry command before executable resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedExec {
    /// Executable name or path from the first token.
    pub executable: String,
    /// Static process arguments after supported field-code handling.
    pub arguments: Vec<String>,
}

/// A reason an `Exec` value cannot be represented safely.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ExecParseError {
    /// No token remained for the executable.
    #[error("Exec contains no executable")]
    MissingExecutable,
    /// A double-quoted token was not terminated.
    #[error("Exec contains an unterminated double quote")]
    UnterminatedQuote,
    /// A trailing backslash had no character to escape.
    #[error("Exec ends with an incomplete escape")]
    IncompleteEscape,
    /// A token cannot be passed to a process API.
    #[error("Exec contains a NUL byte")]
    ContainsNul,
    /// A percent sign did not introduce a field code.
    #[error("Exec contains a trailing percent sign")]
    TrailingPercent,
    /// The field code is not supported by this launcher.
    #[error("Exec contains unsupported field code `%{code}`")]
    UnsupportedFieldCode { code: char },
    /// A dynamic field code appeared inside a larger token.
    #[error("Exec field code `%{code}` must occupy a complete argument")]
    EmbeddedFieldCode { code: char },
    /// A field code attempted to alter the executable token.
    #[error("Exec field code `%{code}` is not allowed in the executable token")]
    ExecutableFieldCode { code: char },
    /// `%k` could not be represented as UTF-8 in the public argument API.
    #[error("desktop-file path for `%k` is not valid UTF-8")]
    NonUtf8DesktopPath,
}

/// Parse a desktop-entry `Exec` value without invoking or emulating a shell.
///
/// Double quotes group text, and a backslash escapes the following character.
/// Outside quotes, spaces and tabs separate tokens. Shell metacharacters have
/// no special meaning. `%f`, `%F`, `%u`, `%U`, and `%i` are removed; `%c` and
/// `%k` become complete arguments; `%%` becomes a literal percent sign.
/// Dynamic field codes embedded in a larger token are rejected.
///
/// # Errors
///
/// Returns [`ExecParseError`] for malformed tokenization, unsafe field-code
/// positions, unsupported field codes, or a missing executable.
pub fn parse_exec(
    input: &str,
    application_name: &str,
    desktop_file: &Path,
) -> Result<ParsedExec, ExecParseError> {
    let tokens = tokenize(input)?;
    let Some(executable_token) = tokens.first() else {
        return Err(ExecParseError::MissingExecutable);
    };
    if executable_token.is_empty() {
        return Err(ExecParseError::MissingExecutable);
    }
    let executable = expand_executable(executable_token)?;
    if executable.is_empty() {
        return Err(ExecParseError::MissingExecutable);
    }

    let desktop_path = desktop_file
        .to_str()
        .ok_or(ExecParseError::NonUtf8DesktopPath)?;
    let mut arguments = Vec::new();
    for token in &tokens[1..] {
        match expand_argument(token, application_name, desktop_path)? {
            Some(argument) => arguments.push(argument),
            None => continue,
        }
    }

    Ok(ParsedExec {
        executable,
        arguments,
    })
}

fn tokenize(input: &str) -> Result<Vec<String>, ExecParseError> {
    if input.contains('\0') {
        return Err(ExecParseError::ContainsNul);
    }
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut token_started = false;
    let mut quoted = false;
    let mut escaped = false;

    for character in input.chars() {
        if escaped {
            token.push(character);
            token_started = true;
            escaped = false;
            continue;
        }
        match character {
            '\\' => {
                escaped = true;
                token_started = true;
            }
            '"' => {
                quoted = !quoted;
                token_started = true;
            }
            ' ' | '\t' if !quoted => {
                if token_started {
                    tokens.push(std::mem::take(&mut token));
                    token_started = false;
                }
            }
            _ => {
                token.push(character);
                token_started = true;
            }
        }
    }
    if escaped {
        return Err(ExecParseError::IncompleteEscape);
    }
    if quoted {
        return Err(ExecParseError::UnterminatedQuote);
    }
    if token_started {
        tokens.push(token);
    }
    Ok(tokens)
}

fn expand_executable(token: &str) -> Result<String, ExecParseError> {
    let mut output = String::new();
    let mut characters = token.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let code = characters.next().ok_or(ExecParseError::TrailingPercent)?;
        if code == '%' {
            output.push('%');
        } else if is_recognized_code(code) {
            return Err(ExecParseError::ExecutableFieldCode { code });
        } else {
            return Err(ExecParseError::UnsupportedFieldCode { code });
        }
    }
    Ok(output)
}

fn expand_argument(
    token: &str,
    application_name: &str,
    desktop_path: &str,
) -> Result<Option<String>, ExecParseError> {
    if let Some(code) = complete_dynamic_code(token) {
        return match code {
            'f' | 'F' | 'u' | 'U' | 'i' => Ok(None),
            'c' => Ok(Some(application_name.to_owned())),
            'k' => Ok(Some(desktop_path.to_owned())),
            _ => Err(ExecParseError::UnsupportedFieldCode { code }),
        };
    }

    let mut output = String::new();
    let mut characters = token.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let code = characters.next().ok_or(ExecParseError::TrailingPercent)?;
        match code {
            '%' => output.push('%'),
            'f' | 'F' | 'u' | 'U' | 'i' | 'c' | 'k' => {
                return Err(ExecParseError::EmbeddedFieldCode { code });
            }
            _ => return Err(ExecParseError::UnsupportedFieldCode { code }),
        }
    }
    Ok(Some(output))
}

fn complete_dynamic_code(token: &str) -> Option<char> {
    let mut characters = token.chars();
    match (characters.next(), characters.next(), characters.next()) {
        (Some('%'), Some(code), None) if code != '%' => Some(code),
        _ => None,
    }
}

fn is_recognized_code(code: char) -> bool {
    matches!(code, 'f' | 'F' | 'u' | 'U' | 'i' | 'c' | 'k')
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{parse_exec, ExecParseError, ParsedExec};

    fn parse(input: &str) -> Result<ParsedExec, ExecParseError> {
        parse_exec(
            input,
            "Example Application",
            Path::new("/data/example.desktop"),
        )
    }

    #[test]
    fn tokenizes_unquoted_and_quoted_commands() {
        assert_eq!(
            parse("example --flag value"),
            Ok(ParsedExec {
                executable: "example".to_owned(),
                arguments: vec!["--flag".to_owned(), "value".to_owned()]
            })
        );
        assert_eq!(
            parse("\"/opt/Example App/example\" \"two words\" \"\""),
            Ok(ParsedExec {
                executable: "/opt/Example App/example".to_owned(),
                arguments: vec!["two words".to_owned(), String::new()]
            })
        );
    }

    #[test]
    fn supports_escaped_spaces_and_quotes() {
        assert_eq!(
            parse("example escaped\\ space \\\"quoted\\\""),
            Ok(ParsedExec {
                executable: "example".to_owned(),
                arguments: vec!["escaped space".to_owned(), "\"quoted\"".to_owned()]
            })
        );
    }

    #[test]
    fn applies_supported_field_codes() {
        let parsed = parse("example %f %F %u %U %i %c %k 100%%");
        assert_eq!(
            parsed,
            Ok(ParsedExec {
                executable: "example".to_owned(),
                arguments: vec![
                    "Example Application".to_owned(),
                    "/data/example.desktop".to_owned(),
                    "100%".to_owned()
                ]
            })
        );
    }

    #[test]
    fn rejects_malformed_or_unknown_field_codes() {
        assert_eq!(parse("example %"), Err(ExecParseError::TrailingPercent));
        assert_eq!(
            parse("example %d"),
            Err(ExecParseError::UnsupportedFieldCode { code: 'd' })
        );
        assert_eq!(
            parse("example prefix%U"),
            Err(ExecParseError::EmbeddedFieldCode { code: 'U' })
        );
        assert_eq!(
            parse("app%c"),
            Err(ExecParseError::ExecutableFieldCode { code: 'c' })
        );
    }

    #[test]
    fn rejects_malformed_quoting_escaping_and_missing_executable() {
        assert_eq!(
            parse("example \"open"),
            Err(ExecParseError::UnterminatedQuote)
        );
        assert_eq!(
            parse("example trailing\\"),
            Err(ExecParseError::IncompleteEscape)
        );
        assert_eq!(parse("  \t "), Err(ExecParseError::MissingExecutable));
        assert_eq!(parse("\"\" arg"), Err(ExecParseError::MissingExecutable));
    }

    #[test]
    fn shell_metacharacters_are_ordinary_arguments() {
        assert_eq!(
            parse("example ; rm -rf / | $HOME && $(touch)"),
            Ok(ParsedExec {
                executable: "example".to_owned(),
                arguments: [";", "rm", "-rf", "/", "|", "$HOME", "&&", "$(touch)"]
                    .map(str::to_owned)
                    .to_vec()
            })
        );
    }
}
