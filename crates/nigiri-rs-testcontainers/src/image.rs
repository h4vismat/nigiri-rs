use crate::FixtureError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerImage {
    name: String,
    tag: String,
    digest: Option<String>,
}

impl ContainerImage {
    pub fn new(name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tag: tag.into(),
            digest: None,
        }
    }

    pub fn with_digest(mut self, digest: impl Into<String>) -> Self {
        self.digest = Some(digest.into());
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }

    pub(crate) fn validate(&self) -> Result<(), FixtureError> {
        if self.name.is_empty() {
            return Err(FixtureError::InvalidConfiguration {
                detail: "image name must not be empty".to_owned(),
            });
        }

        if self.tag.is_empty() {
            return Err(FixtureError::InvalidConfiguration {
                detail: "image tag must not be empty".to_owned(),
            });
        }

        if self
            .digest
            .as_deref()
            .is_some_and(|digest| !is_sha256_digest(digest))
        {
            return Err(FixtureError::InvalidConfiguration {
                detail:
                    "image digest must be sha256 followed by 64 lowercase hexadecimal characters"
                        .to_owned(),
            });
        }

        Ok(())
    }

    pub(crate) fn testcontainers_tag(&self) -> String {
        match &self.digest {
            Some(digest) => format!("{}@{digest}", self.tag),
            None => self.tag.clone(),
        }
    }

    pub(crate) fn bitcoind_default() -> Self {
        Self::new("ghcr.io/getumbrel/docker-bitcoind", "v30.0")
            .with_digest("sha256:f5826a32aed9287cc5ffdec0996f5272634c4b346529cb8627224986ff555101")
    }

    pub(crate) fn electrs_default() -> Self {
        Self::new("ghcr.io/vulpemventures/electrs", "latest")
            .with_digest("sha256:999a2218f423c0fb167ee53b282aa7929a9d4abba38ef16f67f407acd00589d4")
    }

    pub(crate) fn elements_default() -> Self {
        Self::new("ghcr.io/vulpemventures/elements", "latest")
            .with_digest("sha256:8900acd1f8eb4bb9e3f66cd400419155b273137e4526cc5d670ea1a5a25018d8")
    }

    pub(crate) fn electrs_liquid_default() -> Self {
        Self::new("ghcr.io/vulpemventures/electrs-liquid", "latest")
            .with_digest("sha256:d801d1ab07e7a3379462b9d09cf5bb4fada1ced67b001a1a2de31e027c2f6055")
    }
}

fn is_sha256_digest(digest: &str) -> bool {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };

    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use crate::{ContainerImage, FixtureError};

    fn assert_invalid_configuration(error: FixtureError, field: &str) {
        let FixtureError::InvalidConfiguration { detail } = error else {
            panic!("expected invalid configuration error");
        };

        assert!(
            detail.contains(field),
            "configuration detail should identify {field}: {detail}"
        );
        assert!(
            detail.len() <= 256,
            "configuration detail should remain bounded: {detail}"
        );
    }

    // Catches a regression that changes a pinned image descriptor, drops its digest, or rejects it.
    #[test]
    fn default_images_preserve_exact_descriptors() {
        let bitcoind = ContainerImage::bitcoind_default();
        assert_eq!(bitcoind.name(), "ghcr.io/getumbrel/docker-bitcoind");
        assert_eq!(bitcoind.tag(), "v30.0");
        assert_eq!(
            bitcoind.digest(),
            Some("sha256:f5826a32aed9287cc5ffdec0996f5272634c4b346529cb8627224986ff555101")
        );
        assert_eq!(
            bitcoind.testcontainers_tag(),
            "v30.0@sha256:f5826a32aed9287cc5ffdec0996f5272634c4b346529cb8627224986ff555101"
        );
        bitcoind
            .validate()
            .expect("the pinned bitcoind image descriptor is valid");

        let electrs = ContainerImage::electrs_default();
        assert_eq!(electrs.name(), "ghcr.io/vulpemventures/electrs");
        assert_eq!(electrs.tag(), "latest");
        assert_eq!(
            electrs.digest(),
            Some("sha256:999a2218f423c0fb167ee53b282aa7929a9d4abba38ef16f67f407acd00589d4")
        );
        assert_eq!(
            electrs.testcontainers_tag(),
            "latest@sha256:999a2218f423c0fb167ee53b282aa7929a9d4abba38ef16f67f407acd00589d4"
        );
        electrs
            .validate()
            .expect("the pinned electrs image descriptor is valid");

        let elements = ContainerImage::elements_default();
        assert_eq!(elements.name(), "ghcr.io/vulpemventures/elements");
        assert_eq!(elements.tag(), "latest");
        assert_eq!(
            elements.digest(),
            Some("sha256:8900acd1f8eb4bb9e3f66cd400419155b273137e4526cc5d670ea1a5a25018d8")
        );
        elements
            .validate()
            .expect("the pinned Elements image descriptor is valid");

        let electrs_liquid = ContainerImage::electrs_liquid_default();
        assert_eq!(
            electrs_liquid.name(),
            "ghcr.io/vulpemventures/electrs-liquid"
        );
        assert_eq!(electrs_liquid.tag(), "latest");
        assert_eq!(
            electrs_liquid.digest(),
            Some("sha256:d801d1ab07e7a3379462b9d09cf5bb4fada1ced67b001a1a2de31e027c2f6055")
        );
        electrs_liquid
            .validate()
            .expect("the pinned Liquid Electrs image descriptor is valid");
    }

    // Catches a regression that forces caller-provided images to have a digest.
    #[test]
    fn explicit_image_without_digest_keeps_its_tag() {
        let image = ContainerImage::new("registry.example/bitcoin", "custom");

        assert_eq!(image.name(), "registry.example/bitcoin");
        assert_eq!(image.tag(), "custom");
        assert_eq!(image.digest(), None);
        assert_eq!(image.testcontainers_tag(), "custom");
        image
            .validate()
            .expect("an explicit image with no digest is valid");
    }

    // Catches a regression that permits an image reference missing its repository name.
    #[test]
    fn empty_image_name_is_rejected() {
        let error = ContainerImage::new("", "v1")
            .validate()
            .expect_err("an empty image name must be rejected");

        assert_invalid_configuration(error, "image name");
    }

    // Catches a regression that permits an image reference missing its tag.
    #[test]
    fn empty_image_tag_is_rejected() {
        let error = ContainerImage::new("registry.example/bitcoin", "")
            .validate()
            .expect_err("an empty image tag must be rejected");

        assert_invalid_configuration(error, "image tag");
    }

    // Catches a regression that accepts malformed content-addressed image digests.
    #[test]
    fn malformed_image_digests_are_rejected() {
        for digest in [
            "",
            "sha256:deadbeef",
            "sha512:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeF",
        ] {
            let error = ContainerImage::new("registry.example/bitcoin", "v1")
                .with_digest(digest)
                .validate()
                .expect_err("a malformed digest must be rejected");

            assert_invalid_configuration(error, "digest");
        }
    }
}
