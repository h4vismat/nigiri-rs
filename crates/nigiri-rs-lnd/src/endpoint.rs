use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Clone, Debug, Eq, PartialEq)]
enum PeerHost {
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
    Name(String),
    OnionV2(String),
    OnionV3(String),
}

pub(crate) fn normalize_outbound_peer_host(value: &str) -> Result<String, ()> {
    match parse_host(value)? {
        PeerHost::OnionV2(_) => Err(()),
        host => Ok(canonical_host(host)),
    }
}

fn normalize_reported_peer_host(value: &str) -> Result<String, ()> {
    parse_host(value).map(canonical_host)
}

fn canonical_host(host: PeerHost) -> String {
    match host {
        PeerHost::Ipv4(address) => address.to_string(),
        PeerHost::Ipv6(address) => address.to_string(),
        PeerHost::Name(name) | PeerHost::OnionV2(name) | PeerHost::OnionV3(name) => name,
    }
}

pub(crate) fn serialize_peer_endpoint(host: &str, port: u16) -> String {
    if host.parse::<Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub(crate) fn parse_peer_endpoint(value: &str) -> Result<String, ()> {
    if value != value.trim() {
        return Err(());
    }
    let (host, port) = if let Some(bracketed) = value.strip_prefix('[') {
        let (host, port) = bracketed.split_once("]:").ok_or(())?;
        if host.is_empty() || port.contains(':') || port.contains(']') {
            return Err(());
        }
        (format!("[{host}]"), port)
    } else {
        let (host, port) = value.split_once(':').ok_or(())?;
        if host.is_empty() || port.contains(':') {
            return Err(());
        }
        (host.to_owned(), port)
    };
    let port = port.parse::<u16>().map_err(|_| ())?;
    if port == 0 {
        return Err(());
    }
    let host = normalize_reported_peer_host(&host)?;
    Ok(serialize_peer_endpoint(&host, port))
}

fn parse_host(value: &str) -> Result<PeerHost, ()> {
    if value.is_empty() || value != value.trim() {
        return Err(());
    }
    if value.starts_with('[') || value.ends_with(']') {
        let inner = value
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'));
        let address = inner.ok_or(())?.parse::<Ipv6Addr>().map_err(|_| ())?;
        return Ok(PeerHost::Ipv6(address));
    }
    if let Ok(address) = value.parse::<Ipv6Addr>() {
        return Ok(PeerHost::Ipv6(address));
    }
    if value.contains(':') {
        return Err(());
    }
    if let Ok(address) = value.parse::<Ipv4Addr>() {
        return Ok(PeerHost::Ipv4(address));
    }
    parse_name(value)
}

fn parse_name(value: &str) -> Result<PeerHost, ()> {
    if !value.is_ascii() || value.len() > 253 {
        return Err(());
    }
    let name = value.strip_suffix('.').unwrap_or(value);
    if name.is_empty() {
        return Err(());
    }
    let lowercase_name = name.to_ascii_lowercase();
    if let Some(service) = lowercase_name.strip_suffix(".onion") {
        let valid_base32 = service
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte));
        if !valid_base32 || service.contains('.') {
            return Err(());
        }
        return match service.len() {
            16 => Ok(PeerHost::OnionV2(lowercase_name)),
            56 => Ok(PeerHost::OnionV3(lowercase_name)),
            _ => Err(()),
        };
    }
    if name
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err(());
    }
    for label in name.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(());
        }
    }
    Ok(PeerHost::Name(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::parse_peer_endpoint;

    const V2_ENDPOINT: &str = "abcdefghijklmnop.onion:9735";
    const V3_ENDPOINT: &str = concat!(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.onion",
        ":9735"
    );

    #[test]
    fn reported_v2_onion_endpoint_remains_compatible() {
        assert_eq!(parse_peer_endpoint(V2_ENDPOINT).unwrap(), V2_ENDPOINT);
    }

    #[test]
    fn reported_v3_onion_endpoint_is_canonicalized() {
        let reported = concat!(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.ONION.",
            ":9735"
        );

        let endpoint = parse_peer_endpoint(reported).unwrap();

        assert_eq!(endpoint, V3_ENDPOINT);
    }
}
