//! Main window: title bar, explorer | console and diagram tabs, status bar. Also turns
//! data-source events into notifications and the host-key trust and password prompts,
//! and runs the app-wide commands (palette, settings, tabs, theme, updates).

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, TitleBar, WindowExt as _,
    h_flex, v_flex,
};
use gpui_kit::*;
use savoia_core::ConnectionId;
use savoia_tunnel::HostKeyPolicy;

pub use crate::commands::NewConsole;
use crate::commands::{
    self, CheckForUpdates, CloseTab, CommandPalette, NextTab, OpenDocs, OpenSettings, PreviousTab,
    Quit, ReportIssue, SupportOnKofi, UseDarkAppearance, UseLightAppearance, UseSystemAppearance,
};
use crate::console::QueryConsole;
use crate::data_sources::{DataSources, DataSourcesEvent, SourceState};
use crate::data_view::{DataView, DataViewEvent};
use crate::diagram::ErDiagram;
use crate::explorer::{Explorer, ExplorerEvent, NodeRef};
use crate::memory::MemoryMeter;
use crate::settings::{self, prefs};
use crate::settings_view::SettingsView;
use crate::structure::StructureView;
use crate::theme::{self, Appearance};
use crate::transfer::{Direction, TransferView};
use crate::updates::{self, Check};
use crate::{crash, runtime};
use savoia_core::data_query::Filter;

/// A tab of the main area, in opening order.
enum Page {
    Console(Entity<QueryConsole>),
    Diagram(Entity<ErDiagram>),
    Structure(Entity<StructureView>),
    Data(Entity<DataView>),
    Transfer(Entity<TransferView>),
    Settings(Entity<SettingsView>),
}

pub struct Workspace {
    data_sources: Entity<DataSources>,
    explorer: Entity<Explorer>,
    pages: Vec<Page>,
    /// Index into `pages`; meaningless while it's empty.
    active: usize,
    memory: Entity<MemoryMeter>,
    /// Where palette commands are dispatched from when nothing inside the
    /// workspace has focus.
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let data_sources = crate::data_sources::init(cx);
        let mut this = Self::with_data_sources(data_sources, window, cx);
        this.dev_restore(window, cx);
        this.offer_crash_report(window, cx);
        if prefs(cx).check_updates {
            this.check_for_updates(false, window, cx);
        }
        this
    }

    /// For `scripts/dev.sh`, which restarts the app on every change:
    /// `SAVOIA_DEV_RECONNECT=1` connects the last-used data source, and
    /// `SAVOIA_DEV_OPEN=schema.table` then opens that table's data view (or,
    /// with `SAVOIA_DEV_TRANSFER=export|import`, the dump or import wizard on
    /// it; `schema` alone for the whole schema), so each restart lands where
    /// you were.
    fn dev_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("SAVOIA_DEV_RECONNECT").is_none() {
            return;
        }
        let Some(id) = self.data_sources.read(cx).most_recent() else {
            return;
        };
        self.data_sources
            .update(cx, |ds, cx| ds.connect(id, HostKeyPolicy::KnownOnly, cx));
        let Some(target) = std::env::var("SAVOIA_DEV_OPEN").ok() else {
            return;
        };
        let transfer = match std::env::var("SAVOIA_DEV_TRANSFER").as_deref() {
            Ok("export") => Some(Direction::Export),
            Ok("import") => Some(Direction::Import),
            _ => None,
        };
        let (schema, table) = match target.split_once('.') {
            Some((s, t)) => (s.to_owned(), Some(t.to_owned())),
            None if transfer.is_some() => (target.clone(), None),
            None => return,
        };
        let mut opened = false;
        let subscription =
            // DataSources announces a finished connect with an event, not a notify.
            cx.subscribe_in(&self.data_sources, window, move |this, ds, _: &DataSourcesEvent, window, cx| {
                let SourceState::Connected(session) = ds.read(cx).state(id) else {
                    return;
                };
                if std::mem::replace(&mut opened, true) {
                    return;
                }
                let catalog = session.catalog();
                let engine = ds.read(cx).get(id).map(|c| c.engine);
                let database = match engine {
                    Some(savoia_core::Engine::Mysql) => schema.clone(),
                    _ => catalog
                        .current()
                        .map(|d| d.name.clone())
                        .unwrap_or_default(),
                };
                let node = NodeRef {
                    connection: id,
                    database,
                    schema: schema.clone(),
                    table: table.clone(),
                };
                match transfer {
                    Some(direction) => {
                        this.open_transfer(node, direction, window, cx);
                    }
                    None => this.open_data(node, Vec::new(), window, cx),
                }
            });
        self._subscriptions.push(subscription);
    }

    pub fn with_data_sources(
        data_sources: Entity<DataSources>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        settings::init(&data_sources, cx);
        let explorer = cx.new(|cx| Explorer::new(data_sources.clone(), cx));
        let subscriptions = vec![
            // "Match system" keeps following the OS while the app is open.
            cx.observe_window_appearance(window, |_, _, cx| {
                if prefs(cx).appearance == Appearance::System {
                    theme::apply(Appearance::System, cx);
                }
            }),
            cx.observe(&explorer, |_, _, cx| cx.notify()),
            cx.observe(&data_sources, |_, _, cx| cx.notify()),
            cx.subscribe_in(&data_sources, window, Self::on_data_source_event),
            cx.subscribe_in(&explorer, window, Self::on_explorer_event),
            // A focused element that goes away (its tab closed, or hidden by
            // another) takes the focus along, and keys would reach nothing.
            cx.on_focus_lost(window, Self::focus_active),
        ];
        let mut this = Self {
            pages: Vec::new(),
            active: 0,
            memory: cx.new(MemoryMeter::new),
            focus: cx.focus_handle(),
            data_sources,
            explorer,
            _subscriptions: subscriptions,
        };
        // The first console follows the explorer's selection until it runs.
        this.open_console(None, window, cx);
        // Keys and menu commands go up from the focused element; with
        // nothing focused they would never reach the workspace.
        window.focus(&this.focus, cx);
        this
    }

    /// Opens a console tab on `source` and brings it forward.
    pub fn open_console(
        &mut self,
        source: Option<ConnectionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<QueryConsole> {
        let (data_sources, explorer) = (self.data_sources.clone(), self.explorer.clone());
        let console = cx.new(|cx| QueryConsole::new(data_sources, explorer, source, window, cx));
        cx.observe(&console, |_, _, cx| cx.notify()).detach();
        self.pages.push(Page::Console(console.clone()));
        self.active = self.pages.len() - 1;
        cx.notify();
        console
    }

    pub fn new_console(&mut self, _: &NewConsole, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.explorer.read(cx).selected_connection(cx);
        let console = self.open_console(source, window, cx);
        // Ready for typing, like a new tab anywhere else.
        console.update(cx, |console, cx| console.focus(window, cx));
    }

    /// Hands the keyboard to the active tab: its console's editor, else the
    /// workspace itself, which runs the app-wide keys.
    fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_console() {
            Some(console) => console.update(cx, |console, cx| console.focus(window, cx)),
            None => window.focus(&self.focus, cx),
        }
    }

    /// Closes a tab, cancelling its console's running query.
    pub fn close(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.pages.len() {
            return;
        }
        // Cancel now: the last frame may keep the view alive a little longer.
        match self.pages.remove(index) {
            Page::Console(console) => console.update(cx, |console, cx| console.cancel(cx)),
            Page::Transfer(view) => view.update(cx, |view, cx| view.cancel(cx)),
            _ => {}
        }
        if self.active > index || self.active == self.pages.len() {
            self.active = self.active.saturating_sub(1);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn explorer(&self) -> &Entity<Explorer> {
        &self.explorer
    }

    #[cfg(test)]
    pub fn consoles(&self) -> impl Iterator<Item = &Entity<QueryConsole>> {
        self.pages.iter().filter_map(|page| match page {
            Page::Console(console) => Some(console),
            _ => None,
        })
    }

    #[cfg(test)]
    pub fn data_views(&self) -> impl Iterator<Item = &Entity<DataView>> {
        self.pages.iter().filter_map(|page| match page {
            Page::Data(view) => Some(view),
            _ => None,
        })
    }

    #[cfg(test)]
    pub fn settings_tabs(&self) -> usize {
        self.pages
            .iter()
            .filter(|page| matches!(page, Page::Settings(_)))
            .count()
    }

    fn active_console(&self) -> Option<&Entity<QueryConsole>> {
        match self.pages.get(self.active) {
            Some(Page::Console(console)) => Some(console),
            _ => None,
        }
    }

    /// Where SQL for `source` goes: the active console if it runs there (or
    /// isn't bound yet), else the last console on `source`, else a new one.
    fn console_for(
        &mut self,
        source: ConnectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<QueryConsole> {
        let fits = |console: &Entity<QueryConsole>, cx: &App| {
            let console = console.read(cx);
            !console.is_bound() || console.source(cx) == Some(source)
        };
        if let Some(console) = self.active_console().filter(|c| fits(c, cx)) {
            return console.clone();
        }
        let found = self.pages.iter().rposition(|page| match page {
            Page::Console(console) => console.read(cx).source(cx) == Some(source),
            _ => false,
        });
        match found {
            Some(index) => {
                self.active = index;
                match &self.pages[index] {
                    Page::Console(console) => console.clone(),
                    _ => unreachable!(),
                }
            }
            None => self.open_console(Some(source), window, cx),
        }
    }

    /// Opens the diagram of `node`'s schema, or brings its tab forward.
    fn show_diagram(&mut self, node: NodeRef, cx: &mut Context<Self>) {
        let open = self.pages.iter().position(|page| match page {
            Page::Diagram(d) => d.read(cx).shows(&node),
            _ => false,
        });
        match open {
            Some(i) => {
                if let Page::Diagram(diagram) = &self.pages[i] {
                    diagram.update(cx, |d, cx| d.focus(node.table, cx));
                }
                self.active = i;
            }
            None => {
                let data_sources = self.data_sources.clone();
                let diagram = cx.new(|cx| ErDiagram::new(data_sources, node, cx));
                self.pages.push(Page::Diagram(diagram));
                self.active = self.pages.len() - 1;
            }
        }
        cx.notify();
    }

    /// Opens the structure of `node`'s table, or brings its tab forward.
    fn show_structure(&mut self, node: NodeRef, cx: &mut Context<Self>) {
        let open = self.pages.iter().position(|page| match page {
            Page::Structure(s) => s.read(cx).shows(&node),
            _ => false,
        });
        self.active = match open {
            Some(i) => i,
            None => {
                let data_sources = self.data_sources.clone();
                let view = cx.new(|cx| StructureView::new(data_sources, node, cx));
                self.pages.push(Page::Structure(view));
                self.pages.len() - 1
            }
        };
        cx.notify();
    }

    /// Opens the data view of `node`'s table, or brings its tab forward.
    /// With filters (related rows of another view) it always opens anew.
    pub fn open_data(
        &mut self,
        node: NodeRef,
        filters: Vec<Filter>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = self.pages.iter().position(|page| match page {
            Page::Data(view) => filters.is_empty() && view.read(cx).shows(&node),
            _ => false,
        });
        self.active = match open {
            Some(i) => i,
            None => {
                let data_sources = self.data_sources.clone();
                let view = cx.new(|cx| DataView::new(data_sources, node, filters, window, cx));
                cx.subscribe_in(&view, window, Self::on_data_view_event)
                    .detach();
                cx.observe(&view, |_, _, cx| cx.notify()).detach();
                self.pages.push(Page::Data(view));
                self.pages.len() - 1
            }
        };
        cx.notify();
    }

    /// Opens a dump or import wizard on `node`. Each opens anew: two
    /// exports of the same schema can differ in every option.
    pub fn open_transfer(
        &mut self,
        node: NodeRef,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TransferView> {
        let data_sources = self.data_sources.clone();
        let view = cx.new(|cx| TransferView::new(data_sources, node, direction, window, cx));
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        self.pages.push(Page::Transfer(view.clone()));
        self.active = self.pages.len() - 1;
        cx.notify();
        view
    }

    fn on_data_view_event(
        &mut self,
        _: &Entity<DataView>,
        event: &DataViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            DataViewEvent::Sql { connection, sql } => {
                let console = self.console_for(*connection, window, cx);
                console.update(cx, |c, cx| c.insert_sql(sql, false, window, cx));
            }
            DataViewEvent::Open { node, filters } => {
                self.open_data(node.clone(), filters.clone(), window, cx)
            }
        }
        cx.notify();
    }

    fn on_explorer_event(
        &mut self,
        _: &Entity<Explorer>,
        event: &ExplorerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ExplorerEvent::ShowDiagram(node) => self.show_diagram(node.clone(), cx),
            ExplorerEvent::ShowStructure(node) => self.show_structure(node.clone(), cx),
            ExplorerEvent::OpenData(node) => self.open_data(node.clone(), Vec::new(), window, cx),
            ExplorerEvent::Transfer(node, direction) => {
                self.open_transfer(node.clone(), *direction, window, cx);
            }
            ExplorerEvent::Sql {
                connection,
                sql,
                run,
                refresh,
            } => {
                // The SQL lands in a console on its data source; bring it forward.
                let console = self.console_for(*connection, window, cx);
                console.update(cx, |console, cx| console.insert_sql(sql, *run, window, cx));
                // Queued behind the statement: refreshes wait for the running query.
                if *refresh {
                    self.data_sources
                        .update(cx, |ds, cx| ds.refresh(*connection, cx));
                }
                cx.notify();
            }
        }
    }

    fn on_data_source_event(
        &mut self,
        data_sources: &Entity<DataSources>,
        event: &DataSourcesEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            DataSourcesEvent::Changed => cx.notify(),
            DataSourcesEvent::Error(message) => {
                window.push_notification(Notification::error(message.clone()).autohide(false), cx);
            }
            DataSourcesEvent::NeedsHostTrust {
                id,
                host,
                fingerprint,
            } => {
                let (id, data_sources) = (*id, data_sources.clone());
                let description = format!(
                    "The authenticity of {host} can't be established.\n{fingerprint}\n\n\
                     Trust it only if this matches the server's key. It will be added to ~/.ssh/known_hosts."
                );
                window.open_alert_dialog(cx, move |alert, _, cx| {
                    let data_sources = data_sources.clone();
                    alert
                        .icon(Icon::new(IconName::TriangleAlert).text_color(cx.theme().warning))
                        .title("Unknown SSH host")
                        .description(description.clone())
                        .show_cancel(true)
                        .ok_text("Trust and connect")
                        .on_ok(move |_, _, cx| {
                            data_sources.update(cx, |ds, cx| {
                                ds.connect(id, HostKeyPolicy::TrustUnknown, cx)
                            });
                            true
                        })
                });
            }
            DataSourcesEvent::NeedsPassword { id, error } => {
                self.ask_password(*id, error.clone(), data_sources.clone(), window, cx);
            }
        }
    }

    /// Opens the Settings tab, or brings it forward.
    pub fn open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .pages
            .iter()
            .position(|page| matches!(page, Page::Settings(_)));
        self.active = match open {
            Some(i) => i,
            None => {
                let data_sources = self.data_sources.clone();
                self.pages
                    .push(Page::Settings(cx.new(|_| SettingsView::new(data_sources))));
                self.pages.len() - 1
            }
        };
        cx.notify();
    }

    fn close_active(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Self>) {
        self.close(self.active, cx);
    }

    fn next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.pages.is_empty() {
            self.active = (self.active + 1) % self.pages.len();
            cx.notify();
        }
    }

    fn previous_tab(&mut self, _: &PreviousTab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.pages.is_empty() {
            self.active = (self.active + self.pages.len() - 1) % self.pages.len();
            cx.notify();
        }
    }

    /// Opens the command palette over the window. A confirmed command is
    /// dispatched from the workspace once the dialog closes: from the dialog
    /// itself it would never reach the workspace's handlers.
    pub fn open_palette(
        &mut self,
        _: &CommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = cx.new(|cx| CommandState::new(window, cx));
        let commands = commands::commands();
        let workspace_focus = self.focus.clone();
        let palette = state.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let actions: Vec<Box<dyn Action>> = commands.iter().map(|c| (c.action)()).collect();
            let items = commands.iter().map(|c| {
                CommandItem::new()
                    .label(c.label)
                    .icon(c.icon.clone())
                    .keywords(c.keywords.iter().copied())
                    .action((c.action)())
            });
            let focus = workspace_focus.clone();
            dialog.w(px(520.)).p_0().close_button(false).child(
                Command::new(&palette)
                    .items(items)
                    .bordered(false)
                    .placeholder("Type a command…")
                    .on_confirm(move |ix, window, cx| {
                        let Some(action) = actions.get(ix.row) else {
                            return;
                        };
                        let action = action.boxed_clone();
                        window.close_dialog(cx);
                        if window.focused(cx).is_none() {
                            window.focus(&focus, cx);
                        }
                        window.dispatch_action(action, cx);
                    })
                    .on_cancel(|window, cx| window.close_dialog(cx)),
            )
        });
        // After opening: the dialog takes focus for itself when it opens.
        state.update(cx, |state, cx| state.focus(window, cx));
    }

    fn use_appearance(appearance: Appearance, cx: &mut App) {
        settings::set_appearance(appearance, cx);
    }

    fn check_updates_action(
        &mut self,
        _: &CheckForUpdates,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.check_for_updates(true, window, cx);
    }

    /// Asks GitHub Releases for a newer version and offers to install it.
    /// A startup check (`manual` false) stays silent unless there is one.
    fn check_for_updates(&mut self, manual: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !updates::enabled() {
            if manual {
                window.push_notification(
                    Notification::info("This build can't update itself. Download new versions from GitHub Releases.")
                        .id::<UpdateNotice>(),
                    cx,
                );
            }
            return;
        }
        let check = runtime::spawn_blocking(updates::check);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = check.await else {
                return;
            };
            this.update_in(cx, |_, window, cx| match result {
                Ok(Check::Available(update)) => {
                    let version = update.version.clone();
                    let update = std::sync::Arc::new(*update);
                    window.push_notification(
                        Notification::info(format!("Savoia Studio {version} is available."))
                            .id::<UpdateNotice>()
                            .title("Update available")
                            .action(move |_, _, _| {
                                let update = update.clone();
                                Button::new("install-update")
                                    .small()
                                    .primary()
                                    .label("Install and restart")
                                    .on_click(move |_, window, cx| {
                                        install_update(update.clone(), window, cx)
                                    })
                            }),
                        cx,
                    );
                }
                Ok(Check::UpToDate) if manual => window.push_notification(
                    Notification::success(format!(
                        "Savoia Studio {} is the latest version.",
                        env!("CARGO_PKG_VERSION")
                    ))
                    .id::<UpdateNotice>(),
                    cx,
                ),
                Err(error) if manual => window.push_notification(
                    Notification::error(format!("Couldn't check for updates: {error}"))
                        .id::<UpdateNotice>(),
                    cx,
                ),
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// If the last run crashed and the user opted in, offers to open a
    /// prefilled GitHub issue.
    fn offer_crash_report(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(report) = crash::take_pending(prefs(cx).crash_reports) else {
            return;
        };
        let url = crash::issue_url(&report);
        window.push_notification(
            Notification::warning(
                "Savoia Studio closed unexpectedly last time. You can review the report on GitHub before sending it.",
            )
            .title("Crash report")
            .action(move |_, _, _| {
                let url = url.clone();
                Button::new("report-crash")
                    .small()
                    .label("Review report…")
                    .on_click(move |_, _, cx| cx.open_url(&url))
            }),
            cx,
        );
    }

    /// Asks for the database password of `id`, then connects with it.
    fn ask_password(
        &mut self,
        id: ConnectionId,
        error: Option<String>,
        data_sources: Entity<DataSources>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(config) = data_sources.read(cx).get(id).cloned() else {
            return;
        };
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Password")
        });
        password.update(cx, |input, cx| input.focus(window, cx));
        let target = format!("{}@{}:{}", config.user, config.host, config.port);
        let note = if config.save_password {
            "It is saved once the connection works."
        } else {
            "It is kept until Savoia Studio quits."
        };
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let theme = cx.theme();
            let (data_sources, input) = (data_sources.clone(), password.clone());
            alert
                .icon(Icon::new(Lucide::KeyRound).text_color(theme.muted_foreground))
                .title(format!("Password for {}", config.display_name()))
                .description(
                    v_flex()
                        .gap_3()
                        .children(
                            error
                                .clone()
                                .map(|e| div().text_color(theme.danger).child(e)),
                        )
                        .child(format!("Connecting as {target}. {note}"))
                        .child(Input::new(&password).mask_toggle()),
                )
                .show_cancel(true)
                .ok_text("Connect")
                .on_ok(move |_, _, cx| {
                    let value = input.read(cx).value().to_string();
                    data_sources.update(cx, |ds, cx| ds.connect_with_password(id, value, cx));
                    true
                })
        });
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;

        let ds = self.data_sources.read(cx);
        let selected = self
            .explorer
            .read(cx)
            .selected_connection(cx)
            .and_then(|id| ds.get(id).cloned());
        let source_name = selected.as_ref().map(|c| c.display_name());
        let (status_icon, status_text) = match selected.as_ref().map(|c| (c, ds.state(c.id))) {
            None => (Lucide::Plug, "No data source selected".to_string()),
            Some((c, SourceState::Disconnected)) => (
                Lucide::Unplug,
                format!("{} · disconnected", c.display_name()),
            ),
            Some((c, SourceState::Connecting)) => {
                (Lucide::Plug, format!("{} · connecting…", c.display_name()))
            }
            Some((c, SourceState::Failed(err))) => {
                (Lucide::Unplug, format!("{} · {err}", c.display_name()))
            }
            Some((c, SourceState::Connected(session))) => (
                Lucide::Plug,
                format!(
                    "{} · {}{}",
                    c.display_name(),
                    session.catalog().server.version,
                    if c.read_only { " · read-only" } else { "" }
                ),
            ),
        };

        let title_bar = TitleBar::new().child(
            h_flex()
                .gap_2()
                .text_sm()
                .child(
                    // A database glyph on green, not the app icon's shield:
                    // its red and blue would break the Run-only blue rule.
                    h_flex()
                        .size(px(16.))
                        .rounded(px(4.))
                        .justify_center()
                        .bg(theme::c(theme::IVREA_GREEN))
                        .child(
                            Icon::new(Lucide::Database)
                                .size(px(11.))
                                .text_color(theme::c(theme::BAND_INK)),
                        ),
                )
                .child(div().font_semibold().child("Savoia Studio"))
                .children(
                    source_name
                        .clone()
                        .map(|name| div().text_color(muted).child(name)),
                ),
        );

        // The connection color carries onto console tabs, like Beekeeper's
        // colored connections, so prod vs local is visible at a glance.
        let mut seen: Vec<Option<ConnectionId>> = Vec::new();
        let tabs: Vec<Tab> = self
            .pages
            .iter()
            .enumerate()
            .map(|(i, page)| {
                let tab = match page {
                    Page::Console(console) => {
                        let source = console.read(cx).source(cx);
                        seen.push(source);
                        let nth = seen.iter().filter(|s| **s == source).count();
                        let config = source.and_then(|id| ds.get(id));
                        let color = config
                            .and_then(|c| c.color)
                            .map_or(muted, |c| rgb(c.rgb()).into());
                        let mut label = "console".to_string();
                        if nth > 1 {
                            label.push_str(&format!(" {nth}"));
                        }
                        if let Some(config) = config {
                            label.push_str(&format!(" [{}]", config.display_name()));
                        }
                        Tab::new()
                            .label(label)
                            .prefix(Icon::new(Lucide::Database).small().ml_2().text_color(color))
                    }
                    Page::Diagram(diagram) => Tab::new()
                        .label(diagram.read(cx).title())
                        .prefix(Icon::new(Lucide::Workflow).small().ml_2().text_color(muted)),
                    Page::Data(view) => {
                        let source = view.read(cx).connection();
                        let color = ds
                            .get(source)
                            .and_then(|c| c.color)
                            .map_or(muted, |c| rgb(c.rgb()).into());
                        Tab::new()
                            .label(view.read(cx).title())
                            .prefix(Icon::new(Lucide::Table).small().ml_2().text_color(color))
                    }
                    Page::Transfer(view) => {
                        let view = view.read(cx);
                        let color = ds
                            .get(view.connection())
                            .and_then(|c| c.color)
                            .map_or(muted, |c| rgb(c.rgb()).into());
                        let icon = if view.is_running() {
                            Lucide::LoaderCircle
                        } else {
                            match view.direction() {
                                Direction::Export => Lucide::Download,
                                Direction::Import => Lucide::Upload,
                            }
                        };
                        Tab::new()
                            .label(view.title())
                            .prefix(Icon::new(icon).small().ml_2().text_color(color))
                    }
                    Page::Settings(_) => Tab::new()
                        .label("Settings")
                        .prefix(Icon::new(Lucide::Settings).small().ml_2().text_color(muted)),
                    Page::Structure(view) => Tab::new().label(view.read(cx).title()).prefix(
                        Icon::new(Lucide::TableProperties)
                            .small()
                            .ml_2()
                            .text_color(muted),
                    ),
                };
                tab.suffix(
                    Button::new(("close-tab", i))
                        .ghost()
                        .xsmall()
                        .mr_1()
                        .icon(Icon::new(IconName::Close))
                        .tooltip("Close")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close(i, cx);
                        })),
                )
            })
            .collect();
        let workspace = cx.entity().downgrade();
        let content = match self.pages.get(self.active) {
            Some(Page::Console(console)) => console.clone().into_any_element(),
            Some(Page::Diagram(diagram)) => diagram.clone().into_any_element(),
            Some(Page::Structure(view)) => view.clone().into_any_element(),
            Some(Page::Data(view)) => view.clone().into_any_element(),
            Some(Page::Transfer(view)) => view.clone().into_any_element(),
            Some(Page::Settings(view)) => view.clone().into_any_element(),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(muted)
                .child("No console open.")
                .child(
                    Button::new("empty-new-console")
                        .small()
                        .icon(Icon::new(Lucide::SquareTerminal))
                        .label(format!(
                            "New console ({})",
                            commands::keys_label(commands::NEW_CONSOLE_KEYS)
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_console(&NewConsole, window, cx)
                        })),
                )
                .into_any_element(),
        };

        let main = v_flex()
            .size_full()
            .child(
                TabBar::new("pages")
                    .underline()
                    .children(tabs)
                    .selected_index(self.active)
                    .suffix(
                        Button::new("new-console")
                            .ghost()
                            .xsmall()
                            .mx_1()
                            .icon(Icon::new(IconName::Plus))
                            .tooltip_with_action("New console", &NewConsole, None)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_console(&NewConsole, window, cx)
                            })),
                    )
                    .on_click(move |ix, _, cx| {
                        let ix = *ix;
                        workspace
                            .update(cx, |this, cx| {
                                this.active = ix;
                                cx.notify();
                            })
                            .ok();
                    }),
            )
            .child(div().flex_1().min_h_0().child(content));

        let status = StatusBar::new()
            .left(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .child(Icon::new(status_icon).xsmall())
                    .child(status_text),
            )
            .right(
                h_flex()
                    .gap_3()
                    .child(self.memory.clone())
                    .child(div().text_xs().text_color(muted).child("SQL · UTF-8"))
                    .child(
                        Button::new("kofi")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(Lucide::Coffee))
                            .label("Support me on Ko-fi")
                            .tooltip("Savoia Studio is free. Buy the author a coffee.")
                            .on_click(|_, _, cx| cx.open_url(commands::KOFI_URL)),
                    ),
            );

        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::new_console))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::close_active))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::check_updates_action))
            .on_action(|_: &UseSystemAppearance, _, cx| {
                Self::use_appearance(Appearance::System, cx)
            })
            .on_action(|_: &UseLightAppearance, _, cx| Self::use_appearance(Appearance::Light, cx))
            .on_action(|_: &UseDarkAppearance, _, cx| Self::use_appearance(Appearance::Dark, cx))
            .on_action(|_: &OpenDocs, _, cx| cx.open_url(commands::DOCS_URL))
            .on_action(|_: &ReportIssue, _, cx| cx.open_url(crash::ISSUES_URL))
            .on_action(|_: &SupportOnKofi, _, cx| cx.open_url(commands::KOFI_URL))
            .on_action(|_: &Quit, _, cx| cx.quit())
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(title_bar)
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("workspace")
                        .child(
                            resizable_panel()
                                .size(px(320.))
                                .size_range(px(220.)..px(560.))
                                .child(self.explorer.clone()),
                        )
                        .child(resizable_panel().child(main)),
                ),
            )
            .child(status)
    }
}

/// Keys the update notifications, so each step replaces the one before.
struct UpdateNotice;

/// Downloads and installs on the I/O runtime, then restarts into the new
/// version. Failures land in a notification; the running app is untouched.
fn install_update(
    update: std::sync::Arc<cargo_packager_updater::Update>,
    window: &mut Window,
    cx: &mut App,
) {
    window.push_notification(
        Notification::info("Downloading the update…")
            .id::<UpdateNotice>()
            .autohide(false),
        cx,
    );
    let install = runtime::spawn_blocking(move || updates::install(&update));
    window
        .spawn(cx, async move |cx| {
            let result = install.await.map_err(|e| e.to_string()).and_then(|r| r);
            cx.update(|window, cx| match result {
                Ok(()) => cx.restart(),
                Err(error) => window.push_notification(
                    Notification::error(format!("The update failed: {error}"))
                        .id::<UpdateNotice>()
                        .autohide(false),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
}
