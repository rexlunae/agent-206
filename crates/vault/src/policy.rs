//! Credential metadata and access policies, wire-compatible with RustyClaw's
//! `secrets/types.rs` so imported vault metadata keeps its restrictions.
use serde::{Deserialize, Serialize};

/// What kind of secret a credential holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    HttpPasskey,
    UsernamePassword,
    SshKey,
    Token,
    FormAutofill,
    PaymentMethod,
    SecureNote,
    Other,
}

/// When an agent may use a credential. The default requires per-use approval.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessPolicy {
    Always,
    #[default]
    WithApproval,
    WithAuth,
    /// Only while one of the named skills runs; empty means locked.
    SkillOnly(Vec<String>),
    /// Only for one of the named triggers; empty means locked.
    TriggerOnly(Vec<String>),
}

/// Metadata stored beside a credential's value. Contains no secret material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialMeta {
    pub label: String,
    pub kind: CredentialKind,
    pub policy: AccessPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Exact origins (`scheme://host[:port]`) the broker may send this
    /// credential to on the user's behalf. Agent 206 extension; empty means
    /// no brokered destination, which is how imported RustyClaw entries start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<String>,
}

/// Facts the broker established for one agent-initiated use. Only the trusted
/// approval surface may set `user_approved`; only a fresh vault password/TOTP
/// verification may set `authenticated`.
#[derive(Clone, Debug, Default)]
pub struct AccessContext {
    pub user_approved: bool,
    pub authenticated: bool,
    pub active_skill: Option<String>,
    pub active_trigger: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum Denial {
    #[error("agent access to the vault is turned off")]
    AgentAccessDisabled,
    #[error("credential is disabled")]
    Disabled,
    #[error("this use needs the user's approval")]
    ApprovalRequired,
    #[error("this use needs fresh vault authentication")]
    AuthenticationRequired,
    #[error("the active skill is not permitted")]
    SkillNotPermitted,
    #[error("the active trigger is not permitted")]
    TriggerNotPermitted,
    #[error("the credential is not bound to this destination")]
    DestinationNotBound,
    #[error("this credential kind cannot be sent as a bearer value")]
    UnsupportedKind,
}

impl CredentialMeta {
    /// Decide an agent-initiated use. Unlike RustyClaw, the global agent switch
    /// is a precondition, never a substitute for per-use approval.
    pub fn authorize(&self, agent_access: bool, ctx: &AccessContext) -> Result<(), Denial> {
        if !agent_access {
            return Err(Denial::AgentAccessDisabled);
        }
        if self.disabled {
            return Err(Denial::Disabled);
        }
        let listed = |allowed: &[String], active: &Option<String>| {
            active
                .as_deref()
                .is_some_and(|a| allowed.iter().any(|x| x == a))
        };
        match &self.policy {
            AccessPolicy::Always => Ok(()),
            AccessPolicy::WithApproval if ctx.user_approved => Ok(()),
            AccessPolicy::WithApproval => Err(Denial::ApprovalRequired),
            AccessPolicy::WithAuth if ctx.authenticated => Ok(()),
            AccessPolicy::WithAuth => Err(Denial::AuthenticationRequired),
            AccessPolicy::SkillOnly(allowed) if listed(allowed, &ctx.active_skill) => Ok(()),
            AccessPolicy::SkillOnly(_) => Err(Denial::SkillNotPermitted),
            AccessPolicy::TriggerOnly(allowed) if listed(allowed, &ctx.active_trigger) => Ok(()),
            AccessPolicy::TriggerOnly(_) => Err(Denial::TriggerNotPermitted),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn meta(policy: AccessPolicy) -> CredentialMeta {
        CredentialMeta {
            label: uuid::Uuid::new_v4().to_string(),
            kind: CredentialKind::ApiKey,
            policy,
            description: None,
            disabled: false,
            destinations: vec![],
        }
    }
    fn name() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[test]
    fn every_policy_denies_without_its_condition() {
        let none = AccessContext::default();
        let skill = name();
        let trigger = name();
        let cases = [
            (AccessPolicy::WithApproval, Denial::ApprovalRequired),
            (AccessPolicy::WithAuth, Denial::AuthenticationRequired),
            (AccessPolicy::SkillOnly(vec![]), Denial::SkillNotPermitted),
            (
                AccessPolicy::SkillOnly(vec![skill.clone()]),
                Denial::SkillNotPermitted,
            ),
            (
                AccessPolicy::TriggerOnly(vec![trigger.clone()]),
                Denial::TriggerNotPermitted,
            ),
        ];
        for (policy, denial) in cases {
            assert_eq!(meta(policy).authorize(true, &none), Err(denial));
        }
        let other = AccessContext {
            active_skill: Some(name()),
            active_trigger: Some(name()),
            ..Default::default()
        };
        assert_eq!(
            meta(AccessPolicy::SkillOnly(vec![skill.clone()])).authorize(true, &other),
            Err(Denial::SkillNotPermitted)
        );
        // A skill context does not satisfy a trigger restriction and vice versa.
        let crossed = AccessContext {
            active_skill: Some(trigger.clone()),
            ..Default::default()
        };
        assert_eq!(
            meta(AccessPolicy::TriggerOnly(vec![trigger])).authorize(true, &crossed),
            Err(Denial::TriggerNotPermitted)
        );
    }

    #[test]
    fn conditions_grant_only_their_own_policy() {
        let skill = name();
        let approved = AccessContext {
            user_approved: true,
            ..Default::default()
        };
        assert!(
            meta(AccessPolicy::WithApproval)
                .authorize(true, &approved)
                .is_ok()
        );
        assert_eq!(
            meta(AccessPolicy::WithAuth).authorize(true, &approved),
            Err(Denial::AuthenticationRequired)
        );
        let in_skill = AccessContext {
            active_skill: Some(skill.clone()),
            ..Default::default()
        };
        assert!(
            meta(AccessPolicy::SkillOnly(vec![skill]))
                .authorize(true, &in_skill)
                .is_ok()
        );
        assert!(
            meta(AccessPolicy::Always)
                .authorize(true, &in_skill)
                .is_ok()
        );
    }

    #[test]
    fn global_switch_and_disabled_override_everything() {
        let all = AccessContext {
            user_approved: true,
            authenticated: true,
            ..Default::default()
        };
        assert_eq!(
            meta(AccessPolicy::Always).authorize(false, &all),
            Err(Denial::AgentAccessDisabled)
        );
        let mut disabled = meta(AccessPolicy::Always);
        disabled.disabled = true;
        assert_eq!(disabled.authorize(true, &all), Err(Denial::Disabled));
    }

    #[test]
    fn rustyclaw_metadata_round_trips_with_restrictions() {
        let skill = name();
        let legacy = format!(
            r#"{{"label":"L","kind":"username_password","policy":{{"skill_only":["{skill}"]}},"disabled":true}}"#
        );
        let parsed: CredentialMeta = serde_json::from_str(&legacy).unwrap();
        assert_eq!(parsed.policy, AccessPolicy::SkillOnly(vec![skill]));
        assert!(parsed.disabled);
        assert_eq!(parsed.kind, CredentialKind::UsernamePassword);
        let again: CredentialMeta =
            serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(again, parsed);
        // Missing fields default to the restrictive side.
        let minimal: CredentialMeta =
            serde_json::from_str(r#"{"label":"L","kind":"api_key","policy":"with_approval"}"#)
                .unwrap();
        assert!(!minimal.disabled && minimal.policy == AccessPolicy::WithApproval);
        // Unknown policy variants are rejected, never mapped to a default.
        assert!(
            serde_json::from_str::<CredentialMeta>(
                r#"{"label":"L","kind":"api_key","policy":"future_policy"}"#
            )
            .is_err()
        );
    }
}
