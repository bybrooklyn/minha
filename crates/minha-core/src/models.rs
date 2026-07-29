//! Model names and deterministic routing.
//!
//! Routing is deliberately local.  The core crate describes a route, but it
//! does not know how to discover or call a provider endpoint.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Model {
    Spark,
    Luna,
    Terra,
    Sol,
}

impl Model {
    pub const ALL: [Self; 4] = [Self::Spark, Self::Luna, Self::Terra, Self::Sol];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spark => "spark",
            Self::Luna => "luna",
            Self::Terra => "terra",
            Self::Sol => "sol",
        }
    }

    pub const fn slug(self) -> &'static str {
        match self {
            Self::Spark => "gpt-5.3-codex-spark",
            Self::Luna => "gpt-5.6-luna",
            Self::Terra => "gpt-5.6-terra",
            Self::Sol => "gpt-5.6-sol",
        }
    }

    /// Relative priority, where a larger value means more capable and costly.
    pub const fn priority(self) -> u8 {
        match self {
            Self::Spark => 0,
            Self::Luna => 1,
            Self::Terra => 2,
            Self::Sol => 3,
        }
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for Model {
    type Error = UnknownModel;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.trim().to_ascii_lowercase().as_str() {
            "spark" | "gpt-5.3-codex-spark" => Ok(Self::Spark),
            "luna" | "gpt-5.6-luna" => Ok(Self::Luna),
            "terra" | "gpt-5.6-terra" => Ok(Self::Terra),
            "sol" | "gpt-5.6-sol" => Ok(Self::Sol),
            _ => Err(UnknownModel(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownModel(pub String);

impl fmt::Display for UnknownModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown model: {}", self.0)
    }
}

impl std::error::Error for UnknownModel {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteIntent {
    Fast,
    #[default]
    Balanced,
    Reasoning,
    Quality,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteRequest {
    pub intent: RouteIntent,
    pub candidates: Vec<Model>,
}

impl Default for RouteRequest {
    fn default() -> Self {
        Self {
            intent: RouteIntent::Balanced,
            candidates: Model::ALL.to_vec(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Route {
    pub model: Model,
    pub intent: RouteIntent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteError {
    NoCandidates,
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no model candidates available")
    }
}

impl std::error::Error for RouteError {}

pub struct Router;

impl Router {
    pub fn route(request: &RouteRequest) -> Result<Route, RouteError> {
        let preferred = match request.intent {
            RouteIntent::Fast => Model::Spark,
            RouteIntent::Balanced => Model::Luna,
            RouteIntent::Reasoning => Model::Terra,
            RouteIntent::Quality => Model::Sol,
        };
        let model = request
            .candidates
            .iter()
            .copied()
            .find(|m| *m == preferred)
            .or_else(|| match request.intent {
                RouteIntent::Fast => request.candidates.iter().copied().min_by_key(|m| m.priority()),
                _ => request.candidates.iter().copied().max_by_key(|m| m.priority()),
            })
            .ok_or(RouteError::NoCandidates)?;
        Ok(Route {
            model,
            intent: request.intent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_each_intent_to_its_named_tier() {
        for (intent, expected) in [
            (RouteIntent::Fast, Model::Spark),
            (RouteIntent::Balanced, Model::Luna),
            (RouteIntent::Reasoning, Model::Terra),
            (RouteIntent::Quality, Model::Sol),
        ] {
            assert_eq!(
                Router::route(&RouteRequest {
                    intent,
                    candidates: Model::ALL.to_vec()
                })
                .expect("test operation should succeed")
                .model,
                expected
            );
        }
    }

    #[test]
    fn falls_back_without_crossing_the_available_set() {
        let request = RouteRequest {
            intent: RouteIntent::Quality,
            candidates: vec![Model::Spark, Model::Terra],
        };
        assert_eq!(
            Router::route(&request)
                .expect("test operation should succeed")
                .model,
            Model::Terra
        );
    }
}
