//! Settings: infrequently changed options, grouped into short panes.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::IconName;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{App, components, theme};
use crate::app::backend::Command;
use crate::config::CollisionPolicy;

pub fn show(app: &App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let palette = theme::palette(window, cx);

    let field = |label: &'static str, input: Entity<InputState>, width: Pixels| {
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .w(width)
            .child(components::muted(label, palette.text_muted))
            .child(Input::new(&input))
    };

    let downloads = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(components::section_title("Downloads", palette.text))
        .child(field(
            "Default destination",
            app.set_destination.clone(),
            px(420.0),
        ))
        .child(
            div()
                .flex()
                .gap(px(10.0))
                .child(field("Max simultaneous", app.set_max_simultaneous.clone(), px(110.0)))
                .child(field("Retry count", app.set_retry_count.clone(), px(110.0)))
                .child(field("Retry delay (s)", app.set_retry_delay.clone(), px(110.0))),
        );

    let network = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(components::section_title("Network & performance", palette.text))
        .child(
            div()
                .flex()
                .gap(px(10.0))
                .child(field("Timeout (s)", app.set_timeout.clone(), px(110.0)))
                .child(field("Buffer (bytes)", app.set_buffer_size.clone(), px(130.0)))
                .child(field("Bandwidth (KB/s)", app.set_bandwidth.clone(), px(130.0))),
        )
        .child(field("User agent", app.set_user_agent.clone(), px(420.0)));

    let discovery = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(components::section_title("Discovery & filtering", palette.text))
        .child(field("Allowed extensions", app.set_extensions.clone(), px(420.0)))
        .child(field("Custom ad hosts file", app.set_ad_hosts.clone(), px(420.0)))
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(12.0))
                .child(
                    Checkbox::new("set-include-unknown")
                        .label("Include unknown types")
                        .checked(app.settings_state.draft.include_unknown_types)
                        .on_change(cx.listener(|this, value, _, cx| {
                            this.settings_state.draft.include_unknown_types = *value;
                            cx.notify();
                        })),
                )
                .child(
                    Checkbox::new("set-ad-filter")
                        .label("Ad & tracker filter")
                        .checked(app.settings_state.draft.ad_filter_enabled)
                        .on_change(cx.listener(|this, value, _, cx| {
                            this.settings_state.draft.ad_filter_enabled = *value;
                            cx.notify();
                        })),
                )
                .child(
                    Checkbox::new("set-respect-rate")
                        .label("Respect rate limits")
                        .checked(app.settings_state.draft.respect_rate_limit)
                        .on_change(cx.listener(|this, value, _, cx| {
                            this.settings_state.draft.respect_rate_limit = *value;
                            cx.notify();
                        })),
                )
                .child(
                    Checkbox::new("set-private")
                        .label("Allow private addresses")
                        .checked(app.settings_state.draft.allow_private_addresses)
                        .on_change(cx.listener(|this, value, _, cx| {
                            this.settings_state.draft.allow_private_addresses = *value;
                            cx.notify();
                        })),
                ),
        );

    let collision = app.settings_state.draft.filename_collision;
    let collision_picker = div()
        .flex()
        .flex_wrap()
        .gap(px(6.0))
        .children(
            [
                CollisionPolicy::KeepExisting,
                CollisionPolicy::Rename,
                CollisionPolicy::Overwrite,
                CollisionPolicy::Skip,
            ]
            .into_iter()
            .map(|policy| {
                let label = policy.as_str();
                let active = collision == policy;
                Button::new(format!("collision-{label}"))
                    .label(label)
                    .when(active, |button| button.primary())
                    .when(!active, |button| button.ghost())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_state.draft.filename_collision = policy;
                        cx.notify();
                    }))
            }),
        );

    let mut column = div()
        .flex()
        .flex_col()
        .gap(px(16.0))
        .child(components::page_header(
            "Settings",
            "Options that apply to every download. Appearance follows the system.",
        ))
        .child(components::card(downloads, palette))
        .child(components::card(network, palette))
        .child(components::card(discovery, palette))
        .child(
            components::card(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(components::section_title("File name collisions", palette.text))
                    .child(collision_picker),
                palette,
            ),
        );

    if let Some(error) = &app.settings_state.error {
        column = column.child(components::muted(error.clone(), palette.danger));
    }
    if let Some(error) = &app.settings_error {
        column = column.child(components::muted(error.clone(), palette.warning));
    }

    column.child(
        div()
            .flex()
            .gap(px(8.0))
            .child(
                Button::new("settings-save")
                    .primary()
                    .icon(IconName::Check)
                    .label("Save settings")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let read = |entity: &Entity<InputState>| entity.read(cx).value().to_string();
                        this.settings_state.max_simultaneous_text =
                            read(&this.set_max_simultaneous);
                        this.settings_state.connection_timeout_text = read(&this.set_timeout);
                        this.settings_state.retry_count_text = read(&this.set_retry_count);
                        this.settings_state.retry_delay_text = read(&this.set_retry_delay);
                        this.settings_state.buffer_size_text = read(&this.set_buffer_size);
                        this.settings_state.bandwidth_kbps_text = read(&this.set_bandwidth);
                        this.settings_state.user_agent_text = read(&this.set_user_agent);
                        this.settings_state.allowed_extensions_text = read(&this.set_extensions);
                        this.settings_state.draft.default_destination_dir = read(&this.set_destination);
                        this.settings_state.draft.ad_filter_list_path = read(&this.set_ad_hosts);
                        match this.settings_state.validated_settings() {
                            Ok(settings) => {
                                this.settings_state.set_error(None);
                                this.send(Command::SaveSettings { settings });
                            }
                            Err(error) => this.settings_state.set_error(Some(error)),
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("settings-reload")
                    .ghost()
                    .label("Reload")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.sync_settings_inputs(window, cx);
                        cx.notify();
                    })),
            )
            .child(
                components::muted(
                    if app.settings_state.is_dirty() {
                        "Unsaved changes"
                    } else {
                        "All changes saved"
                    },
                    palette.text_muted,
                ),
            ),
    )
}
