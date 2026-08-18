use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Clone, Debug, Eq, PartialEq)]
enum PeerHost {
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
    Name(String),
}

pub(crate) fn normalize_peer_host(value: &str) -> Result<String, ()> {
    parse_host(value).map(|host| match host {
        PeerHost::Ipv4(address) => address.to_string(),
        PeerHost::Ipv6(address) => address.to_string(),
        PeerHost::Name(name) => name,
    })
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
    let host = normalize_peer_host(&host)?;
    Ok(serialize_peer_endpoint(&host, port))
}

fn parse_host(value: &str) -> Result<PeerHost, ()> {
    let value = value.trim();
    if value.is_empty() {
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
    validate_name(value)?;
    Ok(PeerHost::Name(value.to_owned()))
}

fn validate_name(value: &str) -> Result<(), ()> {
    if !value.is_ascii() || value.len() > 253 {
        return Err(());
    }
    let name = value.strip_suffix('.').unwrap_or(value);
    if name.is_empty() {
        return Err(());
    }
    let lowercase_name = name.to_ascii_lowercase();
    if let Some(service) = lowercase_name.strip_suffix(".onion") {
        let valid_length = matches!(service.len(), 16 | 56);
        let valid_base32 = service
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte));
        if !valid_length || !valid_base32 || service.contains('.') {
            return Err(());
        }
        return Ok(());
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
    Ok(())
}
