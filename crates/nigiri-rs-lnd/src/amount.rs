use crate::LndError;

/// Whole bitcoin satoshis.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Sats(u64);

impl Sats {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// One-thousandth of a satoshi.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Millisats(u64);

impl Millisats {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl TryFrom<Sats> for Millisats {
    type Error = LndError;

    fn try_from(value: Sats) -> Result<Self, Self::Error> {
        value
            .0
            .checked_mul(1_000)
            .map(Self)
            .ok_or_else(|| LndError::InvalidRequest {
                detail: "satoshi amount overflows millisatoshis".into(),
            })
    }
}

impl TryFrom<Millisats> for Sats {
    type Error = LndError;

    fn try_from(value: Millisats) -> Result<Self, Self::Error> {
        if !value.0.is_multiple_of(1_000) {
            return Err(LndError::InvalidRequest {
                detail: "millisatoshi amount is not a whole satoshi".into(),
            });
        }
        Ok(Self(value.0 / 1_000))
    }
}
