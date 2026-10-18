//! Personal preview preference, never a publication permission.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PublicationPreference {
    Individual,
    #[default]
    Batch,
}

impl PublicationPreference {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Individual => "Individual comments",
            Self::Batch => "Batch review",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Individual => "individual",
            Self::Batch => "batch",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "individual" => Ok(Self::Individual),
            "batch" => Ok(Self::Batch),
            _ => anyhow::bail!("unsupported local publication preference"),
        }
    }
}
