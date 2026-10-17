//! What a pre-install audit found in an author's skill or extension text.
//! Findings are advice for the user's review: they neither approve nor block,
//! and a clean report does not make content safe.
use serde::{Deserialize, Serialize};

/// Most findings kept for one skill or extension; the rest are counted.
pub const MAX_AUDIT_FINDINGS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditRule {
    /// Downloads a script and runs it in one step.
    RemoteScript,
    /// Opens a shell for another computer.
    ReverseShell,
    /// Names a file where credentials or keys are kept.
    CredentialFile,
    /// Tells the model to set aside its instructions or hide from the user.
    InstructionOverride,
    /// Contains invisible or direction-changing characters.
    HiddenText,
    /// Contains what looks like a key or token.
    SecretValue,
    /// Decodes text and runs it, hiding what runs.
    EncodedCommand,
}
impl AuditRule {
    pub const ALL: [Self; 7] = [
        Self::RemoteScript,
        Self::ReverseShell,
        Self::CredentialFile,
        Self::InstructionOverride,
        Self::HiddenText,
        Self::SecretValue,
        Self::EncodedCommand,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::RemoteScript => "Downloads a script and runs it",
            Self::ReverseShell => "Opens a shell for another computer",
            Self::CredentialFile => "Names a file where credentials are kept",
            Self::InstructionOverride => {
                "Tells the model to ignore its instructions or keep something from you"
            }
            Self::HiddenText => "Contains invisible or direction-changing characters",
            Self::SecretValue => "Contains what looks like a key or token",
            Self::EncodedCommand => "Decodes hidden text and runs it",
        }
    }
}

/// Where a finding is, so the user can find it in what they review.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditLocation {
    /// A line of the reviewed file, counted from 1.
    Line(u32),
    /// A text field of an extension's manifest, such as a tool's description.
    Field {
        /// The tool, parameter or panel the field belongs to; none for the
        /// extension itself.
        item: Option<String>,
        field: AuditField,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditField {
    Name,
    Description,
    ToolName,
    ToolDescription,
    ParameterDescription,
    PanelTitle,
}
impl AuditField {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Description => "description",
            Self::ToolName => "tool name",
            Self::ToolDescription => "tool description",
            Self::ParameterDescription => "parameter description",
            Self::PanelTitle => "panel title",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AuditFinding {
    pub rule: AuditRule,
    pub location: AuditLocation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReport {
    /// At most [`MAX_AUDIT_FINDINGS`], in reading order.
    pub findings: Vec<AuditFinding>,
    /// Findings beyond the ones listed.
    #[serde(default)]
    pub more: u32,
}
impl AuditReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty() && self.more == 0
    }
    /// Add a distinct finding: keep it while under the cap, else count it.
    /// Callers deduplicate, since a counted finding is no longer here to
    /// compare against.
    pub fn push(&mut self, finding: AuditFinding) {
        if self.findings.len() < MAX_AUDIT_FINDINGS {
            self.findings.push(finding);
        } else {
            self.more = self.more.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI prints reports as TOML, so every location must serialize.
    #[test]
    fn a_report_with_findings_serializes_as_toml() {
        #[derive(Serialize)]
        struct Listing {
            audit: AuditReport,
        }
        let item = format!("tool{}", MAX_AUDIT_FINDINGS + 1);
        let mut audit = AuditReport::default();
        audit.push(AuditFinding {
            rule: AuditRule::RemoteScript,
            location: AuditLocation::Line(7),
        });
        audit.push(AuditFinding {
            rule: AuditRule::HiddenText,
            location: AuditLocation::Field {
                item: Some(item.clone()),
                field: AuditField::ToolDescription,
            },
        });
        let text = toml::to_string_pretty(&Listing { audit }).unwrap();
        assert!(text.contains("line = 7"), "{text}");
        assert!(text.contains(&item), "{text}");
        assert!(text.contains("tool_description"), "{text}");
    }
}
