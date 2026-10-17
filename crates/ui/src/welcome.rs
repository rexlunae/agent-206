//! The Get started page: one checklist of first-run setup, with the vault
//! controls beside it. Every step shows what the gateway, the vault broker
//! and the snapshot report right now; nothing is remembered as "done" by the
//! page itself, and a fact nobody reported stays unknown. The steps are the
//! ones `agent206 setup` walks through, plus a provider and a first
//! conversation; projects, messengers and skills follow as extras.
use crate::{PageFrame, State, call, dispatch, error::ErrorNotice, route::Route};
use a206_core::{
    Command, ErrorKind, ErrorReport, GatewayPhase, GatewayReport, MessengerKind, Mode, Reply,
    SkillSummary, Snapshot,
};
use fc_dioxus::prelude::*;

/// What a vault card last read from its broker, in terms the checklist uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VaultFacts {
    pub(crate) exists: bool,
    pub(crate) unlocked: bool,
    /// Unknown while locked: enrollment lives inside the vault.
    pub(crate) totp_enrolled: Option<bool>,
    /// Whether the gateway holds a pairing for this unlock, when the broker says.
    pub(crate) paired: Option<bool>,
}

/// The vault card's latest reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VaultSeen {
    Facts(VaultFacts),
    /// The vault belongs to a remote gateway and is set up on that host.
    Remote,
    /// The state could not be read; the vault card shows why.
    Unreadable,
}

/// Provided by the Get started page so the vault card it embeds can report
/// what it reads. Elsewhere the card finds no listener and reports nothing.
#[derive(Clone, Copy)]
pub(crate) struct VaultProgress(Signal<Option<VaultSeen>>);
impl VaultProgress {
    /// The page's listener, when the calling component is on it. A hook:
    /// call it in the component body, not in a spawned task.
    pub(crate) fn listener() -> Option<Self> {
        try_use_context::<Self>()
    }
    pub(crate) fn report(mut self, seen: VaultSeen) {
        self.0.set(Some(seen));
    }
}

/// How often a vault card on the Get started page re-reads its broker, so
/// a lock or unlock from another client or the CLI reaches the checklist.
pub(crate) const VAULT_POLL: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Progress {
    Done,
    ToDo,
    /// Recommended, not required.
    Optional,
    /// Nobody has reported it, or it is set up somewhere else.
    Unknown,
    Checking,
}
impl Progress {
    fn badge(self) -> (&'static str, BadgeTone, IconName) {
        match self {
            Self::Done => ("Done", BadgeTone::Success, IconName::Check),
            Self::ToDo => ("To do", BadgeTone::Warning, IconName::ArrowEnd),
            Self::Optional => ("Recommended", BadgeTone::Info, IconName::ArrowEnd),
            Self::Unknown => ("Unknown", BadgeTone::Neutral, IconName::Info),
            Self::Checking => ("Checking", BadgeTone::Neutral, IconName::Clock),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Step {
    title: &'static str,
    detail: String,
    progress: Progress,
    /// The page where this step is done, when it is not on this one.
    page: Option<Route>,
}
impl Step {
    fn new(title: &'static str, progress: Progress, detail: impl Into<String>) -> Self {
        Self {
            title,
            detail: detail.into(),
            progress,
            page: None,
        }
    }
    fn on(mut self, page: Route) -> Self {
        self.page = Some(page);
        self
    }
}

fn gateway_step(gateway: Option<&GatewayReport>) -> Step {
    let step = match gateway {
        None => Step::new(
            "Choose and start a gateway",
            Progress::Checking,
            "Checking the configured gateway…",
        ),
        Some(report) => match report.phase {
            GatewayPhase::Running => Step::new(
                "Choose and start a gateway",
                Progress::Done,
                format!("Running: {}", report.target),
            ),
            GatewayPhase::Stopped => Step::new(
                "Choose and start a gateway",
                Progress::ToDo,
                format!(
                    "Stopped: {}. Start it from the Gateway page.",
                    report.target
                ),
            ),
            GatewayPhase::Offline => Step::new(
                "Choose and start a gateway",
                Progress::ToDo,
                format!(
                    "Not reachable: {}. The Gateway page shows why.",
                    report.target
                ),
            ),
        },
    };
    step.on(Route::Gateway {})
}

fn vault_steps(vault: Option<VaultSeen>, gateway: Option<&GatewayReport>) -> [Step; 3] {
    const VAULT: &str = "Create and unlock the vault";
    const TOTP: &str = "Enroll an authenticator";
    const PAIR: &str = "Pair the gateway with the vault";
    let unknown = |title, detail: &str| Step::new(title, Progress::Unknown, detail);
    match vault {
        None => [VAULT, TOTP, PAIR]
            .map(|title| Step::new(title, Progress::Checking, "Checking the vault…")),
        Some(VaultSeen::Remote) => {
            let detail =
                "This gateway runs on another computer. Run `agent206 setup` in a terminal there.";
            [
                unknown(VAULT, detail),
                unknown(TOTP, detail),
                unknown(PAIR, detail),
            ]
        }
        Some(VaultSeen::Unreadable) => {
            let detail = "The vault state could not be read. The vault card below shows why.";
            [
                unknown(VAULT, detail),
                unknown(TOTP, detail),
                unknown(PAIR, detail),
            ]
        }
        Some(VaultSeen::Facts(facts)) => {
            let vault = match (facts.exists, facts.unlocked) {
                (false, _) => Step::new(
                    VAULT,
                    Progress::ToDo,
                    "Choose a password below. It encrypts every credential and cannot be recovered.",
                ),
                (true, false) => Step::new(
                    VAULT,
                    Progress::ToDo,
                    "The vault is locked. Unlock it below.",
                ),
                (true, true) => Step::new(VAULT, Progress::Done, "Created and unlocked."),
            };
            let totp = match facts.totp_enrolled {
                Some(true) => Step::new(TOTP, Progress::Done, "Two-factor authentication is on."),
                Some(false) => Step::new(
                    TOTP,
                    Progress::Optional,
                    "Adds a code from your authenticator app to unlocking and sensitive changes.",
                ),
                None if !facts.exists => Step::new(TOTP, Progress::ToDo, "Create the vault first."),
                None => unknown(TOTP, "Unknown while the vault is locked."),
            };
            // As in the doctor: the broker voids pairings on lock, so a token
            // the gateway still holds counts only while the broker also
            // reports a pairing for this unlock.
            let held = gateway
                .and_then(|report| report.info.as_ref())
                .and_then(|info| info.vault_paired);
            let running = gateway.is_some_and(|report| report.phase == GatewayPhase::Running);
            let pair = match (held, facts.paired) {
                _ if !facts.unlocked => Step::new(
                    PAIR,
                    Progress::ToDo,
                    "Unlock the vault first; locking it ends every pairing.",
                ),
                _ if !running => {
                    Step::new(PAIR, Progress::ToDo, "Start the gateway, then pair it.")
                }
                (Some(false), _) | (_, Some(false)) => Step::new(
                    PAIR,
                    Progress::ToDo,
                    "Not paired. Pair it from the vault card in the desktop or terminal app, or run `agent206 setup`.",
                ),
                (Some(true), Some(true)) => Step::new(
                    PAIR,
                    Progress::Done,
                    "The gateway can make requests that use your credentials.",
                ),
                _ => unknown(
                    PAIR,
                    "The vault and the gateway have not both reported this pairing.",
                ),
            };
            [vault, totp, pair]
        }
    }
}

/// Done once an unexpired catalog lists a model, since a conversation can
/// only use one of those.
fn provider_step(snapshot: &Snapshot, now: u64) -> Step {
    const TITLE: &str = "Connect a model provider";
    let connections = snapshot.settings.providers.len();
    let models = crate::messengers::current_models(snapshot, now).count();
    let step = match (connections, models) {
        (0, _) => Step::new(
            TITLE,
            Progress::ToDo,
            "Add a hosted API or a local server. Models come from the provider's own catalog.",
        ),
        (_, 0) => Step::new(
            TITLE,
            Progress::ToDo,
            format!(
                "Connections: {connections}. No current catalog lists a model; refresh one on Connections."
            ),
        ),
        _ => Step::new(
            TITLE,
            Progress::Done,
            format!("Connections: {connections}. Models listed by their APIs: {models}."),
        ),
    };
    step.on(Route::Providers {})
}

fn conversation_step(snapshot: &Snapshot) -> Step {
    const TITLE: &str = "Start a conversation";
    match snapshot.threads.len() {
        0 => Step::new(
            TITLE,
            Progress::ToDo,
            "Start one below or from the workspace.",
        ),
        count => Step::new(TITLE, Progress::Done, format!("{count} so far.")),
    }
}

fn steps(
    gateway: Option<&GatewayReport>,
    vault: Option<VaultSeen>,
    snapshot: &Snapshot,
    now: u64,
) -> Vec<Step> {
    let mut steps = vec![gateway_step(gateway)];
    steps.extend(vault_steps(vault, gateway));
    steps.push(provider_step(snapshot, now));
    steps.push(conversation_step(snapshot));
    steps
}

/// Steps that widen what Agent 206 can reach. None is needed to start, so they
/// stay out of the checklist count.
/// `skills` is the gateway's last skill listing: `None` until it answers.
fn extras(
    snapshot: &Snapshot,
    skills: Option<&Result<Vec<SkillSummary>, ErrorReport>>,
) -> [Step; 3] {
    let done_or = |done: bool, title, detail: String, hint: String| {
        if done {
            Step::new(title, Progress::Done, detail)
        } else {
            Step::new(title, Progress::Optional, hint)
        }
    };
    let projects = snapshot.projects.len();
    let messengers = &snapshot.settings.messengers;
    let enabled = messengers.iter().filter(|m| m.enabled).count();
    [
        done_or(
            projects > 0,
            "Add a project",
            format!("Projects: {projects}."),
            "Point a project at a folder so its conversations share a root and a coordinator."
                .into(),
        )
        .on(Route::Projects {}),
        done_or(
            enabled > 0,
            "Connect a messenger",
            format!("Turned on: {enabled} of {}.", messengers.len()),
            if messengers.is_empty() {
                let kinds: Vec<_> = MessengerKind::ALL.iter().map(|kind| kind.label()).collect();
                let kinds = match kinds.split_last() {
                    Some((last, rest)) if !rest.is_empty() => {
                        format!("{} or {last}", rest.join(", "))
                    }
                    _ => kinds.concat(),
                };
                format!(
                    "Reach your agent from {kinds}. Any token it signs in with stays in the vault."
                )
            } else {
                "Configured but turned off; a messenger connects only once you turn it on.".into()
            },
        )
        .on(Route::Messengers {}),
        skill_step(skills, done_or).on(Route::Extensions {}),
    ]
}

/// Counts what the gateway offers now: an approval whose `SKILL.md` changed
/// since, or that no longer exists, is not an enabled skill.
fn skill_step(
    skills: Option<&Result<Vec<SkillSummary>, ErrorReport>>,
    done_or: impl Fn(bool, &'static str, String, String) -> Step,
) -> Step {
    const TITLE: &str = "Enable a skill";
    match skills {
        None => Step::new(TITLE, Progress::Checking, "Reading the gateway's skills…"),
        Some(Err(_)) => Step::new(
            TITLE,
            Progress::Unknown,
            "The gateway's skills could not be read; the error is shown below.",
        ),
        Some(Ok(list)) => {
            let enabled = list.iter().filter(|skill| skill.enabled).count();
            let changed = list.iter().filter(|skill| skill.changed).count();
            let hint = if changed > 0 {
                format!(
                    "Changed since you approved them: {changed}. Review them again to turn them back on."
                )
            } else {
                "Review a skill folder from your profile and turn it on. Skills never add permissions."
                    .into()
            };
            done_or(enabled > 0, TITLE, format!("Enabled: {enabled}."), hint)
        }
    }
}

/// Whether the workspace should point at this page: the steps the snapshot
/// alone can tell are unfinished.
pub(crate) fn unfinished(snapshot: &Snapshot, now: u64) -> bool {
    provider_step(snapshot, now).progress != Progress::Done || snapshot.threads.is_empty()
}

#[component]
pub(crate) fn Welcome() -> Element {
    let state = use_context::<State>();
    let progress = use_context_provider(|| VaultProgress(Signal::new(None)));
    let now = crate::messengers::use_catalog_clock(state)();
    let mut skills = use_signal(|| None::<Result<Vec<SkillSummary>, ErrorReport>>);
    // Each read is numbered; only the latest may report, so a slow reply
    // from before an approval or a gateway switch never overwrites a newer one.
    let mut latest = use_signal(|| 0_u64);
    let mut reload = move || {
        let request = latest.peek().wrapping_add(1);
        latest.set(request);
        skills.set(None);
        // Detached: the reply lands in this page's signals, and one that
        // outlives the page or a newer read is dropped by the check below.
        spawn(async move {
            let read = match call(Command::Skills).await {
                Ok(Reply::Skills(list)) => Ok(list),
                Ok(_) => Err(ErrorReport::new(
                    ErrorKind::Protocol,
                    "The gateway answered the skill listing with a different reply",
                )),
                Err(report) => {
                    crate::error::record("skills", &report);
                    Err(report)
                }
            };
            if *latest.peek() == request {
                skills.set(Some(read));
            }
        });
    };
    // Approvals made elsewhere arrive as settings revisions, and a switch to
    // another gateway changes its generation; edits to a skill's files change
    // neither, so the card also offers a refresh.
    let revision = state.data.read().settings.revision;
    let gateway_generation = (state.gateway_generation)();
    use_effect(use_reactive!(|revision, gateway_generation| {
        let _ = (revision, gateway_generation);
        reload();
    }));
    let vault = initial_vault(progress);
    let snapshot = state.data.read();
    let gateway = state.gateway.read();
    let steps = steps(gateway.as_ref(), vault, &snapshot, now);
    let done = steps
        .iter()
        .filter(|step| step.progress == Progress::Done)
        .count();
    let total = steps.len();
    let extras = extras(&snapshot, skills.read().as_ref());
    let skills_failed = skills
        .read()
        .as_ref()
        .and_then(|read| read.as_ref().err().cloned());
    rsx! { PageFrame { kicker: "Get started", title: "Set up Agent 206.", description: "Each step shows what your gateway and vault report now. Finish them in any order; this page updates as you go.",
        Card { variant: CardVariant::Feature,
            CardHeader { CardTitle { "Setup checklist" }
                Badge { tone: if done == total { BadgeTone::Success } else { BadgeTone::Accent }, "{done} of {total} done" }
            }
            CardBody {
                List { ordered: true, divided: true,
                    for step in steps { StepItem { key: "{step.title}", step } }
                }
            }
        }
        Card { variant: CardVariant::Elevated,
            CardHeader { CardTitle { "When you're ready" }
                Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, onclick: move |_| reload(), "Check skills again" }
            }
            CardBody {
                List { divided: true,
                    for step in extras { StepItem { key: "{step.title}", step } }
                }
                if let Some(report) = skills_failed { ErrorNotice { report, title: "Skills could not be read".to_owned() } }
            }
        }
        crate::pages::VaultControls {}
        if snapshot.threads.is_empty() {
            Card { variant: CardVariant::Elevated,
                CardHeader { CardTitle { "Your first conversation" } }
                CardBody { p { class: "a206-muted", "A new conversation starts without tool grants. Choose its model and mode in the composer." } }
                CardFooter { Button { variant: ButtonVariant::Primary, onclick: move |_| dispatch(state, Command::CreateThread { title: "New conversation".into(), mode: Mode::Chat }), "Start a conversation" } }
            }
        }
        Card { variant: CardVariant::Subtle,
            CardBody { p { class: "a206-muted", "Working in a terminal? `agent206 setup` runs the gateway, vault, authenticator and pairing steps through the same requests, then prints the health report." } }
        }
    } }
}

#[component]
fn StepItem(step: Step) -> Element {
    let (label, tone, icon) = step.progress.badge();
    rsx! { ListItem {
        leading: rsx! { Icon { name: icon, size: IconSize::Sm } },
        headline: rsx! { "{step.title}" },
        supporting: rsx! { "{step.detail}" },
        trailing: rsx! { Badge { tone, "{label}" } },
        action: step.page.map_or(ListAction::None, |page| ListAction::Link(page.to_string())),
    } }
}

/// Native clients know before reading the broker whether the vault is on a
/// remote host; the web card learns that from its bridge and reports it.
fn initial_vault(progress: VaultProgress) -> Option<VaultSeen> {
    #[cfg(not(target_arch = "wasm32"))]
    match use_hook(crate::vault::placement) {
        crate::vault::Placement::Remote(_) => return Some(VaultSeen::Remote),
        // The vault card shows why it cannot read the setup.
        crate::vault::Placement::Unreadable(_) | crate::vault::Placement::NoProfile => {
            return Some(VaultSeen::Unreadable);
        }
        crate::vault::Placement::Local(_) => {}
    }
    (progress.0)()
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{Catalog, GatewayInfo, ModelInfo, ModelKey, ProviderApi, ProviderConfig};
    use uuid::Uuid;

    fn text() -> String {
        Uuid::new_v4().to_string()
    }
    fn running(vault_paired: Option<bool>) -> GatewayReport {
        let info = GatewayInfo {
            pid: 1,
            version: text(),
            protocol: 1,
            uptime_seconds: 0,
            data_dir: text(),
            active_runs: 0,
            active_downloads: 0,
            accepting_work: None,
            vault_paired,
        };
        GatewayReport::new(text(), GatewayPhase::Running, Some(info), None)
    }
    fn stopped() -> GatewayReport {
        GatewayReport::new(text(), GatewayPhase::Stopped, None, None)
    }
    fn facts(
        exists: bool,
        unlocked: bool,
        totp_enrolled: Option<bool>,
        paired: Option<bool>,
    ) -> VaultSeen {
        VaultSeen::Facts(VaultFacts {
            exists,
            unlocked,
            totp_enrolled,
            paired,
        })
    }
    fn progress(steps: &[Step]) -> Vec<Progress> {
        steps.iter().map(|step| step.progress).collect()
    }
    /// A snapshot with one connection whose catalog lists one model until `expires_at`.
    fn connected(expires_at: u64) -> Snapshot {
        let mut snapshot = crate::empty_snapshot();
        let provider = Uuid::new_v4();
        snapshot.settings.providers.push(ProviderConfig {
            id: provider,
            label: text(),
            base_url: text(),
            credential: None,
            api: ProviderApi::default(),
        });
        snapshot.catalogs.push(Catalog {
            provider,
            source: text(),
            fetched_at: 0,
            expires_at,
            models: vec![ModelInfo {
                key: ModelKey {
                    provider,
                    model: text(),
                },
                name: text(),
                context_limit: None,
                output_limit: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                reasoning: None,
                reasoning_efforts: vec![],
            }],
            balance: None,
            balance_currency: None,
            token_balance: None,
        });
        snapshot
    }

    #[test]
    fn a_fresh_profile_lists_every_step_as_unfinished() {
        let snapshot = crate::empty_snapshot();
        let steps = steps(
            Some(&stopped()),
            Some(facts(false, false, None, None)),
            &snapshot,
            0,
        );
        assert_eq!(progress(&steps), [Progress::ToDo; 6]);
        assert!(unfinished(&snapshot, 0));
    }

    #[test]
    fn nothing_reported_is_checking_not_done() {
        let steps = steps(None, None, &crate::empty_snapshot(), 0);
        assert_eq!(progress(&steps[..4]), [Progress::Checking; 4]);
    }

    #[test]
    fn a_locked_vault_leaves_enrollment_unknown_and_pairing_to_do() {
        // The gateway still claims a pairing, but locking voids it.
        let report = running(Some(true));
        let [vault, totp, pair] = vault_steps(Some(facts(true, false, None, None)), Some(&report));
        assert_eq!(vault.progress, Progress::ToDo);
        assert_eq!(totp.progress, Progress::Unknown);
        assert_eq!(pair.progress, Progress::ToDo);
    }

    #[test]
    fn pairing_is_done_only_when_broker_and_gateway_both_confirm_it() {
        let unlocked = |paired| facts(true, true, Some(false), paired);
        let [vault, totp, pair] =
            vault_steps(Some(unlocked(Some(true))), Some(&running(Some(true))));
        assert_eq!(
            progress(&[vault, totp, pair]),
            [Progress::Done, Progress::Optional, Progress::Done]
        );
        // A token the gateway kept after another client locked and unlocked
        // the vault is not a pairing the broker accepts.
        let [_, _, pair] = vault_steps(Some(unlocked(None)), Some(&running(Some(true))));
        assert_eq!(pair.progress, Progress::Unknown);
        let [_, _, pair] = vault_steps(Some(unlocked(Some(true))), Some(&running(None)));
        assert_eq!(pair.progress, Progress::Unknown);
        for (broker, gateway) in [(Some(false), Some(true)), (Some(true), Some(false))] {
            let [_, _, pair] = vault_steps(Some(unlocked(broker)), Some(&running(gateway)));
            assert_eq!(pair.progress, Progress::ToDo, "{broker:?} {gateway:?}");
        }
        let [_, _, pair] = vault_steps(Some(unlocked(Some(true))), Some(&stopped()));
        assert_eq!(pair.progress, Progress::ToDo);
        assert!(pair.detail.contains("Start the gateway"), "{}", pair.detail);
    }

    #[test]
    fn a_remote_or_unreadable_vault_is_unknown() {
        for seen in [VaultSeen::Remote, VaultSeen::Unreadable] {
            assert_eq!(
                progress(&vault_steps(Some(seen), Some(&running(Some(true))))),
                [Progress::Unknown; 3]
            );
        }
    }

    #[test]
    fn a_provider_counts_only_while_its_catalog_is_current() {
        let expires_at = u64::from(Uuid::new_v4().as_fields().1) + 1;
        let mut snapshot = connected(expires_at);
        assert_eq!(
            provider_step(&snapshot, expires_at - 1).progress,
            Progress::Done
        );
        assert_eq!(
            provider_step(&snapshot, expires_at).progress,
            Progress::ToDo
        );
        assert!(unfinished(&snapshot, expires_at - 1), "no conversation yet");
        snapshot.catalogs.clear();
        assert_eq!(provider_step(&snapshot, 0).progress, Progress::ToDo);
    }

    #[test]
    fn everything_reported_done_completes_the_checklist() {
        let expires_at = u64::from(Uuid::new_v4().as_fields().1) + 1;
        let mut snapshot = connected(expires_at);
        snapshot
            .threads
            .push(a206_core::Thread::new(text(), a206_core::Mode::Chat));
        let steps = steps(
            Some(&running(Some(true))),
            Some(facts(true, true, Some(true), Some(true))),
            &snapshot,
            0,
        );
        assert_eq!(progress(&steps), [Progress::Done; 6]);
        assert!(!unfinished(&snapshot, 0));
    }

    #[test]
    fn extras_are_recommended_until_the_snapshot_shows_them() {
        let none = Some(&Ok(vec![]));
        let mut snapshot = crate::empty_snapshot();
        assert_eq!(progress(&extras(&snapshot, none)), [Progress::Optional; 3]);
        snapshot.projects.push(a206_core::Project {
            id: Uuid::new_v4(),
            name: text(),
            root: text(),
            manager: Uuid::new_v4(),
        });
        let mut messenger = a206_core::MessengerConfig::new(text(), MessengerKind::ALL[0]);
        snapshot.settings.messengers.push(messenger.clone());
        let [project, off, _] = extras(&snapshot, none);
        assert_eq!(project.progress, Progress::Done);
        // Configured but off is not connected.
        assert_eq!(off.progress, Progress::Optional);
        assert!(off.detail.contains("turned off"), "{}", off.detail);
        messenger.enabled = true;
        snapshot.settings.messengers = vec![messenger];
        let enabled = Ok(vec![SkillSummary {
            name: text(),
            enabled: true,
            ..Default::default()
        }]);
        assert_eq!(
            progress(&extras(&snapshot, Some(&enabled))),
            [Progress::Done; 3]
        );
    }

    #[test]
    fn a_skill_counts_only_while_the_gateway_offers_it() {
        let snapshot = crate::empty_snapshot();
        let skill = |read| extras(&snapshot, read).into_iter().last().unwrap();
        assert_eq!(skill(None).progress, Progress::Checking);
        let failed = Err(ErrorReport::new(ErrorKind::Protocol, text()));
        assert_eq!(skill(Some(&failed)).progress, Progress::Unknown);
        // An approval whose SKILL.md changed since is not offered.
        let changed = Ok(vec![SkillSummary {
            name: text(),
            changed: true,
            ..Default::default()
        }]);
        let step = skill(Some(&changed));
        assert_eq!(step.progress, Progress::Optional);
        assert!(step.detail.contains("Changed since"), "{}", step.detail);
    }

    #[derive(Clone, Debug, PartialEq, Routable)]
    enum PageRoute {
        #[route("/")]
        Page {},
    }
    #[component]
    fn Page() -> Element {
        let state = crate::route::tests::loaded_state(crate::empty_snapshot());
        use_context_provider(|| state);
        rsx! { Welcome {} }
    }

    #[test]
    fn the_page_shows_the_checklist_vault_card_and_first_conversation() {
        use dioxus::history::{History, MemoryHistory};
        use std::rc::Rc;
        crate::route::tests::configured();
        fn app() -> Element {
            rsx! { Router::<PageRoute> {} }
        }
        let mut dom = VirtualDom::new(app);
        dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Setup checklist"), "{html}");
        // Nothing has reported yet, so nothing is counted as done.
        assert!(html.contains("0 of 6 done"), "{html}");
        assert!(html.contains("Checking the configured gateway"), "{html}");
        assert!(html.contains("Checking the vault"), "{html}");
        assert!(
            html.contains(&format!("href=\"{}\"", Route::Providers {})),
            "{html}"
        );
        assert!(html.contains("Start a conversation"), "{html}");
        // Extras link to their pages and stay out of the count.
        assert!(html.contains("Connect a messenger"), "{html}");
        assert!(
            html.contains(&format!("href=\"{}\"", Route::Messengers {})),
            "{html}"
        );
    }
}
