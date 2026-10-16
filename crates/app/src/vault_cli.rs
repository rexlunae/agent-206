//! `agent206 vault …`: a trusted terminal surface for the vault broker. It uses
//! the same broker requests, throttling and step-up rules as the GUI.
use a206_broker::{BrokerError, Client, ClientError, Reply, Request, Role, WireSecret, WireValue};
use a206_vault::{AccessPolicy, CredentialKind, CredentialMeta};
use anyhow::{Context, Result, bail};
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
    if let Ok(client) = Client::connect(root, Role::User).await {
        return Ok(client);
    }
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = root.join("vault");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join("broker.log"))?;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .arg("--data-dir")
        .arg(root)
        .args(["vault", "broker"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()
        .context("start the vault broker")?;
    for _ in 0..100 {
        if let Ok(client) = Client::connect(root, Role::User).await {
            // The broker outlives this command; reap it if it exits first.
            std::thread::spawn(move || child.wait());
            return Ok(client);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let _ = child.kill();
    bail!(
        "vault broker did not start; inspect {}",
        dir.join("broker.log").display()
    )
}

async fn status(client: &mut Client) -> Result<a206_broker::BrokerStatus> {
    match client.call(Request::Status).await? {
        Reply::Status(status) => Ok(status),
        _ => bail!("unexpected broker reply"),
    }
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
    let request = if state.totp_enrolled == Some(true) {
        Request::StepUp {
            password: None,
            totp: Some(rpassword::prompt_password("Authenticator code: ")?),
        }
    } else {
        Request::StepUp {
            password: Some(hidden("Vault password: ")?),
            totp: None,
        }
    };
    client.call(request).await?;
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
        if let VaultAction::Broker = action {
            return Ok(a206_broker::serve(root).await?);
        }
        let mut client = connect(root).await?;
        match action {
            VaultAction::Broker => unreachable!(),
            VaultAction::Init => {
                if status(&mut client).await?.exists {
                    bail!("a vault already exists; use `agent206 vault unlock`");
                }
                unlock(&mut client, true).await?;
                println!("Vault created and unlocked");
            }
            VaultAction::Unlock => {
                unlock(&mut client, false).await?;
                println!("Vault unlocked");
            }
            VaultAction::Lock => {
                client.call(Request::Lock).await?;
                println!("Vault locked");
            }
            VaultAction::Stop => {
                client.call(Request::Shutdown).await?;
                println!("Vault broker stopped");
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
                Reply::Credentials(list) => {
                    for (name, meta) in list {
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
                let put = |value| Request::PutCredential {
                    name: name.clone(),
                    meta: meta.clone(),
                    value,
                };
                match client.call(put(value)).await {
                    Err(ClientError::Broker(BrokerError::StepUpRequired)) => {
                        bail!("{name} already exists; delete it first or use policy/bind")
                    }
                    result => {
                        result?;
                    }
                }
                println!("Credential stored");
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
