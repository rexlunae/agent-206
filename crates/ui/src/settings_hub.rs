//! Settings navigation shared by the hub, sidebar ancestry and command palette.
use crate::{PageFrame, State, route::Route};
use fc_dioxus::prelude::*;

pub(crate) struct SettingsSection {
    id: &'static str,
    title: &'static str,
    pub(crate) items: Vec<SettingsItem>,
}

pub(crate) struct SettingsItem {
    pub(crate) route: Route,
    description: &'static str,
}

pub(crate) fn sections() -> [SettingsSection; 3] {
    use Route::*;
    let item = |route, description| SettingsItem { route, description };
    [
        SettingsSection {
            id: "settings-connections",
            title: "Connections & services",
            items: vec![
                item(
                    Providers {},
                    "Model providers, available models, web search and coding-agent backends.",
                ),
                item(
                    Messengers {},
                    "Messaging accounts, delivery and channel bindings.",
                ),
                item(Joshua {}, "Manage local and remote model services."),
                item(
                    Gateway {},
                    "Gateway connection, process controls and service health.",
                ),
            ],
        },
        SettingsSection {
            id: "settings-preferences",
            title: "Agents & appearance",
            items: vec![
                item(Prompts {}, "Edit mode prompts and delegated-work limits."),
                item(Appearance {}, "Theme, palette, color scheme and contrast."),
            ],
        },
        SettingsSection {
            id: "settings-security",
            title: "Security & setup",
            items: vec![
                item(
                    Security {},
                    "Vault authentication, credential policies and gateway access.",
                ),
                item(
                    Imports {},
                    "Preview settings and credential migrations from other tools.",
                ),
                item(
                    Welcome {},
                    "Review the setup checklist and remaining steps.",
                ),
            ],
        },
    ]
}

#[component]
pub(crate) fn Settings() -> Element {
    let state = use_context::<State>();
    rsx! { PageFrame { icon: IconName::Settings, kicker: "Workspace", title: "Settings",
        description: "Manage your connections, preferences and security.",
        for section in sections() {
            section { key: "{section.id}", class: "a206-stack", aria_labelledby: section.id,
                h2 { id: section.id, "{section.title}" }
                div { class: "a206-grid",
                    for item in section.items {
                        Card { key: "{item.route}", CardBody { div { class: "a206-stack",
                            if item.route == (Route::Appearance {}) {
                                Button { variant: ButtonVariant::Ghost, aria_haspopup: "dialog",
                                    onclick: move |_| { let mut open = state.appearance; open.set(true); },
                                    Icon { name: item.route.icon(), size: IconSize::Sm } "{item.route.label()}"
                                }
                            } else {
                                LinkButton { variant: ButtonVariant::Ghost, href: item.route.to_string(),
                                    Icon { name: item.route.icon(), size: IconSize::Sm } "{item.route.label()}"
                                    Icon { name: IconName::ArrowEnd, size: IconSize::Sm }
                                }
                            }
                            p { class: "a206-muted", "{item.description}" }
                        } } }
                    }
                }
            }
        }
    } }
}
