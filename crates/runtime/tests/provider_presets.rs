use a206_core::*;
use a206_protocol::Request;
use a206_runtime::gateway::Gateway;
async fn save(gateway: &Gateway, settings: Settings) -> Reply {
    gateway
        .request(Request {
            id: Id::new_v4(),
            body: Command::SaveSettings(settings),
        })
        .await
        .body
}
#[tokio::test]
async fn persists_custom_overridden_and_hidden_presets_without_changing_accounts() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(directory.path()).unwrap();
    let builtins = ProviderPreset::builtins();
    let mut replacement = builtins[0].clone();
    replacement.name = Id::new_v4().to_string();
    replacement.base_url = format!("http://127.0.0.1/{}/v1", Id::new_v4());
    let custom = ProviderPreset {
        id: Id::new_v4().to_string(),
        name: Id::new_v4().to_string(),
        group: Id::new_v4().to_string(),
        api: ProviderApi::OpenAi,
        base_url: "".into(),
        setup: Id::new_v4().to_string(),
    };
    let account = replacement.configure(replacement.base_url.clone());
    let mut settings = Settings::default();
    settings.providers.push(account.clone());
    settings.provider_presets.entries = vec![replacement.clone(), custom.clone()];
    settings
        .provider_presets
        .disabled
        .insert(builtins[1].id.clone());
    assert_eq!(save(&gateway, settings).await, Reply::Ok);
    let snapshot = gateway.snapshot().unwrap();
    let effective = snapshot.settings.provider_presets.effective();
    assert_eq!(
        effective.iter().find(|p| p.id == replacement.id),
        Some(&replacement)
    );
    assert!(effective.contains(&custom));
    assert!(!effective.iter().any(|p| p.id == builtins[1].id));
    assert!(snapshot.catalogs.is_empty());
    assert_eq!(snapshot.settings.providers, vec![account.clone()]);
    let config: Settings =
        toml::from_str(&std::fs::read_to_string(directory.path().join("settings.toml")).unwrap())
            .unwrap();
    assert_eq!(config.provider_presets, snapshot.settings.provider_presets);
    let mut settings = snapshot.settings;
    settings.provider_presets.include_builtins = false;
    assert_eq!(save(&gateway, settings).await, Reply::Ok);
    assert_eq!(
        gateway
            .snapshot()
            .unwrap()
            .settings
            .provider_presets
            .effective(),
        vec![replacement, custom]
    );
    drop(gateway);
    let restored = Gateway::open(directory.path()).unwrap().snapshot().unwrap();
    assert_eq!(restored.settings.providers, vec![account]);
    assert!(!restored.settings.provider_presets.include_builtins);
    assert_eq!(restored.settings.provider_presets.entries.len(), 2);
}
#[tokio::test]
async fn rejects_duplicate_and_credential_bearing_presets_before_persisting() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(directory.path()).unwrap();
    let mut settings = Settings::default();
    let mut preset = ProviderPreset::builtins()[0].clone();
    settings.provider_presets.entries = vec![preset.clone(), preset.clone()];
    assert!(matches!(
        save(&gateway, settings).await,
        Reply::Error { .. }
    ));
    preset.base_url = format!("https://example.invalid/v1?key={}", Id::new_v4());
    let mut settings = Settings::default();
    settings.provider_presets.entries.push(preset);
    assert!(matches!(
        save(&gateway, settings).await,
        Reply::Error { .. }
    ));
    assert_eq!(gateway.snapshot().unwrap().settings.revision, 0);
    assert!(!directory.path().join("settings.toml").exists());
}
