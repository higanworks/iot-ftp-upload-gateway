use std::borrow::Cow;

/// Escapes ASCII control characters (everything `char::is_control` reports true for, including
/// `\r` and ANSI escape bytes) using Rust's backslash escape notation, so a client-controlled
/// string (a filename, or a raw command argument) can never forge extra lines or terminal
/// control sequences when written verbatim into a human-readable log line
/// (PROJECT_SECURITY.md section 11). A `\n` can't appear here in the first place -- the control
/// line reader already stops at the first one -- but `\r` and other control bytes can. JSON-format
/// logs are unaffected either way, since JSON string escaping already handles this.
pub fn escape_control_chars(input: &str) -> Cow<'_, str> {
    if !input.chars().any(|c| c.is_control()) {
        return Cow::Borrowed(input);
    }
    let mut escaped = String::with_capacity(input.len());
    for c in input.chars() {
        if c.is_control() {
            escaped.extend(c.escape_default());
        } else {
            escaped.push(c);
        }
    }
    Cow::Owned(escaped)
}

/// Returns true if `body` -- a command line already stripped of its trailing line terminator --
/// still contains a CR or LF byte. `body` is what the control-line reader treated as a single
/// FTP command; if a CR or LF survives inside it, the client smuggled a line break into what
/// this Gateway forwards to the Backend as one command, and the Backend could split it into two
/// (PROJECT_SECURITY.md section 4). Backslash (`\`) is not an FTP special character and is never
/// flagged here.
pub fn has_embedded_line_break(body: &str) -> bool {
    body.contains('\r') || body.contains('\n')
}

/// The minimal set of FTP commands an IoT device needs for uploading, as listed in PROJECT.ja.md section 4.
#[derive(Debug, PartialEq, Eq)]
pub enum FtpCommand {
    User(String),
    Pass(String),
    Syst,
    Type(String),
    Pwd,
    Cwd(String),
    Pasv,
    Epsv,
    Stor(String),
    Quit,
    Noop,
    Unknown(String),
}

impl FtpCommand {
    pub fn parse(line: &str) -> Self {
        let line = line.trim();
        let (verb, rest) = match line.split_once(' ') {
            Some((verb, rest)) => (verb, rest.trim()),
            None => (line, ""),
        };

        match verb.to_ascii_uppercase().as_str() {
            "USER" => FtpCommand::User(rest.to_string()),
            "PASS" => FtpCommand::Pass(rest.to_string()),
            "SYST" => FtpCommand::Syst,
            "TYPE" => FtpCommand::Type(rest.to_string()),
            "PWD" => FtpCommand::Pwd,
            "CWD" => FtpCommand::Cwd(rest.to_string()),
            "PASV" => FtpCommand::Pasv,
            // The optional network-protocol argument (e.g. "EPSV 2" for IPv6) is ignored:
            // this gateway is IPv4-only, so any EPSV request is handled the same way.
            "EPSV" => FtpCommand::Epsv,
            "STOR" => FtpCommand::Stor(rest.to_string()),
            "QUIT" => FtpCommand::Quit,
            "NOOP" => FtpCommand::Noop,
            other => FtpCommand::Unknown(other.to_string()),
        }
    }

    /// String representation for logging. Never includes the PASS argument (the password itself).
    pub fn as_log_str(&self) -> String {
        match self {
            FtpCommand::User(arg) => format!("USER {}", escape_control_chars(arg)),
            FtpCommand::Pass(_) => "PASS <redacted>".to_string(),
            FtpCommand::Syst => "SYST".to_string(),
            FtpCommand::Type(arg) => format!("TYPE {}", escape_control_chars(arg)),
            FtpCommand::Pwd => "PWD".to_string(),
            FtpCommand::Cwd(arg) => format!("CWD {}", escape_control_chars(arg)),
            FtpCommand::Pasv => "PASV".to_string(),
            FtpCommand::Epsv => "EPSV".to_string(),
            FtpCommand::Stor(arg) => format!("STOR {}", escape_control_chars(arg)),
            FtpCommand::Quit => "QUIT".to_string(),
            FtpCommand::Noop => "NOOP".to_string(),
            FtpCommand::Unknown(verb) => format!("UNKNOWN {}", escape_control_chars(verb)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_with_argument() {
        assert_eq!(
            FtpCommand::parse("USER iot"),
            FtpCommand::User("iot".to_string())
        );
    }

    #[test]
    fn parses_pass_with_argument() {
        assert_eq!(
            FtpCommand::parse("PASS secret123"),
            FtpCommand::Pass("secret123".to_string())
        );
    }

    #[test]
    fn parses_case_insensitively() {
        assert_eq!(
            FtpCommand::parse("user iot"),
            FtpCommand::User("iot".to_string())
        );
        assert_eq!(FtpCommand::parse("Quit"), FtpCommand::Quit);
    }

    #[test]
    fn parses_argument_less_commands() {
        assert_eq!(FtpCommand::parse("PWD"), FtpCommand::Pwd);
        assert_eq!(FtpCommand::parse("PASV"), FtpCommand::Pasv);
        assert_eq!(FtpCommand::parse("NOOP"), FtpCommand::Noop);
    }

    #[test]
    fn parses_epsv_ignoring_protocol_argument() {
        assert_eq!(FtpCommand::parse("EPSV"), FtpCommand::Epsv);
        assert_eq!(FtpCommand::parse("EPSV 2"), FtpCommand::Epsv);
    }

    #[test]
    fn parses_unknown_command() {
        assert_eq!(
            FtpCommand::parse("RETR file.txt"),
            FtpCommand::Unknown("RETR".to_string())
        );
    }

    #[test]
    fn pass_log_str_never_contains_password() {
        let command = FtpCommand::parse("PASS super-secret");
        assert_eq!(command.as_log_str(), "PASS <redacted>");
    }

    #[test]
    fn log_str_escapes_control_characters_in_arguments() {
        let command = FtpCommand::parse("STOR evil\rfile.txt");
        let log_str = command.as_log_str();
        assert!(!log_str.contains('\r'));
        assert_eq!(log_str, "STOR evil\\rfile.txt");
    }

    #[test]
    fn escape_control_chars_leaves_plain_input_untouched() {
        assert!(matches!(
            escape_control_chars("plain.txt"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn has_embedded_line_break_detects_cr() {
        assert!(has_embedded_line_break("evil\rfile.txt"));
    }

    #[test]
    fn has_embedded_line_break_detects_lf() {
        assert!(has_embedded_line_break("evil\nfile.txt"));
    }

    #[test]
    fn has_embedded_line_break_detects_crlf() {
        assert!(has_embedded_line_break("evil\r\nfile.txt"));
    }

    #[test]
    fn has_embedded_line_break_allows_plain_input() {
        assert!(!has_embedded_line_break("plain.txt"));
    }

    #[test]
    fn has_embedded_line_break_allows_backslash() {
        assert!(!has_embedded_line_break(r"back\slash\file.txt"));
    }
}
