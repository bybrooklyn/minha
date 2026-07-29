//! The deliberately small tool surface exposed to agents.
//!
//! The catalog in this module is closed on purpose.  Callers may choose a
//! policy, but cannot manufacture a new tool or silently widen a role's
//! permissions at runtime.

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Role {
    Planner,
    Implementer,
    Reviewer,
    Integrator,
    Recovery,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Risk {
    Read,
    LocalWrite,
    Destructive,
    RemoteWrite,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Tool {
    Git,
    Worktree,
    Rewind,
    Merge,
    Recovery,
    Github,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolSpec {
    pub tool: Tool,
    pub risk: Risk,
    pub roles: &'static [Role],
}

const ALL: &[Role] = &[
    Role::Planner,
    Role::Implementer,
    Role::Reviewer,
    Role::Integrator,
    Role::Recovery,
];
const IMPLEMENT: &[Role] = &[Role::Implementer, Role::Integrator, Role::Recovery];
const INTEGRATE: &[Role] = &[Role::Integrator, Role::Recovery];
const RECOVER: &[Role] = &[Role::Recovery, Role::Integrator];

pub const TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        tool: Tool::Git,
        risk: Risk::Read,
        roles: ALL,
    },
    ToolSpec {
        tool: Tool::Worktree,
        risk: Risk::LocalWrite,
        roles: IMPLEMENT,
    },
    ToolSpec {
        tool: Tool::Rewind,
        risk: Risk::Destructive,
        roles: RECOVER,
    },
    ToolSpec {
        tool: Tool::Merge,
        risk: Risk::LocalWrite,
        roles: INTEGRATE,
    },
    ToolSpec {
        tool: Tool::Recovery,
        risk: Risk::LocalWrite,
        roles: RECOVER,
    },
    ToolSpec {
        tool: Tool::Github,
        risk: Risk::RemoteWrite,
        roles: INTEGRATE,
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolRequest {
    pub role: Role,
    pub tool: Tool,
    pub acknowledged_risk: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizationError {
    UnknownTool,
    RoleDenied,
    RiskNotAcknowledged,
}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownTool => "tool is not in the fixed catalog",
            Self::RoleDenied => "role is not allowed to use this tool",
            Self::RiskNotAcknowledged => "this tool requires explicit risk acknowledgement",
        })
    }
}

impl std::error::Error for AuthorizationError {}

pub fn spec(tool: Tool) -> Option<&'static ToolSpec> {
    TOOL_SPECS.iter().find(|candidate| candidate.tool == tool)
}

pub fn authorize(request: ToolRequest) -> Result<&'static ToolSpec, AuthorizationError> {
    let Some(spec) = spec(request.tool) else {
        return Err(AuthorizationError::UnknownTool);
    };
    if !spec.roles.contains(&request.role) {
        return Err(AuthorizationError::RoleDenied);
    }
    if spec.risk >= Risk::Destructive && !request.acknowledged_risk {
        return Err(AuthorizationError::RiskNotAcknowledged);
    }
    Ok(spec)
}

/// A policy-bound runner.  It validates every invocation before dispatching
/// it, so callers cannot accidentally execute a command as another role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RiskRunner {
    role: Role,
    acknowledge_risk: bool,
}

impl RiskRunner {
    pub const fn new(role: Role) -> Self {
        Self {
            role,
            acknowledge_risk: false,
        }
    }
    pub const fn with_risk_acknowledged(mut self) -> Self {
        self.acknowledge_risk = true;
        self
    }
    pub const fn role(self) -> Role {
        self.role
    }
    pub fn check(self, tool: Tool) -> Result<&'static ToolSpec, AuthorizationError> {
        authorize(ToolRequest {
            role: self.role,
            tool,
            acknowledged_risk: self.acknowledge_risk,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roles_and_risks_are_fixed() {
        assert!(RiskRunner::new(Role::Reviewer).check(Tool::Git).is_ok());
        assert_eq!(
            RiskRunner::new(Role::Implementer).check(Tool::Merge),
            Err(AuthorizationError::RoleDenied)
        );
        assert_eq!(
            RiskRunner::new(Role::Integrator).check(Tool::Github),
            Err(AuthorizationError::RiskNotAcknowledged)
        );
        assert!(
            RiskRunner::new(Role::Integrator)
                .with_risk_acknowledged()
                .check(Tool::Github)
                .is_ok()
        );
    }
}
