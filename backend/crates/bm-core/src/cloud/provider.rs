//! The clouds a worker can be launched on, and nothing about how.

/// One provider a launch can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provider {
    Aws,
    InstaBox,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Aws => "aws",
            Provider::InstaBox => "instabox",
        }
    }

    /// What an operator may type, aliases included.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "aws" | "ec2" | "amazon" => Some(Provider::Aws),
            "instabox" | "ibx" | "instant" => Some(Provider::InstaBox),
            _ => None,
        }
    }

    /// The deadline a launch gets when neither the command nor the provider's
    /// own config names one. `None` here would mean "no deadline by default",
    /// which is the one default that costs money.
    pub fn default_ttl_minutes(self) -> Option<u32> {
        Some(7 * 60)
    }

    /// Every provider this build can actually launch on, for `:help`.
    pub fn known() -> &'static [Provider] {
        &[Provider::Aws, Provider::InstaBox]
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
