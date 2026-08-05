use crate::FixtureError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerImage {
    name: String,
    tag: String,
    digest: Option<String>,
    entrypoint: Option<String>,
}

impl ContainerImage {
    pub fn new(name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tag: tag.into(),
            digest: None,
            entrypoint: None,
        }
    }

    pub fn with_digest(mut self, digest: impl Into<String>) -> Self {
        self.digest = Some(digest.into());
        self
    }

    /// Overrides the image's own `ENTRYPOINT`, which is what the fixture uses by default.
    ///
    /// Needed only by images that do not start their daemon on their own. Blockstream's `elementsd`
    /// image declares no entrypoint and defaults to `bash`, so the flag vector a chain builds would
    /// be execed as a program name; Nigiri's Elements image and both Mempool indexers already
    /// entrypoint their daemon and must not be given one here, or they would exec it twice.
    pub fn with_entrypoint(mut self, entrypoint: impl Into<String>) -> Self {
        self.entrypoint = Some(entrypoint.into());
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

    pub fn entrypoint(&self) -> Option<&str> {
        self.entrypoint.as_deref()
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

        if self
            .entrypoint
            .as_deref()
            .is_some_and(|entrypoint| entrypoint.trim().is_empty())
        {
            return Err(FixtureError::InvalidConfiguration {
                detail: "image entrypoint must not be blank".to_owned(),
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
        Self::new("ghcr.io/getumbrel/docker-bitcoind", "v31.0")
            .with_digest("sha256:89185fc2792a9824cbe18f7ad4ead02a3a9a14adf5b34eb42f60ebec36201fa0")
    }

    /// Mempool's Esplora-Electrs fork rather than Nigiri's, which has not been rebuilt since 2022.
    ///
    /// Both chains pin the same `v3.4.0-dev1` build even though Bitcoin has a stable `v3.3.0`
    /// available: the Liquid variant publishes no stable tag, and the ported suite runs the same
    /// assertions against both indexers, so a version skew between them would be a difference the
    /// tests cannot distinguish from a chain difference.
    pub(crate) fn electrs_default() -> Self {
        Self::new("mempool/electrs", "v3.4.0-dev1")
            .with_digest("sha256:35963870c36a8da5fff8310e94df15869d0e97788bdaa11e63901cdb26fd781a")
    }

    /// Blockstream's own `elementsd` image, built from the verified `ElementsProject/elements`
    /// release binaries, rather than Nigiri's rebuild of the same daemon.
    ///
    /// This is a provenance choice, not a version bump: Nigiri's image runs Elements Core v23.3.3
    /// too, so the `liquidregtest` genesis this fixture builds is unchanged — which
    /// `liquid_fixture.rs` asserts against a hash read from a real Nigiri stack.
    ///
    /// The entrypoint is explicit because this image declares none and defaults to `bash`.
    pub(crate) fn elements_default() -> Self {
        Self::new("blockstream/elementsd", "23.3.3")
            .with_digest("sha256:1abe3ae514662492279c9ba8adc94fea46a0fa60efdd62f4eb93d3e803adff37")
            .with_entrypoint("elementsd")
    }

    /// Built with the `liquid` cargo feature, which is what supplies `--parent-network` and the
    /// `liquidregtest` network value [`Liquid::electrs_cmd`] passes.
    ///
    /// [`Liquid::electrs_cmd`]: crate::chain::FixtureChain::electrs_cmd
    pub(crate) fn electrs_liquid_default() -> Self {
        Self::new("mempool/electrs-liquid", "v3.4.0-dev1")
            .with_digest("sha256:4f26e7f2e8d837b79638881415f6cbe84c699855ae568162db986321442a4288")
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
        assert_eq!(bitcoind.tag(), "v31.0");
        assert_eq!(
            bitcoind.digest(),
            Some("sha256:89185fc2792a9824cbe18f7ad4ead02a3a9a14adf5b34eb42f60ebec36201fa0")
        );
        assert_eq!(
            bitcoind.testcontainers_tag(),
            "v31.0@sha256:89185fc2792a9824cbe18f7ad4ead02a3a9a14adf5b34eb42f60ebec36201fa0"
        );
        bitcoind
            .validate()
            .expect("the pinned bitcoind image descriptor is valid");

        let electrs = ContainerImage::electrs_default();
        assert_eq!(electrs.name(), "mempool/electrs");
        assert_eq!(electrs.tag(), "v3.4.0-dev1");
        assert_eq!(
            electrs.digest(),
            Some("sha256:35963870c36a8da5fff8310e94df15869d0e97788bdaa11e63901cdb26fd781a")
        );
        assert_eq!(
            electrs.testcontainers_tag(),
            "v3.4.0-dev1@sha256:35963870c36a8da5fff8310e94df15869d0e97788bdaa11e63901cdb26fd781a"
        );
        electrs
            .validate()
            .expect("the pinned electrs image descriptor is valid");

        let elements = ContainerImage::elements_default();
        assert_eq!(elements.name(), "blockstream/elementsd");
        assert_eq!(elements.tag(), "23.3.3");
        assert_eq!(
            elements.digest(),
            Some("sha256:1abe3ae514662492279c9ba8adc94fea46a0fa60efdd62f4eb93d3e803adff37")
        );
        elements
            .validate()
            .expect("the pinned Elements image descriptor is valid");

        // The entrypoints are the whole reason this is a per-image property: dropping the Elements
        // one leaves the image running `bash`, and adding one to any of the other three execs their
        // daemon twice.
        assert_eq!(elements.entrypoint(), Some("elementsd"));
        assert_eq!(bitcoind.entrypoint(), None);
        assert_eq!(electrs.entrypoint(), None);

        let electrs_liquid = ContainerImage::electrs_liquid_default();
        assert_eq!(electrs_liquid.name(), "mempool/electrs-liquid");
        assert_eq!(electrs_liquid.tag(), "v3.4.0-dev1");
        assert_eq!(
            electrs_liquid.digest(),
            Some("sha256:4f26e7f2e8d837b79638881415f6cbe84c699855ae568162db986321442a4288")
        );
        assert_eq!(electrs_liquid.entrypoint(), None);
        electrs_liquid
            .validate()
            .expect("the pinned Liquid Electrs image descriptor is valid");
    }

    // Catches a regression that accepts a blank entrypoint, which Docker would reject only once the
    // container was already being created.
    #[test]
    fn a_blank_entrypoint_is_rejected_before_any_request_is_built() {
        for blank in ["", " ", "\t"] {
            let error = ContainerImage::new("registry.example/elements", "custom")
                .with_entrypoint(blank)
                .validate()
                .expect_err("a blank entrypoint must be rejected");

            assert_invalid_configuration(error, "entrypoint");
        }
    }

    // Catches a regression that makes an entrypoint mandatory: most images declare their own and
    // must be left alone.
    #[test]
    fn an_image_without_an_entrypoint_keeps_the_images_own() {
        let image = ContainerImage::new("registry.example/electrs", "custom");

        assert_eq!(image.entrypoint(), None);
        image
            .validate()
            .expect("an image that defers to its own entrypoint is valid");
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
