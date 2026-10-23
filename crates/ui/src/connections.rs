//! Explicit user edits of existing accounts. Drafts retain their settings
//! revision; polling must never rebase a draft onto a different saved account.
use crate::{State, error::ErrorNotice};
use a206_core::{Command, ErrorKind, ErrorReport, Id, ProviderConfig, Reply, Settings};
use fc_dioxus::prelude::*;

fn update(base: &Settings, draft: &ProviderConfig) -> Result<Settings, ErrorReport> {
    let mut settings = base.clone();
    let account = settings
        .providers
        .iter_mut()
        .find(|p| p.id == draft.id)
        .ok_or_else(|| {
            ErrorReport::new(
                ErrorKind::NotFound,
                "Connection was removed. Reload connections.",
            )
        })?;
    if draft.label.trim().is_empty() || draft.base_url.trim().is_empty() {
        return Err(ErrorReport::new(
            ErrorKind::InvalidRequest,
            "Connection name and API base URL are required.",
        ));
    }
    *account = ProviderConfig {
        label: draft.label.trim().into(),
        base_url: draft.base_url.trim().into(),
        credential: draft
            .credential
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        ..draft.clone()
    };
    Ok(settings)
}

fn saved(reply: Reply) -> Result<(), ErrorReport> {
    match reply {
        Reply::Ok => Ok(()),
        Reply::Error(report) => Err(report),
        _ => Err(ErrorReport::new(
            ErrorKind::Protocol,
            "Unexpected connection update response.",
        )),
    }
}

fn change(
    base: &Settings,
    revision: u64,
    draft: Option<&ProviderConfig>,
    online: bool,
    busy: bool,
) -> Option<Settings> {
    if !online || busy || base.revision != revision {
        return None;
    }
    let draft = draft?;
    // Compare the raw draft before normalization: opening or reloading an
    // account must never submit whitespace cleanup as a user edit.
    if base.providers.iter().find(|p| p.id == draft.id) == Some(draft) {
        return None;
    }
    // Invalid local fields disable Save; gateway errors from actual saves
    // remain structured and are displayed below.
    let next = update(base, draft).ok()?;
    (next != *base).then_some(next)
}

#[component]
pub(crate) fn ConnectionEditor(
    initial: Settings,
    provider: Id,
    onclose: EventHandler<()>,
) -> Element {
    let state = use_context::<State>();
    let mut base = use_signal(|| initial.clone());
    let mut draft = use_signal(|| initial.providers.iter().find(|p| p.id == provider).cloned());
    let mut busy = use_signal(|| false);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let api = use_memo(move || draft().map(|p| p.api.label().to_owned()));
    let name = use_memo(move || draft().map(|p| p.label).unwrap_or_default());
    let url = use_memo(move || draft().map(|p| p.base_url).unwrap_or_default());
    let credential = use_memo(move || draft().and_then(|p| p.credential).unwrap_or_default());
    let stale = base.read().revision != state.data.read().settings.revision;
    let online = (state.connected)();
    let disabled = busy() || !online;
    let availability = if disabled {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let can_save = change(
        &base.read(),
        state.data.read().settings.revision,
        draft.read().as_ref(),
        online,
        busy(),
    )
    .is_some();
    rsx! { Dialog { open: true, title: rsx! { "Edit connection" }, size: DialogSize::Lg, scrollable: true,
        close_label: name!("Close connection editor"), aria_busy: "{busy}",
        description: rsx! { "Changing a connection stops its active responses and clears its cached model catalog. Refresh models after saving. This does not grant access to a vault credential." },
        onclose: move |_| {
            // Keep the editor alive until a sent save is confirmed and refreshed;
            // closing a dialog cannot roll back a command already received.
            if !busy() { onclose.call(()); }
        },
        div { class: "a206-stack",
        if busy() { p { role: "status", "Saving connection… Wait for the result before closing." } }
        if stale { Alert { tone: AlertTone::Warning, "Settings changed. Your draft is preserved. Reload before editing the current connection." } }
        if !online { p { "Gateway is disconnected. Reconnect before saving." } }
        if draft().is_none() { p { "Connection is unavailable. Close this editor and refresh connections." } }
        div { class: "a206-grid",
            Field { label: rsx! { "Connection name" }, Input { availability: availability.clone(), value: ReadSignal::from(name), oninput: move |v| { if let Some(p) = draft.write().as_mut() { p.label = v; } } } }
            Field { label: rsx! { "API base URL" }, hint: rsx! { "HTTPS or loopback HTTP. No credentials or query parameters." }, Input { r#type: InputType::Url, availability: availability.clone(), value: ReadSignal::from(url), oninput: move |v| { if let Some(p) = draft.write().as_mut() { p.base_url = v; } } } }
            Field { label: rsx! { "Protocol" }, Select { label: name!("Connection API protocol"), availability: availability.clone(), value: ReadSignal::from(api), options: a206_core::ProviderApi::ALL.into_iter().map(|a| SelectChoice::new(a.label(), a.label())).collect(), onchange: move |v| { if let Some(api) = a206_core::ProviderApi::ALL.into_iter().find(|a| a.label() == v) && let Some(p) = draft.write().as_mut() { p.api = api; } } } }
            Field { label: rsx! { "Vault credential name" }, hint: rsx! { "Saved credential reference only. Never paste a key. Leave blank to remove the reference." }, Input { availability: availability.clone(), value: ReadSignal::from(credential), oninput: move |v: String| { if let Some(p) = draft.write().as_mut() { p.credential = (!v.is_empty()).then_some(v); } } } }
        }
        if let Some(report) = failure() { ErrorNotice { report, title: "Connection not saved".to_string() } }
        div { class: "a206-row",
            Button { variant: ButtonVariant::Primary, availability: if can_save { Availability::Enabled } else { Availability::Disabled }, onclick: move |_| {
                let Some(settings) = change(&base.read(), state.data.read().settings.revision, draft.read().as_ref(), (state.connected)(), busy()) else { return; };
                busy.set(true); failure.set(None);
                // Deliberately detached from this synchronous event handler,
                // but owned by the Dioxus editor scope and canceled on unmount
                // (including a gateway switch). A sent save may still commit;
                // cancellation is not rollback. The next snapshot is authoritative.
                // While mounted, retain errors and await refresh before closing.
                spawn(async move {
                    let result = crate::call(Command::SaveSettings(Box::new(settings))).await.and_then(saved);
                    crate::refresh(state).await;
                    busy.set(false);
                    match result { Ok(()) => onclose.call(()), Err(report) => failure.set(Some(report)) }
                });
            }, "Save changes" }
            Button { variant: ButtonVariant::Secondary, availability: availability.clone(), onclick: move |_| {
                let settings = state.data.read().settings.clone();
                draft.set(settings.providers.iter().find(|p| p.id == provider).cloned());
                base.set(settings); failure.set(None);
            }, "Reload saved values" }
            Button { variant: ButtonVariant::Ghost, availability: if busy() { Availability::Disabled } else { Availability::Enabled }, onclick: move |_| onclose.call(()), "Cancel" }
        }
    } } }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone, Debug, PartialEq, Routable)]
    enum FixtureRoute {
        #[route("/")]
        Page {},
    }
    #[component]
    fn Page() -> Element {
        let snapshot = use_context::<a206_core::Snapshot>();
        let state = crate::route::tests::loaded_state(snapshot.clone());
        use_context_provider(|| state);
        let provider = snapshot.settings.providers[0].id;
        rsx! { ConnectionEditor { initial: snapshot.settings, provider, onclose: |_| {} } }
    }
    #[test]
    fn account_edits_are_in_a_dialog_with_cancel_and_saved_value_recovery() {
        use dioxus::history::{History, MemoryHistory};
        use std::rc::Rc;
        fn app() -> Element {
            rsx! { Router::<FixtureRoute> {} }
        }
        let (settings, _) = fixture();
        let mut snapshot = crate::empty_snapshot();
        snapshot.settings = settings;
        let mut dom = VirtualDom::new(app);
        dom.provide_root_context(snapshot);
        dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        let dialog = html
            .split("<dialog")
            .nth(1)
            .unwrap()
            .split("</dialog>")
            .next()
            .unwrap();
        for control in [
            "Edit connection",
            "Close connection editor",
            "Cancel",
            "Reload saved values",
            "Save changes",
        ] {
            assert!(dialog.contains(control));
        }
        assert!(!html.contains("<h3>Edit connection</h3>"));
    }
    fn fixture() -> (Settings, ProviderConfig) {
        let account = ProviderConfig {
            id: Id::new_v4(),
            label: Id::new_v4().to_string(),
            base_url: "https://localhost/v1".into(),
            api: Default::default(),
            credential: None,
        };
        let mut settings = Settings::default();
        settings.providers.push(account.clone());
        (settings, account)
    }
    #[test]
    fn edits_preserve_identity_revision_and_other_settings() {
        let (mut base, mut account) = fixture();
        base.revision = 17;
        let untouched = base.clone();
        account.label = " Renamed ".into();
        account.credential = Some(" saved-reference ".into());
        let next = update(&base, &account).unwrap();
        assert_eq!(next.revision, 17);
        assert_eq!(next.providers[0].id, account.id);
        assert_eq!(next.providers[0].label, "Renamed");
        assert_eq!(
            next.providers[0].credential.as_deref(),
            Some("saved-reference")
        );
        assert_eq!(base, untouched);
        let mut expected = base;
        expected.providers[0] = next.providers[0].clone();
        assert_eq!(next, expected);
    }
    #[test]
    fn empty_reference_removes_binding_and_removed_accounts_are_not_recreated() {
        let (mut base, mut account) = fixture();
        account.credential = Some("  ".into());
        assert_eq!(
            update(&base, &account).unwrap().providers[0].credential,
            None
        );
        base.providers.clear();
        assert_eq!(
            update(&base, &account).unwrap_err().kind,
            ErrorKind::NotFound
        );
    }
    #[test]
    fn missing_fields_and_unexpected_success_replies_are_refused() {
        let (base, mut account) = fixture();
        account.label.clear();
        assert!(update(&base, &account).is_err());
        assert!(saved(Reply::Text("not confirmation".into())).is_err());
        let error =
            ErrorReport::new(ErrorKind::Conflict, "Concurrent edit").with_correlation(Id::new_v4());
        assert_eq!(saved(Reply::Error(error.clone())).unwrap_err(), error);
    }

    #[test]
    fn concurrent_updates_do_not_rebase_or_submit_a_draft() {
        let (base, mut account) = fixture();
        assert!(change(&base, base.revision, Some(&account), true, false).is_none());
        account.label = "Unsaved draft".into();
        assert!(change(&base, base.revision, Some(&account), true, false).is_some());
        assert!(change(&base, base.revision + 1, Some(&account), true, false).is_none());
        assert_eq!(account.label, "Unsaved draft");
        assert!(change(&base, base.revision, Some(&account), false, false).is_none());
        assert!(change(&base, base.revision, Some(&account), true, true).is_none());
    }

    #[test]
    fn untouched_whitespace_is_not_an_edit() {
        let (mut base, mut account) = fixture();
        account.label = format!(" {} ", account.label);
        account.base_url = format!(" {} ", account.base_url);
        for reference in [None, Some(" saved-reference ".into()), Some("  ".into())] {
            account.credential = reference;
            base.providers[0] = account.clone();
            assert!(change(&base, base.revision, Some(&account), true, false).is_none());
            let mut edited = account.clone();
            edited.label = "Deliberate rename".into();
            assert!(change(&base, base.revision, Some(&edited), true, false).is_some());
        }
    }
}
