use url::Url;

use crate::FixtureError;

/// Builds the `http://host:port/` base URL for a mapped container port.
///
/// The runtime chooses both the host and the port, so neither is trusted into a URL by string
/// concatenation.
pub(crate) fn mapped_http_url(host: &str, port: u16) -> Result<Url, FixtureError> {
    let mut url = Url::parse("http://localhost/").expect("the static mapped URL is valid");
    // A URL host must bracket an IPv6 literal, but a container runtime reports one bare, so the
    // bracketed form is tried before the host is called invalid.
    url.set_host(Some(host))
        .or_else(|error| match host.parse::<std::net::Ipv6Addr>() {
            Ok(address) => url.set_host(Some(&format!("[{address}]"))),
            Err(_) => Err(error),
        })
        .map_err(|_| FixtureError::InvalidConfiguration {
            detail: "container runtime returned an invalid mapped host".to_owned(),
        })?;
    url.set_port(Some(port))
        .map_err(|()| FixtureError::InvalidConfiguration {
            detail: "container runtime returned an invalid mapped port".to_owned(),
        })?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::mapped_http_url;

    // Catches a regression that builds a mapped base URL by concatenation, which a runtime-supplied
    // host such as an IPv6 literal would silently corrupt.
    #[test]
    fn a_mapped_url_keeps_its_runtime_host_and_port() {
        assert_eq!(
            mapped_http_url("127.0.0.1", 32_768)
                .expect("a loopback host and port are valid")
                .as_str(),
            "http://127.0.0.1:32768/"
        );
        assert_eq!(
            mapped_http_url("::1", 32_768)
                .expect("an IPv6 host is valid")
                .as_str(),
            "http://[::1]:32768/"
        );
    }
}
