//! `agent206 vault …`: a trusted terminal surface for the vault broker. It uses
//! the same broker requests, throttling and step-up rules as the GUI.
use a206_broker::{BrokerError, Client, ClientError, Reply, Request, WireSecret, WireValue};
use a206_runtime::vault;
use a206_vault::{AccessPolicy, CredentialKind, CredentialMeta};
use anyhow::{Result, bail};
use clap::{Subcommand, ValueEnum};
use std::path::Path;

#[derive(Subcommand)]
pub enum VaultAction {
    /// Create a new encrypted vault and unlock it.
    Init,
    /// Unlock the vault in the broker (asks for an authenticator code when enrolled).
    Unlock,
    /// Lock the vault and revoke every step-up.
    Lock,
    /// Let the running gateway use credentials for this unlock (`unlock`
    /// does this automatically when the gateway is already running).
    Pair,
    /// Show whether the vault exists, is unlocked and has two-factor enrolled.
    Status,
    /// Lock the vault and stop the broker process.
    Stop,
    /// List credential names, kinds and policies (never values).
    List,
    /// Add a credential; the value is read from a hidden prompt.
    Add {
        name: String,
        #[arg(long, value_enum, default_value = "api-key")]
        kind: Kind,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, value_enum, default_value = "ask")]
        policy: Policy,
        /// Skill allowed by `--policy skill` (repeatable).
        #[arg(long = "skill")]
        skills: Vec<String>,
        /// Trigger allowed by `--policy trigger` (repeatable).
        #[arg(long = "trigger")]
        triggers: Vec<String>,
        /// Exact origin the broker may send this credential to (repeatable).
        #[arg(long = "destination")]
        destinations: Vec<String>,
    },
    /// Turn an untyped entry from an older vault into a typed credential.
    Adopt {
        name: String,
        /// Store it under a different name (required when the old name
        /// contains `:` or starts with `messenger/`).
        #[arg(long = "as")]
        rename: Option<String>,
        #[arg(long, value_enum, default_value = "api-key")]
        kind: Kind,
        #[arg(long, value_enum, default_value = "ask")]
        policy: Policy,
        #[arg(long = "skill")]
        skills: Vec<String>,
        #[arg(long = "trigger")]
        triggers: Vec<String>,
        #[arg(long = "destination")]
        destinations: Vec<String>,
    },
    /// Change who may use a credential.
    Policy {
        name: String,
        #[arg(value_enum)]
        policy: Policy,
        #[arg(long = "skill")]
        skills: Vec<String>,
        #[arg(long = "trigger")]
        triggers: Vec<String>,
    },
    /// Replace the origins the broker may send a credential to.
    Bind {
        name: String,
        destinations: Vec<String>,
    },
    Disable {
        name: String,
    },
    Enable {
        name: String,
    },
    Delete {
        name: String,
    },
    /// Allow or forbid agents to request any credential use.
    AgentAccess {
        #[arg(value_enum)]
        state: Switch,
    },
    /// Print a credential's value after step-up authentication.
    Reveal {
        name: String,
    },
    /// Manage authenticator (TOTP) two-factor authentication.
    Totp {
        #[command(subcommand)]
        command: TotpAction,
    },
    /// Serve the vault broker for this profile.
    #[command(hide = true)]
    Broker,
}

#[derive(Subcommand)]
pub enum TotpAction {
    /// Enroll an authenticator app; requires a valid code before it takes effect.
    Enroll {
        #[arg(long, default_value = "agent206")]
        account: String,
    },
    /// Remove two-factor authentication; requires a current code.
    Remove,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Kind {
    ApiKey,
    Token,
    Login,
    Passkey,
    Note,
    Other,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Policy {
    /// Agents may use it without asking (Always).
    Open,
    /// Each agent use needs approval (WithApproval).
    Ask,
    /// Each agent use needs fresh authentication (WithAuth).
    Auth,
    /// Only the listed skills (SkillOnly).
    Skill,
    /// Only the listed triggers (TriggerOnly).
    Trigger,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Switch {
    On,
    Off,
}

fn policy(policy: Policy, skills: Vec<String>, triggers: Vec<String>) -> AccessPolicy {
    match policy {
        Policy::Open => AccessPolicy::Always,
        Policy::Ask => AccessPolicy::WithApproval,
        Policy::Auth => AccessPolicy::WithAuth,
        Policy::Skill => AccessPolicy::SkillOnly(skills),
        Policy::Trigger => AccessPolicy::TriggerOnly(triggers),
    }
}

fn hidden(prompt: &str) -> Result<WireSecret> {
    Ok(WireSecret::new(rpassword::prompt_password(prompt)?))
}

async fn connect(root: &Path) -> Result<Client> {
    Ok(vault::connect(root).await?)
}

async fn status(client: &mut Client) -> Result<a206_broker::BrokerStatus> {
    Ok(vault::status(client).await?)
}

/// Unlock with the password, then the authenticator code only when the
/// encrypted vault says two-factor is enrolled.
async fn unlock(client: &mut Client, create: bool) -> Result<()> {
    let password = hidden(if create {
        "New vault password: "
    } else {
        "Vault password: "
    })?;
    if create && hidden("Repeat password: ")?.expose() != password.expose() {
        bail!("passwords do not match");
    }
    let request = |password: &WireSecret, totp| Request::Unlock {
        password: WireSecret::new(password.expose().to_owned()),
        totp,
        create,
    };
    match client.call(request(&password, None)).await {
        Err(ClientError::Broker(BrokerError::TotpRequired)) => {
            let code = rpassword::prompt_password("Authenticator code: ")?;
            client.call(request(&password, Some(code))).await?;
        }
        result => {
            result?;
        }
    }
    Ok(())
}

async fn step_up(client: &mut Client) -> Result<()> {
    let state = status(client).await?;
    if !state.unlocked {
        bail!("the vault is locked; run `agent206 vault unlock`");
    }
    if state.totp_enrolled != Some(true) {
        client
            .call(Request::StepUp {
                password: Some(hidden("Vault password: ")?),
                totp: None,
            })
            .await?;
        return Ok(());
    }
    loop {
        let code = rpassword::prompt_password("Authenticator code: ")?;
        match client
            .call(Request::StepUp {
                password: None,
                totp: Some(code),
            })
            .await
        {
            // Each code works once, so one just used to unlock is refused here.
            Err(ClientError::Broker(BrokerError::TotpReplayed)) => {
                eprintln!("That code was already used; enter the next one from your app.");
            }
            result => {
                result?;
                return Ok(());
            }
        }
    }
}

async fn pair_if_running(client: &mut Client, root: &Path) -> Result<()> {
    if vault::gateway_running(root).await {
        // The unlock itself succeeded; a failed pairing is reported, not fatal.
        match vault::pair_gateway(client, root).await {
            Ok(()) => println!("Gateway paired with the vault"),
            Err(error) => {
                let error = anyhow::Error::from(error);
                eprintln!("Could not pair the gateway ({error:#}); run `agent206 vault pair`")
            }
        }
    } else {
        println!("After starting the gateway, run `agent206 vault pair` so it can use credentials");
    }
    Ok(())
}

fn print_value(value: &WireValue) {
    match value {
        WireValue::Single(v) => println!("{}", v.expose()),
        WireValue::UserPass { username, password } => {
            println!(
                "username\t{}\npassword\t{}",
                username.expose(),
                password.expose()
            )
        }
        WireValue::SshKeyPair {
            private_key,
            public_key,
        } => println!("{}\n{}", public_key.expose(), private_key.expose()),
        WireValue::FormFields(fields) => {
            for (k, v) in fields {
                println!("{k}\t{}", v.expose());
            }
        }
        WireValue::PaymentCard {
            cardholder,
            number,
            expiry,
            cvv,
            extra,
        } => {
            println!(
                "cardholder\t{}\nnumber\t{}\nexpiry\t{}\ncvv\t{}",
                cardholder.expose(),
                number.expose(),
                expiry.expose(),
                cvv.expose()
            );
            for (k, v) in extra {
                println!("{k}\t{}", v.expose());
            }
        }
    }
}

async fn set_disabled(client: &mut Client, name: String, disabled: bool) -> Result<()> {
    step_up(client).await?;
    client
        .call(Request::UpdateCredential {
            name,
            policy: None,
            disabled: Some(disabled),
            destinations: None,
        })
        .await?;
    println!(
        "Credential {}",
        if disabled { "disabled" } else { "enabled" }
    );
    Ok(())
}

pub fn run(root: &Path, action: VaultAction) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async move {
        match action {
            VaultAction::Broker => return Ok(a206_broker::serve(root).await?),
            // Works against a broker from any version, and never starts one.
            VaultAction::Stop => {
                match Client::shutdown(root).await {
                    // Only an absent or unbound socket means no broker.
                    Err(ClientError::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) =>
                    {
                        println!("Vault broker is not running")
                    }
                    result => {
                        result?;
                        println!("Vault broker stopped");
                    }
                }
                return Ok(());
            }
            _ => {}
        }
        let mut client = connect(root).await?;
        match action {
            VaultAction::Broker | VaultAction::Stop => unreachable!(),
            VaultAction::Init => {
                if status(&mut client).await?.exists {
                    bail!("a vault already exists; use `agent206 vault unlock`");
                }
                unlock(&mut client, true).await?;
                println!("Vault created and unlocked");
                pair_if_running(&mut client, root).await?;
            }
            VaultAction::Unlock => {
                unlock(&mut client, false).await?;
                println!("Vault unlocked");
                pair_if_running(&mut client, root).await?;
            }
            VaultAction::Pair => {
                if !vault::gateway_running(root).await {
                    bail!("the gateway is not running; start it with `agent206 gateway start`");
                }
                step_up(&mut client).await?;
                vault::pair_gateway(&mut client, root).await?;
                println!("Gateway paired with the vault");
            }
            VaultAction::Lock => {
                client.call(Request::Lock).await?;
                println!("Vault locked");
            }
            VaultAction::Status => {
                let s = status(&mut client).await?;
                let known = |v: Option<bool>| match v {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown while locked",
                };
                println!("exists\t{}", if s.exists { "yes" } else { "no" });
                println!("unlocked\t{}", if s.unlocked { "yes" } else { "no" });
                println!("two-factor\t{}", known(s.totp_enrolled));
                println!("agent access\t{}", known(s.agent_access));
            }
            VaultAction::List => match client.call(Request::Credentials).await? {
                Reply::Credentials { typed, legacy } => {
                    for name in legacy {
                        println!("{name}\tuntyped (convert with `agent206 vault adopt`)");
                    }
                    for (name, meta) in typed {
                        let state = if meta.disabled { "\tdisabled" } else { "" };
                        println!(
                            "{name}\t{:?}\t{:?}\t{}{state}",
                            meta.kind,
                            meta.policy,
                            meta.destinations.join(",")
                        );
                    }
                }
                _ => bail!("unexpected broker reply"),
            },
            VaultAction::Add {
                name,
                kind,
                label,
                policy: chosen,
                skills,
                triggers,
                destinations,
            } => {
                let Reply::Credentials { typed, .. } = client.call(Request::Credentials).await?
                else {
                    bail!("unexpected broker reply");
                };
                if typed.iter().any(|(existing, _)| *existing == name) {
                    bail!("{name} already exists; delete it first or use policy/bind");
                }
                step_up(&mut client).await?;
                let (kind, value) = match kind {
                    Kind::Login => (
                        CredentialKind::UsernamePassword,
                        WireValue::UserPass {
                            username: hidden("Username: ")?,
                            password: hidden("Password: ")?,
                        },
                    ),
                    single => (
                        match single {
                            Kind::ApiKey => CredentialKind::ApiKey,
                            Kind::Token => CredentialKind::Token,
                            Kind::Passkey => CredentialKind::HttpPasskey,
                            Kind::Note => CredentialKind::SecureNote,
                            _ => CredentialKind::Other,
                        },
                        WireValue::Single(hidden("Secret value: ")?),
                    ),
                };
                let meta = CredentialMeta {
                    label: label.unwrap_or_else(|| name.clone()),
                    kind,
                    policy: policy(chosen, skills, triggers),
                    description: None,
                    disabled: false,
                    destinations,
                };
                client
                    .call(Request::PutCredential {
                        name,
                        meta,
                        value,
                        replace: false,
                    })
                    .await?;
                println!("Credential stored");
            }
            VaultAction::Adopt {
                name: legacy,
                rename,
                kind,
                policy: chosen,
                skills,
                triggers,
                destinations,
            } => {
                let kind = match kind {
                    Kind::ApiKey => CredentialKind::ApiKey,
                    Kind::Token => CredentialKind::Token,
                    Kind::Passkey => CredentialKind::HttpPasskey,
                    Kind::Note => CredentialKind::SecureNote,
                    Kind::Other => CredentialKind::Other,
                    Kind::Login => {
                        bail!("an untyped entry holds one value; choose a single-value kind")
                    }
                };
                let name = rename.unwrap_or_else(|| legacy.clone());
                step_up(&mut client).await?;
                client
                    .call(Request::AdoptLegacy {
                        legacy,
                        meta: CredentialMeta {
                            label: name.clone(),
                            kind,
                            policy: policy(chosen, skills, triggers),
                            description: None,
                            disabled: false,
                            destinations,
                        },
                        name,
                    })
                    .await?;
                println!("Credential converted");
            }
            VaultAction::Policy {
                name,
                policy: chosen,
                skills,
                triggers,
            } => {
                step_up(&mut client).await?;
                client
                    .call(Request::UpdateCredential {
                        name,
                        policy: Some(policy(chosen, skills, triggers)),
                        disabled: None,
                        destinations: None,
                    })
                    .await?;
                println!("Policy updated");
            }
            VaultAction::Bind { name, destinations } => {
                step_up(&mut client).await?;
                client
                    .call(Request::UpdateCredential {
                        name,
                        policy: None,
                        disabled: None,
                        destinations: Some(destinations),
                    })
                    .await?;
                println!("Destinations updated");
            }
            VaultAction::Disable { name } => set_disabled(&mut client, name, true).await?,
            VaultAction::Enable { name } => set_disabled(&mut client, name, false).await?,
            VaultAction::Delete { name } => {
                step_up(&mut client).await?;
                client.call(Request::DeleteCredential { name }).await?;
                println!("Credential deleted");
            }
            VaultAction::AgentAccess { state } => {
                step_up(&mut client).await?;
                client
                    .call(Request::SetAgentAccess {
                        enabled: state == Switch::On,
                    })
                    .await?;
                println!(
                    "Agent access {}",
                    if state == Switch::On { "on" } else { "off" }
                );
            }
            VaultAction::Reveal { name } => {
                step_up(&mut client).await?;
                match client.call(Request::Reveal { name }).await? {
                    Reply::Value(value) => print_value(&value),
                    _ => bail!("unexpected broker reply"),
                }
            }
            VaultAction::Totp { command } => match command {
                TotpAction::Enroll { account } => {
                    step_up(&mut client).await?;
                    let Reply::TotpEnrollment {
                        otpauth_url,
                        manual_key,
                        ..
                    } = client.call(Request::BeginTotp { account }).await?
                    else {
                        bail!("unexpected broker reply");
                    };
                    println!("Add this account to your authenticator app.");
                    println!("Setup key: {}", manual_key.expose());
                    println!("Or open: {}", otpauth_url.expose());
                    loop {
                        let code = rpassword::prompt_password("Code from the app: ")?;
                        match client.call(Request::ConfirmTotp { code }).await {
                            Ok(_) => break,
                            Err(ClientError::Broker(BrokerError::AuthenticationFailed)) => {
                                eprintln!("That code did not match; try the next one.");
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                    println!("Two-factor authentication enrolled");
                }
                TotpAction::Remove => {
                    let password = hidden("Vault password: ")?;
                    let code = rpassword::prompt_password("Authenticator code: ")?;
                    client.call(Request::RemoveTotp { password, code }).await?;
                    println!("Two-factor authentication removed");
                }
            },
        }
        Ok(())
    })
}
