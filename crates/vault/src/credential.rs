//! Typed credentials stored with RustyClaw's key layout: metadata under
//! `cred:<name>`, values under `val:<name>` plus kind-specific suffixes.
use crate::{
    AccessContext, CredentialKind, CredentialMeta, Error, Secret, StepUpWindow, Vault, reserved,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Instant};
use zeroize::Zeroizing;

const AGENT_ACCESS_KEY: &str = "__a206_agent_access";

/// Decrypted credential material. Never serialized, logged or sent to a client.
pub enum CredentialValue {
    Single(Secret),
    UserPass {
        username: Secret,
        password: Secret,
    },
    SshKeyPair {
        private_key: Secret,
        public_key: Secret,
    },
    FormFields(BTreeMap<String, Secret>),
    PaymentCard {
        cardholder: Secret,
        number: Secret,
        expiry: Secret,
        cvv: Secret,
        extra: BTreeMap<String, Secret>,
    },
}

impl std::fmt::Debug for CredentialValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl CredentialValue {
    fn fits(&self, kind: CredentialKind) -> bool {
        use CredentialKind as K;
        matches!(
            (self, kind),
            (Self::UserPass { .. }, K::UsernamePassword)
                | (Self::SshKeyPair { .. }, K::SshKey)
                | (Self::FormFields(_), K::FormAutofill)
                | (Self::PaymentCard { .. }, K::PaymentMethod)
                | (
                    Self::Single(_),
                    K::ApiKey | K::HttpPasskey | K::Token | K::SecureNote | K::Other
                )
        )
    }
}

#[derive(Serialize, Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
struct Card {
    #[serde(default)]
    cardholder: String,
    #[serde(default)]
    number: String,
    #[serde(default)]
    expiry: String,
    #[serde(default)]
    cvv: String,
}

fn exposed_map(map: &BTreeMap<String, Secret>) -> Zeroizing<String> {
    let plain: BTreeMap<&str, Zeroizing<String>> = map
        .iter()
        .map(|(k, v)| (k.as_str(), Zeroizing::new(v.expose(str::to_owned))))
        .collect();
    let plain: BTreeMap<&str, &str> = plain.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Zeroizing::new(serde_json::to_string(&plain).unwrap_or_default())
}

fn secret_map(json: &Secret) -> Result<BTreeMap<String, Secret>, Error> {
    json.expose(|j| serde_json::from_str::<BTreeMap<String, String>>(j))
        .map(|m| m.into_iter().map(|(k, v)| (k, Secret::new(v))).collect())
        .map_err(|_| Error::Corrupted)
}

fn meta_key(name: &str) -> String {
    format!("cred:{name}")
}
fn val_key(name: &str) -> String {
    format!("val:{name}")
}

impl Vault {
    /// Whether agents may use credentials at all. Held in the encrypted vault,
    /// off by default, and changeable only by the user through the broker.
    pub fn agent_access(&self) -> Result<bool, Error> {
        Ok(self
            .raw(AGENT_ACCESS_KEY)?
            .is_some_and(|v| v.expose(|v| v == "enabled")))
    }
    pub fn set_agent_access(&mut self, enabled: bool) -> Result<(), Error> {
        self.commit(|inner| {
            if enabled {
                inner.set(AGENT_ACCESS_KEY, "enabled");
            } else {
                let _ = inner.remove(AGENT_ACCESS_KEY);
            }
        })
    }

    /// Store or replace a typed credential. Stale values from a previous kind
    /// are removed in the same atomic write.
    pub fn put_credential(
        &mut self,
        name: &str,
        meta: &CredentialMeta,
        value: CredentialValue,
    ) -> Result<(), Error> {
        if reserved(name) || name.contains(':') {
            return Err(Error::InvalidName);
        }
        if !value.fits(meta.kind) {
            return Err(Error::KindMismatch);
        }
        let meta_json = serde_json::to_string(meta).map_err(|_| Error::Corrupted)?;
        let val = val_key(name);
        let mut entries: Vec<(String, Zeroizing<String>)> =
            vec![(meta_key(name), Zeroizing::new(meta_json))];
        let mut add = |suffix: &str, s: &Secret| {
            entries.push((
                format!("{val}{suffix}"),
                Zeroizing::new(s.expose(str::to_owned)),
            ));
        };
        match &value {
            CredentialValue::Single(v) => add("", v),
            CredentialValue::UserPass { username, password } => {
                add("", password);
                add(":user", username);
            }
            CredentialValue::SshKeyPair {
                private_key,
                public_key,
            } => {
                add("", private_key);
                add(":pub", public_key);
            }
            CredentialValue::FormFields(fields) => {
                add(":fields", &Secret::new(exposed_map(fields).to_string()));
            }
            CredentialValue::PaymentCard {
                cardholder,
                number,
                expiry,
                cvv,
                extra,
            } => {
                let card = Card {
                    cardholder: cardholder.expose(str::to_owned),
                    number: number.expose(str::to_owned),
                    expiry: expiry.expose(str::to_owned),
                    cvv: cvv.expose(str::to_owned),
                };
                let json = Zeroizing::new(serde_json::to_string(&card).unwrap_or_default());
                add(":card", &Secret::new(json.to_string()));
                if !extra.is_empty() {
                    add(":card_extra", &Secret::new(exposed_map(extra).to_string()));
                }
            }
        }
        let stale = self.value_keys(name)?;
        self.commit(|inner| {
            for key in stale {
                let _ = inner.remove(&key);
            }
            for (key, value) in &entries {
                inner.set(key, value.as_str());
            }
        })
    }

    /// Metadata for every typed credential, without values.
    pub fn credentials(&self) -> Result<Vec<(String, CredentialMeta)>, Error> {
        let names: Vec<String> = self
            .manager()?
            .keys()
            .filter_map(|k| k.strip_prefix("cred:").map(str::to_owned))
            .collect();
        names
            .into_iter()
            .map(|name| self.credential(&name).map(|meta| (name, meta)))
            .collect()
    }

    pub fn credential(&self, name: &str) -> Result<CredentialMeta, Error> {
        let json = self.raw(&meta_key(name))?.ok_or(Error::NotFound)?;
        json.expose(|j| serde_json::from_str::<CredentialMeta>(j))
            .map_err(|_| Error::Corrupted)
    }

    pub fn update_credential(
        &mut self,
        name: &str,
        change: impl FnOnce(&mut CredentialMeta),
    ) -> Result<(), Error> {
        let mut meta = self.credential(name)?;
        change(&mut meta);
        let json = serde_json::to_string(&meta).map_err(|_| Error::Corrupted)?;
        let key = meta_key(name);
        self.commit(|inner| inner.set(&key, json.as_str()))
    }

    pub fn delete_credential(&mut self, name: &str) -> Result<(), Error> {
        self.credential(name)?;
        let mut keys = self.value_keys(name)?;
        keys.push(meta_key(name));
        self.commit(|inner| {
            for key in keys {
                let _ = inner.remove(&key);
            }
        })
    }

    /// Agent-initiated use, decided by the vault-held switch and the
    /// credential's policy. The value reaches only the broker-side closure.
    pub fn use_credential<T>(
        &self,
        name: &str,
        ctx: &AccessContext,
        use_value: impl FnOnce(&CredentialValue) -> T,
    ) -> Result<T, Error> {
        let meta = self.credential(name)?;
        meta.authorize(self.agent_access()?, ctx)?;
        Ok(use_value(&self.load_value(name, meta.kind)?))
    }

    /// Broker-performed use for a user-configured service (for example a model
    /// provider). The value never leaves the broker: the closure must attach
    /// it to a request for `origin`. Agent policy does not apply because no
    /// agent receives the value; the user's destination binding does.
    pub fn use_for_destination<T>(
        &self,
        name: &str,
        origin: &str,
        use_value: impl FnOnce(&Secret) -> T,
    ) -> Result<T, Error> {
        let meta = self.credential(name)?;
        if meta.disabled {
            return Err(crate::Denial::Disabled.into());
        }
        if !matches!(meta.kind, CredentialKind::ApiKey | CredentialKind::Token) {
            return Err(crate::Denial::UnsupportedKind.into());
        }
        if !meta.destinations.iter().any(|d| d == origin) {
            return Err(crate::Denial::DestinationNotBound.into());
        }
        match self.load_value(name, meta.kind)? {
            CredentialValue::Single(value) => Ok(use_value(&value)),
            _ => Err(Error::Corrupted),
        }
    }

    /// User-initiated reveal from the trusted surface. Requires an active
    /// step-up window; disabled credentials remain viewable by their owner.
    pub fn reveal_credential<T>(
        &self,
        name: &str,
        step_up: &StepUpWindow,
        now: Instant,
        use_value: impl FnOnce(&CredentialValue) -> T,
    ) -> Result<T, Error> {
        if !step_up.is_active(now) {
            return Err(crate::Denial::AuthenticationRequired.into());
        }
        let meta = self.credential(name)?;
        Ok(use_value(&self.load_value(name, meta.kind)?))
    }

    fn value_keys(&self, name: &str) -> Result<Vec<String>, Error> {
        let val = val_key(name);
        let suffixed = format!("{val}:");
        Ok(self
            .manager()?
            .keys()
            .filter(|k| {
                *k == val
                    || k.strip_prefix(&suffixed).is_some_and(|s| {
                        ["user", "pub", "fields", "card", "card_extra"].contains(&s)
                    })
            })
            .map(str::to_owned)
            .collect())
    }

    fn load_value(&self, name: &str, kind: CredentialKind) -> Result<CredentialValue, Error> {
        let val = val_key(name);
        let get = |suffix: &str| -> Result<Secret, Error> {
            Ok(self
                .raw(&format!("{val}{suffix}"))?
                .unwrap_or_else(|| Secret::new(String::new())))
        };
        Ok(match kind {
            CredentialKind::UsernamePassword => CredentialValue::UserPass {
                username: get(":user")?,
                password: get("")?,
            },
            CredentialKind::SshKey => CredentialValue::SshKeyPair {
                private_key: get("")?,
                public_key: get(":pub")?,
            },
            CredentialKind::FormAutofill => {
                let json = self.raw(&format!("{val}:fields"))?;
                CredentialValue::FormFields(match json {
                    Some(json) => secret_map(&json)?,
                    None => BTreeMap::new(),
                })
            }
            CredentialKind::PaymentMethod => {
                let mut card: Card = get(":card")?
                    .expose(|j| serde_json::from_str(if j.is_empty() { "{}" } else { j }))
                    .map_err(|_| Error::Corrupted)?;
                let extra = match self.raw(&format!("{val}:card_extra"))? {
                    Some(json) => secret_map(&json)?,
                    None => BTreeMap::new(),
                };
                let take = |s: &mut String| Secret::new(std::mem::take(s));
                CredentialValue::PaymentCard {
                    cardholder: take(&mut card.cardholder),
                    number: take(&mut card.number),
                    expiry: take(&mut card.expiry),
                    cvv: take(&mut card.cvv),
                    extra,
                }
            }
            _ => CredentialValue::Single(get("")?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccessPolicy, Denial, tests::unlocked};

    fn random() -> String {
        uuid::Uuid::new_v4().to_string()
    }
    fn meta(kind: CredentialKind, policy: AccessPolicy) -> CredentialMeta {
        CredentialMeta {
            label: random(),
            kind,
            policy,
            description: None,
            disabled: false,
            destinations: vec![],
        }
    }
    fn single(ctx: &AccessContext, vault: &Vault, name: &str) -> Result<String, Error> {
        vault.use_credential(name, ctx, |v| match v {
            CredentialValue::Single(s) => s.expose(str::to_owned),
            _ => panic!("wrong shape"),
        })
    }

    #[test]
    fn agent_use_follows_switch_policy_and_disabled_state() {
        let (_dir, mut vault, password) = unlocked();
        let name = random();
        let value = random();
        vault
            .put_credential(
                &name,
                &meta(CredentialKind::ApiKey, AccessPolicy::WithApproval),
                CredentialValue::Single(Secret::new(value.clone())),
            )
            .unwrap();
        let approved = AccessContext {
            user_approved: true,
            ..Default::default()
        };
        assert!(matches!(
            single(&approved, &vault, &name),
            Err(Error::Denied(Denial::AgentAccessDisabled))
        ));
        vault.set_agent_access(true).unwrap();
        assert!(matches!(
            single(&AccessContext::default(), &vault, &name),
            Err(Error::Denied(Denial::ApprovalRequired))
        ));
        assert_eq!(single(&approved, &vault, &name).unwrap(), value);
        vault
            .update_credential(&name, |m| m.disabled = true)
            .unwrap();
        assert!(matches!(
            single(&approved, &vault, &name),
            Err(Error::Denied(Denial::Disabled))
        ));
        // State persists in the encrypted file, not in memory or config.
        vault.lock();
        vault.unlock(&password).unwrap();
        assert!(vault.agent_access().unwrap());
        assert!(vault.credential(&name).unwrap().disabled);
        assert_eq!(vault.names().unwrap(), Vec::<String>::new());
        assert_eq!(vault.credentials().unwrap().len(), 1);
    }

    #[test]
    fn every_kind_round_trips_and_replacing_drops_stale_values() {
        let (dir, mut vault, _) = unlocked();
        vault.set_agent_access(true).unwrap();
        let ctx = AccessContext {
            user_approved: true,
            ..Default::default()
        };
        let name = random();
        let (user, pass, field) = (random(), random(), random());
        vault
            .put_credential(
                &name,
                &meta(CredentialKind::UsernamePassword, AccessPolicy::WithApproval),
                CredentialValue::UserPass {
                    username: Secret::new(user.clone()),
                    password: Secret::new(pass.clone()),
                },
            )
            .unwrap();
        let got = vault
            .use_credential(&name, &ctx, |v| match v {
                CredentialValue::UserPass { username, password } => (
                    username.expose(str::to_owned),
                    password.expose(str::to_owned),
                ),
                _ => panic!("wrong shape"),
            })
            .unwrap();
        assert_eq!(got, (user.clone(), pass.clone()));
        let fields = BTreeMap::from([(random(), Secret::new(field.clone()))]);
        vault
            .put_credential(
                &name,
                &meta(CredentialKind::FormAutofill, AccessPolicy::WithApproval),
                CredentialValue::FormFields(fields),
            )
            .unwrap();
        let got = vault
            .use_credential(&name, &ctx, |v| match v {
                CredentialValue::FormFields(f) => f
                    .values()
                    .map(|s| s.expose(str::to_owned))
                    .collect::<Vec<_>>(),
                _ => panic!("wrong shape"),
            })
            .unwrap();
        assert_eq!(got, vec![field.clone()]);
        assert!(!vault.has_raw(&format!("val:{name}:user")).unwrap());
        assert!(!vault.has_raw(&format!("val:{name}")).unwrap());
        let card_name = random();
        let cvv = random();
        vault
            .put_credential(
                &card_name,
                &meta(CredentialKind::PaymentMethod, AccessPolicy::WithApproval),
                CredentialValue::PaymentCard {
                    cardholder: Secret::new(random()),
                    number: Secret::new(random()),
                    expiry: Secret::new(random()),
                    cvv: Secret::new(cvv.clone()),
                    extra: BTreeMap::new(),
                },
            )
            .unwrap();
        let got = vault
            .use_credential(&card_name, &ctx, |v| match v {
                CredentialValue::PaymentCard { cvv, .. } => cvv.expose(str::to_owned),
                _ => panic!("wrong shape"),
            })
            .unwrap();
        assert_eq!(got, cvv);
        let file = std::fs::read_to_string(dir.path().join("vault.json")).unwrap();
        for plain in [&user, &pass, &field, &cvv] {
            assert!(!file.contains(plain.as_str()));
        }
        vault.delete_credential(&name).unwrap();
        assert!(matches!(vault.credential(&name), Err(Error::NotFound)));
        assert!(!vault.has_raw(&format!("val:{name}:fields")).unwrap());
    }

    #[test]
    fn rejects_mismatched_kinds_names_and_corrupted_metadata() {
        let (_dir, mut vault, _) = unlocked();
        let value = || CredentialValue::Single(Secret::new(random()));
        assert!(matches!(
            vault.put_credential(
                &random(),
                &meta(CredentialKind::SshKey, AccessPolicy::Always),
                value()
            ),
            Err(Error::KindMismatch)
        ));
        for bad in ["", "a:user", "__x", "messenger/bot"] {
            assert!(matches!(
                vault.put_credential(
                    bad,
                    &meta(CredentialKind::Token, AccessPolicy::Always),
                    value()
                ),
                Err(Error::InvalidName)
            ));
        }
        let name = random();
        vault
            .commit(|inner| inner.set(&format!("cred:{name}"), "{not json"))
            .unwrap();
        vault.set_agent_access(true).unwrap();
        assert!(matches!(vault.credential(&name), Err(Error::Corrupted)));
        assert!(matches!(
            vault.use_credential(&name, &AccessContext::default(), |_| ()),
            Err(Error::Corrupted)
        ));
    }

    #[test]
    fn destination_use_requires_exact_binding_and_enabled_token() {
        let (_dir, mut vault, _) = unlocked();
        let name = random();
        let value = random();
        let origin = format!("https://{}.test", random());
        let mut held = meta(CredentialKind::ApiKey, AccessPolicy::SkillOnly(vec![]));
        vault
            .put_credential(
                &name,
                &held,
                CredentialValue::Single(Secret::new(value.clone())),
            )
            .unwrap();
        let send = |vault: &Vault, origin: &str| {
            vault.use_for_destination(&name, origin, |s| s.expose(str::to_owned))
        };
        assert!(matches!(
            send(&vault, &origin),
            Err(Error::Denied(Denial::DestinationNotBound))
        ));
        held.destinations = vec![origin.clone()];
        vault
            .update_credential(&name, |m| *m = held.clone())
            .unwrap();
        // Agent access stays off: this path never hands the value to an agent.
        assert_eq!(send(&vault, &origin).unwrap(), value);
        for other in [
            format!("{origin}:8443"),
            origin.replace("https", "http"),
            format!("{origin}.evil"),
        ] {
            assert!(matches!(
                send(&vault, &other),
                Err(Error::Denied(Denial::DestinationNotBound))
            ));
        }
        vault
            .update_credential(&name, |m| m.disabled = true)
            .unwrap();
        assert!(matches!(
            send(&vault, &origin),
            Err(Error::Denied(Denial::Disabled))
        ));
        let login = random();
        let mut login_meta = meta(CredentialKind::UsernamePassword, AccessPolicy::Always);
        login_meta.destinations = vec![origin.clone()];
        vault
            .put_credential(
                &login,
                &login_meta,
                CredentialValue::UserPass {
                    username: Secret::new(random()),
                    password: Secret::new(random()),
                },
            )
            .unwrap();
        assert!(matches!(
            vault.use_for_destination(&login, &origin, |_| ()),
            Err(Error::Denied(Denial::UnsupportedKind))
        ));
    }

    #[test]
    fn reveal_needs_an_active_step_up_window() {
        let (_dir, mut vault, _) = unlocked();
        let name = random();
        let value = random();
        let mut held = meta(CredentialKind::Token, AccessPolicy::Always);
        held.disabled = true;
        vault
            .put_credential(
                &name,
                &held,
                CredentialValue::Single(Secret::new(value.clone())),
            )
            .unwrap();
        let start = Instant::now();
        let mut window = StepUpWindow::default();
        let reveal = |w: &StepUpWindow, at| {
            vault.reveal_credential(&name, w, at, |v| match v {
                CredentialValue::Single(s) => s.expose(str::to_owned),
                _ => panic!("wrong shape"),
            })
        };
        assert!(reveal(&window, start).is_err());
        window.grant(start);
        assert_eq!(reveal(&window, start).unwrap(), value);
        assert!(reveal(&window, start + StepUpWindow::DURATION).is_err());
        window.grant(start);
        window.revoke();
        assert!(reveal(&window, start).is_err());
    }
}
