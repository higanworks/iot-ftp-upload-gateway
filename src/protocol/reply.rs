use std::net::Ipv4Addr;

/// Builds the 227 reply to a PASV command.
/// Format: "227 Entering Passive Mode (h1,h2,h3,h4,p1,p2).\r\n"
pub fn pasv_reply(ip: Ipv4Addr, port: u16) -> String {
    let [o1, o2, o3, o4] = ip.octets();
    let p1 = (port >> 8) as u8;
    let p2 = (port & 0xFF) as u8;
    format!("227 Entering Passive Mode ({o1},{o2},{o3},{o4},{p1},{p2}).\r\n")
}

/// Builds the 229 reply to an EPSV command (RFC 2428).
/// Format: "229 Entering Extended Passive Mode (|||port|).\r\n"
/// Unlike PASV, no address is included: the client is expected to reuse the address of the
/// control connection for the data connection.
pub fn epsv_reply(port: u16) -> String {
    format!("229 Entering Extended Passive Mode (|||{port}|).\r\n")
}

/// Parses a 227 PASV reply (e.g. "227 Entering Passive Mode (127,0,0,1,39,16).")
/// and extracts the address and port to connect to. Used when the Gateway itself
/// acts as a PASV client toward a backend server.
pub fn parse_pasv_reply(line: &str) -> Option<(Ipv4Addr, u16)> {
    let start = line.find('(')?;
    let end = start + line[start..].find(')')?;

    let numbers: Vec<u16> = line[start + 1..end]
        .split(',')
        .map(|part| part.trim().parse().ok())
        .collect::<Option<_>>()?;

    let [o1, o2, o3, o4, p1, p2]: [u16; 6] = numbers.try_into().ok()?;
    if [o1, o2, o3, o4, p1, p2].iter().any(|&n| n > 255) {
        return None;
    }

    let ip = Ipv4Addr::new(o1 as u8, o2 as u8, o3 as u8, o4 as u8);
    let port = (p1 << 8) | p2;
    Some((ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_address_and_port() {
        let reply = pasv_reply(Ipv4Addr::new(127, 0, 0, 1), 10000);
        assert_eq!(reply, "227 Entering Passive Mode (127,0,0,1,39,16).\r\n");
    }

    #[test]
    fn encodes_minimum_port() {
        let reply = pasv_reply(Ipv4Addr::new(0, 0, 0, 0), 0);
        assert_eq!(reply, "227 Entering Passive Mode (0,0,0,0,0,0).\r\n");
    }

    #[test]
    fn encodes_maximum_port_and_address() {
        let reply = pasv_reply(Ipv4Addr::new(255, 255, 255, 255), 65535);
        assert_eq!(
            reply,
            "227 Entering Passive Mode (255,255,255,255,255,255).\r\n"
        );
    }

    #[test]
    fn encodes_epsv_reply() {
        assert_eq!(
            epsv_reply(10000),
            "229 Entering Extended Passive Mode (|||10000|).\r\n"
        );
        assert_eq!(
            epsv_reply(0),
            "229 Entering Extended Passive Mode (|||0|).\r\n"
        );
        assert_eq!(
            epsv_reply(65535),
            "229 Entering Extended Passive Mode (|||65535|).\r\n"
        );
    }

    #[test]
    fn round_trips_through_generate_and_parse() {
        let ip = Ipv4Addr::new(192, 168, 1, 1);
        let port = 51200;
        let reply = pasv_reply(ip, port);
        assert_eq!(parse_pasv_reply(&reply), Some((ip, port)));
    }

    #[test]
    fn parses_reply_with_extra_leading_text() {
        let parsed = parse_pasv_reply("227 Entering Passive Mode (10,0,0,5,200,3).\r\n");
        assert_eq!(parsed, Some((Ipv4Addr::new(10, 0, 0, 5), 51203)));
    }

    #[test]
    fn rejects_malformed_reply() {
        assert_eq!(parse_pasv_reply("200 OK\r\n"), None);
        assert_eq!(
            parse_pasv_reply("227 Entering Passive Mode (1,2,3).\r\n"),
            None
        );
        assert_eq!(
            parse_pasv_reply("227 Entering Passive Mode (1,2,3,4,5,300).\r\n"),
            None
        );
    }
}
