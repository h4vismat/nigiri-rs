#[derive(Debug, Eq, PartialEq)]
pub(crate) enum OwnedResource {
    Container { name: String, id: Option<String> },
    Network { name: String, id: Option<String> },
}

impl OwnedResource {
    fn name(&self) -> &str {
        match self {
            Self::Container { name, .. } | Self::Network { name, .. } => name,
        }
    }

    fn confirm(&mut self, id: String) {
        match self {
            Self::Container { id: current, .. } | Self::Network { id: current, .. } => {
                *current = Some(id);
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct ResourceLedger {
    creation_order: Vec<OwnedResource>,
}

impl ResourceLedger {
    pub(crate) fn expect_network(&mut self, name: String) {
        self.creation_order
            .push(OwnedResource::Network { name, id: None });
    }

    pub(crate) fn expect_container(&mut self, name: String) {
        self.creation_order
            .push(OwnedResource::Container { name, id: None });
    }

    pub(crate) fn confirm_container(&mut self, name: &str, id: String) {
        self.confirm(name, id);
    }

    #[allow(dead_code)]
    pub(crate) fn confirm_network(&mut self, name: &str, id: String) {
        self.confirm(name, id);
    }

    fn confirm(&mut self, name: &str, id: String) {
        if let Some(resource) = self
            .creation_order
            .iter_mut()
            .rev()
            .find(|resource| resource.name() == name)
        {
            resource.confirm(id);
        }
    }

    pub(crate) fn take_cleanup_order(&mut self) -> Vec<OwnedResource> {
        self.creation_order.drain(..).rev().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{OwnedResource, ResourceLedger};

    #[test]
    fn cleanup_reverses_creation_and_keeps_unacknowledged_names() {
        let mut ledger = ResourceLedger::default();
        ledger.expect_network("fixture-network".to_owned());
        ledger.expect_container("bitcoin-node".to_owned());
        ledger.confirm_container("bitcoin-node", "node-id".to_owned());
        ledger.expect_container("bitcoin-electrs".to_owned());

        assert_eq!(
            ledger.take_cleanup_order(),
            vec![
                OwnedResource::Container {
                    name: "bitcoin-electrs".to_owned(),
                    id: None,
                },
                OwnedResource::Container {
                    name: "bitcoin-node".to_owned(),
                    id: Some("node-id".to_owned()),
                },
                OwnedResource::Network {
                    name: "fixture-network".to_owned(),
                    id: None,
                },
            ]
        );
        assert!(ledger.take_cleanup_order().is_empty());
    }
}
