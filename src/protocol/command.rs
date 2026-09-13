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
            FtpCommand::User(arg) => format!("USER {arg}"),
            FtpCommand::Pass(_) => "PASS <redacted>".to_string(),
            FtpCommand::Syst => "SYST".to_string(),
            FtpCommand::Type(arg) => format!("TYPE {arg}"),
            FtpCommand::Pwd => "PWD".to_string(),
            FtpCommand::Cwd(arg) => format!("CWD {arg}"),
            FtpCommand::Pasv => "PASV".to_string(),
            FtpCommand::Epsv => "EPSV".to_string(),
            FtpCommand::Stor(arg) => format!("STOR {arg}"),
            FtpCommand::Quit => "QUIT".to_string(),
            FtpCommand::Noop => "NOOP".to_string(),
            FtpCommand::Unknown(verb) => format!("UNKNOWN {verb}"),
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
}
