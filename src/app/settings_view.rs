//! Settings: infrequently changed options, grouped into short panes.

use dioxus::prelude::*;

use crate::app::{components, use_ui};
use crate::app::backend::Command;
use crate::config::CollisionPolicy;

#[component]
pub fn Settings() -> Element {
    let mut ui = use_ui();

    let (destination, max_simultaneous, retry_count, retry_delay, timeout, buffer_size, bandwidth, user_agent, extensions, ad_hosts) = {
        let model = ui.model.read();
        (
            model.set_destination.clone(),
            model.set_max_simultaneous.clone(),
            model.set_retry_count.clone(),
            model.set_retry_delay.clone(),
            model.set_timeout.clone(),
            model.set_buffer_size.clone(),
            model.set_bandwidth.clone(),
            model.set_user_agent.clone(),
            model.set_extensions.clone(),
            model.set_ad_hosts.clone(),
        )
    };

    let (include_unknown, ad_filter, respect_rate, private_addresses, collision, dirty, error, startup_error) = {
        let model = ui.model.read();
        let draft = &model.settings_state.draft;
        (
            draft.include_unknown_types,
            draft.ad_filter_enabled,
            draft.respect_rate_limit,
            draft.allow_private_addresses,
            draft.filename_collision,
            model.settings_state.is_dirty(),
            model.settings_state.error.clone(),
            model.settings_error.clone(),
        )
    };

    let policies = [
        CollisionPolicy::KeepExisting,
        CollisionPolicy::Rename,
        CollisionPolicy::Overwrite,
        CollisionPolicy::Skip,
    ];

    rsx! {
        components::PageHeader {
            title: "Settings".to_string(),
            subtitle: "Options that apply to every download. Appearance follows the system.".to_string(),
        }
        components::Card {
            components::SectionTitle { text: "Downloads".to_string(), tone: "".to_string() }
            div { class: "field wide",
                label { "Default destination" }
                input {
                    value: "{destination}",
                    oninput: move |event| ui.model.write().set_destination = event.value(),
                }
            }
            div { class: "row",
                div { class: "field small",
                    label { "Max simultaneous" }
                    input {
                        value: "{max_simultaneous}",
                        oninput: move |event| ui.model.write().set_max_simultaneous = event.value(),
                    }
                }
                div { class: "field small",
                    label { "Retry count" }
                    input {
                        value: "{retry_count}",
                        oninput: move |event| ui.model.write().set_retry_count = event.value(),
                    }
                }
                div { class: "field small",
                    label { "Retry delay (s)" }
                    input {
                        value: "{retry_delay}",
                        oninput: move |event| ui.model.write().set_retry_delay = event.value(),
                    }
                }
            }
        }
        components::Card {
            components::SectionTitle { text: "Network & performance".to_string(), tone: "".to_string() }
            div { class: "row",
                div { class: "field small",
                    label { "Timeout (s)" }
                    input {
                        value: "{timeout}",
                        oninput: move |event| ui.model.write().set_timeout = event.value(),
                    }
                }
                div { class: "field medium",
                    label { "Buffer (bytes)" }
                    input {
                        value: "{buffer_size}",
                        oninput: move |event| ui.model.write().set_buffer_size = event.value(),
                    }
                }
                div { class: "field medium",
                    label { "Bandwidth (KB/s)" }
                    input {
                        value: "{bandwidth}",
                        oninput: move |event| ui.model.write().set_bandwidth = event.value(),
                    }
                }
            }
            div { class: "field wide",
                label { "User agent" }
                input {
                    value: "{user_agent}",
                    oninput: move |event| ui.model.write().set_user_agent = event.value(),
                }
            }
        }
        components::Card {
            components::SectionTitle { text: "Discovery & filtering".to_string(), tone: "".to_string() }
            div { class: "field wide",
                label { "Allowed extensions" }
                input {
                    value: "{extensions}",
                    oninput: move |event| ui.model.write().set_extensions = event.value(),
                }
            }
            div { class: "field wide",
                label { "Custom ad hosts file" }
                input {
                    value: "{ad_hosts}",
                    oninput: move |event| ui.model.write().set_ad_hosts = event.value(),
                }
            }
            div { class: "row",
                label { class: "checkbox",
                    input {
                        r#type: "checkbox",
                        checked: include_unknown,
                        onchange: move |event| ui.model.write().settings_state.draft.include_unknown_types = event.checked(),
                    }
                    "Include unknown types"
                }
                label { class: "checkbox",
                    input {
                        r#type: "checkbox",
                        checked: ad_filter,
                        onchange: move |event| ui.model.write().settings_state.draft.ad_filter_enabled = event.checked(),
                    }
                    "Ad & tracker filter"
                }
                label { class: "checkbox",
                    input {
                        r#type: "checkbox",
                        checked: respect_rate,
                        onchange: move |event| ui.model.write().settings_state.draft.respect_rate_limit = event.checked(),
                    }
                    "Respect rate limits"
                }
                label { class: "checkbox",
                    input {
                        r#type: "checkbox",
                        checked: private_addresses,
                        onchange: move |event| ui.model.write().settings_state.draft.allow_private_addresses = event.checked(),
                    }
                    "Allow private addresses"
                }
            }
        }
        components::Card {
            components::SectionTitle { text: "File name collisions".to_string(), tone: "".to_string() }
            div { class: "row",
                for policy in policies {
                    button {
                        class: if policy == collision { "active" } else { "ghost" },
                        onclick: move |_| ui.model.write().settings_state.draft.filename_collision = policy,
                        "{policy.as_str()}"
                    }
                }
            }
        }
        if let Some(error) = error {
            components::Muted { text: error, tone: "danger".to_string() }
        }
        if let Some(error) = startup_error {
            components::Muted { text: error, tone: "warning".to_string() }
        }
        div { class: "row",
            button {
                class: "primary",
                onclick: move |_| save(ui),
                "Save settings"
            }
            button {
                class: "ghost",
                onclick: move |_| ui.model.write().sync_settings_inputs(),
                "Reload"
            }
            components::Muted {
                text: (if dirty { "Unsaved changes" } else { "All changes saved" }).to_string(),
                tone: "".to_string(),
            }
        }
    }
}

fn save(mut ui: crate::app::Ui) {
    let mut model = ui.model.write();
    let values = (
        model.set_max_simultaneous.clone(),
        model.set_timeout.clone(),
        model.set_retry_count.clone(),
        model.set_retry_delay.clone(),
        model.set_buffer_size.clone(),
        model.set_bandwidth.clone(),
        model.set_user_agent.clone(),
        model.set_extensions.clone(),
        model.set_destination.clone(),
        model.set_ad_hosts.clone(),
    );
    let result = {
        let state = &mut model.settings_state;
        state.max_simultaneous_text = values.0;
        state.connection_timeout_text = values.1;
        state.retry_count_text = values.2;
        state.retry_delay_text = values.3;
        state.buffer_size_text = values.4;
        state.bandwidth_kbps_text = values.5;
        state.user_agent_text = values.6;
        state.allowed_extensions_text = values.7;
        state.draft.default_destination_dir = values.8;
        state.draft.ad_filter_list_path = values.9;
        state.validated_settings()
    };
    match result {
        Ok(settings) => {
            model.settings_state.set_error(None);
            model.send(Command::SaveSettings { settings });
        }
        Err(error) => model.settings_state.set_error(Some(error)),
    }
}
