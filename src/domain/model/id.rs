use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BookId(uuid::Uuid);

impl Default for BookId {
    fn default() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl BookId {
    pub fn new() -> Self {
        Self::default()
    }
}

impl fmt::Display for BookId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(uuid::Uuid);

impl Default for NodeId {
    fn default() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl NodeId {
    pub fn new() -> Self {
        Self::default()
    }

    /// 短縮ID（UUIDの先頭8文字）
    pub fn short(&self) -> String {
        self.0.to_string()[..8].to_string()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
