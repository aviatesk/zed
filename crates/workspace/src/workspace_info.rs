use std::time::Duration;

use chrono::Local;
use gpui::{Context, EventEmitter, IntoElement, Render, Task, Window};
use settings::Settings as _;
use ui::{Label, LabelSize, Tooltip, prelude::*};
use util::ResultExt;

use crate::{HideStatusItem, StatusBarSettings, StatusItemView, item::ItemHandle};

pub struct StatusBarClock {
    _tick: Task<()>,
}

impl StatusBarClock {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let tick = cx.spawn(async move |this, cx| {
            loop {
                let seconds_in_minute = Local::now()
                    .format("%S")
                    .to_string()
                    .parse::<u64>()
                    .unwrap_or(0);
                cx.background_executor()
                    .timer(Duration::from_secs(60 - seconds_in_minute))
                    .await;
                this.update(cx, |_, cx| cx.notify()).log_err();
            }
        });

        Self { _tick: tick }
    }
}

impl Render for StatusBarClock {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !StatusBarSettings::get_global(cx).clock_button {
            return div().hidden().into_any_element();
        }
        let formatted = Local::now().format("%a %b %-d %H:%M").to_string();
        div()
            .id("status-bar-clock")
            .tooltip(|_, cx| {
                Tooltip::simple(Local::now().format("%a %b %-d %H:%M:%S").to_string(), cx)
            })
            .child(
                Label::new(formatted)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }
}

impl EventEmitter<crate::ToolbarItemEvent> for StatusBarClock {}

impl StatusItemView for StatusBarClock {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings.status_bar.get_or_insert_default().clock_button = Some(false);
        }))
    }
}
