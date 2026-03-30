use crate::TitleBar;
use gpui::{App, Context, Entity, EventEmitter, IntoElement, Render, Subscription, Window};
use workspace::{HideStatusItem, StatusItemView, ToolbarItemEvent, item::ItemHandle};

pub struct WorkspaceInfo {
    title_bar: Entity<TitleBar>,
    _subscription: Subscription,
}

impl WorkspaceInfo {
    pub fn new(title_bar: Entity<TitleBar>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&title_bar, |_, _, cx| cx.notify());
        Self {
            title_bar,
            _subscription: subscription,
        }
    }
}

impl Render for WorkspaceInfo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.title_bar.update(cx, |title_bar, cx| {
            title_bar.render_status_bar_workspace_info(window, cx)
        })
    }
}

impl EventEmitter<ToolbarItemEvent> for WorkspaceInfo {}

impl StatusItemView for WorkspaceInfo {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

pub struct UserMenuButton {
    title_bar: Entity<TitleBar>,
    _subscription: Subscription,
}

impl UserMenuButton {
    pub fn new(title_bar: Entity<TitleBar>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&title_bar, |_, _, cx| cx.notify());
        Self {
            title_bar,
            _subscription: subscription,
        }
    }
}

impl Render for UserMenuButton {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.title_bar.update(cx, |title_bar, cx| {
            title_bar.render_status_bar_controls(window, cx)
        })
    }
}

impl EventEmitter<ToolbarItemEvent> for UserMenuButton {}

impl StatusItemView for UserMenuButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
