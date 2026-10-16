//! Query console: toolbar, SQL editor and the output/result panes below it.
//!
//! Run sends the selection, else the statement at the caret, to the
//! console's data source; Run script sends the whole editor. The data
//! source is the one the console was opened on, else the one selected in the explorer
//! at its first run. A reader task moves the query's events into the grid, pausing
//! while the grid has enough rows (see [`Pacer`]).

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{resizable_panel, v_resizable};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::path::PathBuf;

use savoia_core::export::Format;
use savoia_core::{AppError, ConnectionId, Engine, QueryEvent, split};
use savoia_store::HistoryEntry;

use crate::commands::keys_label;
use crate::completion::SqlCompletion;
use crate::data_sources::{DataSources, SourceState};
use crate::explorer::Explorer;
use crate::history::HistoryPanel;
use crate::results::{Next, Pacer, ResultSet};
use crate::session::{self, Session};
use crate::theme::BandDisabled as _;
use crate::{runtime, theme};

actions!(console, [RunQuery, RunScript]);

const CONTEXT: &str = "QueryConsole";

pub fn init(cx: &mut App) {
    // Bound inside the editor too, where ⌘↩ would otherwise be its own Enter.
    cx.bind_keys([
        KeyBinding::new("secondary-enter", RunQuery, Some(CONTEXT)),
        KeyBinding::new(
            "secondary-enter",
            RunQuery,
            Some(&format!("{CONTEXT} > Input")),
        ),
        KeyBinding::new("secondary-shift-enter", RunScript, Some(CONTEXT)),
        KeyBinding::new(
            "secondary-shift-enter",
            RunScript,
            Some(&format!("{CONTEXT} > Input")),
        ),
    ]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Output,
    /// A result set of the last run, by position.
    Result(usize),
}

/// One result set of the last run: its grid, and its own rows and time.
struct ResultTab {
    table: Entity<TableState<ResultSet>>,
    summary: SharedString,
    _scrolled: Subscription,
}

struct OutputLine {
    text: SharedString,
    error: bool,
}

/// The query in flight. Dropping it drops the reader task and with it the
/// `RunningQuery`, which cancels and drains in the background.
struct Running {
    started: Instant,
    ran_at: SystemTime,
    source: ConnectionId,
    sql: String,
    /// Rows returned and affected so far, for the history.
    rows: u64,
    error: Option<String>,
    _reader: Task<()>,
}

pub struct QueryConsole {
    data_sources: Entity<DataSources>,
    explorer: Entity<Explorer>,
    /// The data source this console runs on. `None` until the first run
    /// when opened with nothing selected.
    source: Option<ConnectionId>,
    editor: Entity<EditorState>,
    results: Vec<ResultTab>,
    output: Vec<OutputLine>,
    pane: Pane,
    running: Option<Running>,
    /// Results and time of the last run, for the Output pane's header.
    summary: Option<SharedString>,
    /// The engine of the last run, for copying results as SQL.
    engine: Engine,
    /// Quick filter over the rows of every result of the last run.
    filter: Entity<InputState>,
    /// A CSV export waiting for its result to finish loading.
    pending_export: Option<(usize, PathBuf)>,
}

impl QueryConsole {
    pub fn new(
        data_sources: Entity<DataSources>,
        explorer: Entity<Explorer>,
        source: Option<ConnectionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| EditorState::new(window, cx).language("sql"));
        let completion = SqlCompletion {
            data_sources: data_sources.clone(),
            console: cx.entity().downgrade(),
        };
        editor.update(cx, |editor, _| {
            editor.lsp_mut().completion_provider = Some(Rc::new(completion));
        });
        cx.observe(&explorer, |_, _, cx| cx.notify()).detach();
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter rows"));
        cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.apply_filter(cx);
            }
        })
        .detach();
        Self {
            data_sources,
            explorer,
            source,
            editor,
            results: Vec::new(),
            output: Vec::new(),
            pane: Pane::Output,
            running: None,
            summary: None,
            engine: Engine::Postgres,
            filter,
            pending_export: None,
        }
    }

    pub(crate) fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let text = self.filter.read(cx).value().to_string();
        for tab in &self.results {
            tab.table.update(cx, |table, cx| {
                table.delegate_mut().set_filter(&text);
                table.refresh(cx);
            });
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn filter(&self) -> &Entity<InputState> {
        &self.filter
    }

    /// Copies the shown rows of result `ix` as TSV.
    pub(crate) fn copy_result(&self, ix: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.results.get(ix) {
            let text = tab.table.read(cx).delegate().export(Format::Tsv, None);
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Asks where to save result `ix` as CSV, loading the rest of it first
    /// if it is still streaming.
    fn export_result(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let directory = std::env::home_dir().unwrap_or_default();
        let path = cx.prompt_for_new_path(&directory, Some("result.csv"));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            this.update(cx, |this, cx| {
                let Some(tab) = this.results.get(ix) else {
                    return;
                };
                match tab.table.read(cx).delegate().pacer().cloned() {
                    Some(pacer) => {
                        pacer.load_all();
                        this.pending_export = Some((ix, path));
                        this.log("Loading the rest of the result to export it…".into(), false);
                        cx.notify();
                    }
                    None => this.write_export(ix, path, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    fn write_export(&mut self, ix: usize, path: PathBuf, cx: &mut Context<Self>) {
        let Some(tab) = self.results.get(ix) else {
            return;
        };
        let rows = tab.table.read(cx).delegate();
        let (text, shown) = (rows.export(Format::Csv, None), rows.shown_len());
        cx.spawn(async move |this, cx| {
            let target = path.clone();
            let written = session::join(runtime::spawn_blocking(move || {
                std::fs::write(&target, text).map_err(AppError::storage)
            }))
            .await;
            this.update(cx, |this, cx| {
                match written {
                    Ok(()) => this.log(
                        format!("Exported {} to {}", count(shown, "row"), path.display()),
                        false,
                    ),
                    Err(err) => this.log(format!("Export failed: {err}"), true),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[cfg(test)]
    pub fn editor(&self) -> &Entity<EditorState> {
        &self.editor
    }

    #[cfg(any(test, feature = "bench"))]
    pub fn results(&self) -> Vec<Entity<TableState<ResultSet>>> {
        self.results.iter().map(|tab| tab.table.clone()).collect()
    }

    /// The header text of each result tab, then of the Output pane.
    #[cfg(test)]
    pub fn summaries(&self) -> (Vec<String>, Option<String>) {
        let tabs = self.results.iter().map(|t| t.summary.to_string()).collect();
        (tabs, self.summary.as_ref().map(|s| s.to_string()))
    }

    #[cfg(test)]
    pub fn output_lines(&self) -> Vec<String> {
        self.output.iter().map(|l| l.text.to_string()).collect()
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The data source this console runs on, or the one selected in the
    /// explorer while it isn't bound yet.
    pub fn source(&self, cx: &App) -> Option<ConnectionId> {
        self.source
            .or_else(|| self.explorer.read(cx).selected_connection(cx))
    }

    pub fn is_bound(&self) -> bool {
        self.source.is_some()
    }

    /// The session of [`Self::source`], if it's connected.
    fn session(&self, cx: &App) -> Result<(ConnectionId, Arc<Session>), &'static str> {
        let id = self
            .source(cx)
            .ok_or("Select a data source in the explorer to run queries.")?;
        match self.data_sources.read(cx).state(id) {
            SourceState::Connected(session) => Ok((id, session.clone())),
            _ => Err("Connect this console's data source to run queries."),
        }
    }

    /// Adds `sql` after what's in the editor and selects it, so a run takes
    /// just this statement and the user's own SQL stays put (and undoable).
    pub fn insert_sql(
        &mut self,
        sql: &str,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if run && self.running.is_some() {
            window.push_notification(
                Notification::warning("A query is already running. Cancel it first."),
                cx,
            );
            return;
        }
        self.editor.update(cx, |editor, cx| {
            let current = editor.value();
            let head = current.trim_end();
            let text = if head.is_empty() {
                sql.to_string()
            } else {
                format!("{head}\n\n{sql}")
            };
            let start = text.len() - sql.len();
            editor.replace_all(text.clone(), window, cx);
            editor.set_selected_range(start..text.len(), cx);
            editor.focus(window, cx);
        });
        if run {
            self.run(&RunQuery, window, cx);
        }
        cx.notify();
    }

    /// Puts the keyboard in the editor.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    /// What a run sends: the selection if there is one, else the whole
    /// editor (`script`) or the statement at the caret.
    fn sql(&self, engine: Engine, script: bool, cx: &App) -> String {
        let editor = self.editor.read(cx);
        let selected = editor.selected_value();
        if !selected.trim().is_empty() {
            return selected.to_string();
        }
        let text = editor.value();
        if script {
            return split::script(&text, engine);
        }
        split::statement_at(&text, engine, editor.cursor())
            .map(|range| text[range].to_owned())
            .unwrap_or_default()
    }

    pub(crate) fn run(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.start(false, window, cx);
    }

    fn run_script(&mut self, _: &RunScript, window: &mut Window, cx: &mut Context<Self>) {
        self.start(true, window, cx);
    }

    fn start(&mut self, script: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let (id, session) = match self.session(cx) {
            Ok(found) => found,
            Err(message) => {
                window.push_notification(Notification::warning(message), cx);
                return;
            }
        };
        let Some(engine) = self.data_sources.read(cx).get(id).map(|c| c.engine) else {
            return;
        };
        self.engine = engine;
        let sql = self.sql(engine, script, cx);
        if sql.trim().is_empty() {
            return;
        }
        self.source = Some(id);

        self.results.clear();
        self.output.clear();
        self.pane = Pane::Output;
        self.summary = None;
        self.pending_export = None;
        self.filter
            .update(cx, |filter, cx| filter.set_value("", window, cx));
        let run_sql = sql.clone();
        let reader = cx.spawn_in(window, async move |this, cx| {
            let started = session::join(runtime::spawn(async move { session.execute(sql).await }));
            let mut query = match started.await {
                Ok(query) => query,
                Err(err) => {
                    this.update_in(cx, |this, window, cx| this.fail(err, window, cx))
                        .ok();
                    this.update(cx, |this, cx| this.finish(cx)).ok();
                    return;
                }
            };
            // Rows of the current result set; `None` for statements without one.
            let mut loaded: Option<usize> = None;
            let mut skipped = 0;
            let mut pacer: Option<Arc<Pacer>> = None;
            while let Some(event) = query.next().await {
                let alive = match event {
                    Ok(QueryEvent::Columns(meta)) => {
                        let fresh = Pacer::new();
                        pacer = Some(fresh.clone());
                        loaded = Some(0);
                        skipped = 0;
                        this.update_in(cx, |this, window, cx| {
                            this.start_result(meta, fresh, window, cx)
                        })
                    }
                    Ok(QueryEvent::Rows(rows)) => {
                        let total = loaded.get_or_insert(0);
                        // Wait with this page in hand rather than after adding
                        // it, so an end that follows the last page isn't held up.
                        let next = match &pacer {
                            Some(pacer) => pacer.room_for(*total).await,
                            None => Next::Load,
                        };
                        if next == Next::Skip {
                            skipped += rows.len();
                            continue;
                        }
                        *total += rows.len();
                        let total = *total;
                        this.update(cx, |this, cx| this.add_rows(rows, total, cx))
                    }
                    Ok(QueryEvent::Done {
                        rows_affected,
                        elapsed,
                    }) => {
                        let rows = loaded.take();
                        pacer = None;
                        this.update(cx, |this, cx| {
                            this.statement_done(rows, skipped, rows_affected, elapsed, cx)
                        })
                    }
                    Err(err) => this.update_in(cx, |this, window, cx| this.fail(err, window, cx)),
                };
                if alive.is_err() {
                    return;
                }
            }
            this.update(cx, |this, cx| this.finish(cx)).ok();
        });
        self.running = Some(Running {
            started: Instant::now(),
            ran_at: SystemTime::now(),
            source: id,
            sql: run_sql,
            rows: 0,
            error: None,
            _reader: reader,
        });
        cx.notify();
    }

    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        let Some(mut running) = self.running.take() else {
            return;
        };
        running.error = Some("cancelled".into());
        self.record(&running, cx);
        drop(running);
        // Only the last result can still be streaming.
        let streaming = self
            .results
            .last()
            .map(|tab| tab.table.read(cx).delegate())
            .filter(|rows| rows.pacer().is_some())
            .map(|rows| rows.len());
        let message = match streaming {
            Some(rows) => format!("Cancelled after {}", count(rows, "row")),
            None => "Cancelled".to_string(),
        };
        if let Some(rows) = streaming {
            self.set_last_summary(format!("{} · cancelled", count(rows, "row")));
        }
        self.stop_grid(cx);
        self.log(message, false);
        if self.pending_export.take().is_some() {
            self.log("Export cancelled".into(), true);
        }
        self.summary = Some("cancelled".into());
        cx.notify();
    }

    fn start_result(
        &mut self,
        meta: Arc<[savoia_core::ColumnMeta]>,
        pacer: Arc<Pacer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let table = cx.new(|cx| {
            let mut table = TableState::new(ResultSet::empty(self.engine), window, cx);
            table.delegate_mut().start(meta, pacer);
            table.refresh(cx);
            table
        });
        // Scrolling asks for rows, which can end a pause.
        let scrolled = cx.observe(&table, |_, _, cx| cx.notify());
        self.results.push(ResultTab {
            table,
            summary: "0 rows…".into(),
            _scrolled: scrolled,
        });
        self.pane = Pane::Result(self.results.len() - 1);
        cx.notify();
    }

    fn add_rows(&mut self, rows: Vec<savoia_core::Row>, total: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.results.last() {
            tab.table.update(cx, |table, cx| {
                table.delegate_mut().extend(rows);
                cx.notify();
            });
        }
        self.set_last_summary(format!("{}…", count(total, "row")));
        cx.notify();
    }

    fn statement_done(
        &mut self,
        rows: Option<usize>,
        skipped: usize,
        rows_affected: Option<u64>,
        elapsed: Duration,
        cx: &mut Context<Self>,
    ) {
        let what = match (rows, rows_affected) {
            (Some(rows), _) if skipped > 0 => {
                format!("{} ({} skipped)", count(rows, "row"), count(skipped, "row"))
            }
            (Some(rows), _) => count(rows, "row"),
            (None, Some(affected)) => format!("{} affected", count(affected as usize, "row")),
            (None, None) => "OK".to_string(),
        };
        if let Some(running) = &mut self.running {
            running.rows += match rows {
                Some(rows) => (rows + skipped) as u64,
                None => rows_affected.unwrap_or(0),
            };
        }
        let line = format!("{what} · {}", duration(elapsed));
        if rows.is_some() {
            self.set_last_summary(line.clone());
            self.stop_grid(cx);
            let last = self.results.len().wrapping_sub(1);
            if let Some((_, path)) = self.pending_export.take_if(|(ix, _)| *ix == last) {
                self.write_export(last, path, cx);
            }
        }
        self.log(line, false);
        cx.notify();
    }

    fn fail(&mut self, err: AppError, window: &mut Window, cx: &mut Context<Self>) {
        let message = err.to_string();
        if let Some(running) = &mut self.running {
            running.error = Some(message.clone());
        }
        self.log(message.clone(), true);
        self.pane = Pane::Output;
        window.push_notification(Notification::error(message), cx);
        cx.notify();
    }

    /// The execution ended (all statements done, or an error).
    fn finish(&mut self, cx: &mut Context<Self>) {
        if let Some(running) = self.running.take() {
            self.record(&running, cx);
            self.stop_grid(cx);
            let time = duration(running.started.elapsed());
            self.summary = Some(
                match self.results.len() {
                    0 => time,
                    n => format!("{} · {time}", count(n, "result")),
                }
                .into(),
            );
        }
        cx.notify();
    }

    fn record(&self, running: &Running, cx: &mut Context<Self>) {
        let entry = HistoryEntry {
            connection: running.source,
            sql: running.sql.clone(),
            ran_at: running.ran_at,
            duration: running.started.elapsed(),
            rows: running.rows,
            error: running.error.clone(),
        };
        self.data_sources
            .update(cx, |ds, _| ds.record_history(&entry));
    }

    /// Opens the history; picking an entry puts its SQL in this console.
    fn show_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let console = cx.entity().downgrade();
        let panel = cx.new(|cx| {
            HistoryPanel::new(
                self.data_sources.clone(),
                Rc::new(move |sql: String, window: &mut Window, cx: &mut App| {
                    console
                        .update(cx, |console, cx| {
                            console.insert_sql(&sql, false, window, cx)
                        })
                        .ok();
                }),
                window,
                cx,
            )
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Query history")
                .w(px(720.))
                .child(panel.clone())
        });
    }

    /// Ends streaming into the last result, the only one that can stream.
    fn stop_grid(&self, cx: &mut Context<Self>) {
        if let Some(tab) = self.results.last() {
            tab.table.update(cx, |table, cx| {
                table.delegate_mut().finish();
                cx.notify();
            });
        }
    }

    fn set_last_summary(&mut self, summary: String) {
        if let Some(tab) = self.results.last_mut() {
            tab.summary = summary.into();
        }
    }

    fn log(&mut self, text: String, error: bool) {
        self.output.push(OutputLine {
            text: text.into(),
            error,
        });
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The console's Ivrea band: the zone you run things from. Run is the
        // app's one Savoy-blue control.
        let sep = || {
            div()
                .w(px(1.))
                .h(px(16.))
                .mx_1()
                .bg(theme::c(theme::BAND_RULE))
        };
        let tool = |id: &'static str| Button::new(id).custom(theme::band_button(cx)).small();
        let running = self.is_running();
        let source = self
            .source(cx)
            .and_then(|id| self.data_sources.read(cx).get(id))
            .map(|c| c.display_name());

        h_flex()
            .h(px(34.))
            .px_2()
            .gap_0p5()
            .bg(theme::c(theme::IVREA_GREEN))
            .text_color(theme::c(theme::BAND_INK))
            .child(
                Button::new("run")
                    .custom(theme::run_button(cx))
                    .bg(theme::run_fill())
                    .small()
                    .icon(Icon::new(IconName::Play))
                    .label("Run")
                    .tooltip_with_action(
                        "Run the statement at the caret, or the selection",
                        &RunQuery,
                        Some(CONTEXT),
                    )
                    .disabled(running)
                    .on_click(cx.listener(|this, _, window, cx| this.run(&RunQuery, window, cx))),
            )
            .child(
                tool("run-script")
                    .icon(Icon::new(Lucide::ListVideo))
                    .tooltip_with_action("Run the whole script", &RunScript, Some(CONTEXT))
                    .band_disabled(running)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.run_script(&RunScript, window, cx)),
                    ),
            )
            .child(
                tool("cancel")
                    .icon(Icon::new(Lucide::CircleStop))
                    .tooltip("Cancel running query")
                    .band_disabled(!running)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            .child(sep())
            .child(
                tool("history")
                    .icon(Icon::new(Lucide::Timer))
                    .tooltip("Query history")
                    .on_click(cx.listener(|this, _, window, cx| this.show_history(window, cx))),
            )
            .child(
                tool("tx")
                    .label("Tx: Auto")
                    .tooltip("Transaction mode")
                    .on_click(coming_later("Transaction modes")),
            )
            .child(sep())
            .child(
                tool("explain")
                    .label("Explain")
                    .tooltip("Explain plan")
                    .on_click(coming_later("Explain plans")),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .text_color(theme::c(theme::BAND_INK_MUTED))
                    .child(Icon::new(Lucide::Database).xsmall())
                    .child(source.unwrap_or_else(|| "no data source".to_string())),
            )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // Only the last result streams, so only it can be paused.
        let paused = self.results.last().and_then(|tab| {
            let rows = tab.table.read(cx).delegate();
            rows.is_paused()
                .then(|| rows.pacer().cloned())
                .flatten()
                .map(|pacer| (self.results.len(), pacer))
        });
        let pane_tab = |id: ElementId, label: SharedString, pane: Pane| {
            let selected = self.pane == pane;
            div()
                .id(id)
                .flex_none()
                .cursor_pointer()
                .border_b_2()
                .border_color(if selected {
                    cx.theme().ring
                } else {
                    transparent_black()
                })
                .when(!selected, |this| this.text_color(theme.muted_foreground))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.pane = pane;
                    cx.notify();
                }))
        };
        let result_ix = match self.pane {
            Pane::Result(ix) if ix < self.results.len() => Some(ix),
            _ => None,
        };
        let summary = match self.pane {
            Pane::Result(ix) => self.results.get(ix).map(|tab| tab.summary.clone()),
            Pane::Output => self.summary.clone(),
        };

        let body = match self.pane {
            Pane::Result(ix) if ix < self.results.len() => div()
                .flex_1()
                .min_h_0()
                .child(DataTable::new(&self.results[ix].table).bordered(false))
                .into_any_element(),
            _ if self.output.is_empty() => div()
                .flex_1()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(if self.is_running() {
                    "Running…".to_string()
                } else {
                    format!(
                        "Run a query ({}) to see results.",
                        keys_label("secondary-enter")
                    )
                })
                .into_any_element(),
            _ => v_flex()
                .id("output")
                .flex_1()
                .min_h_0()
                .p_3()
                .gap_1()
                .overflow_y_scroll()
                .font_family("monospace")
                .text_sm()
                .children(self.output.iter().map(|line| {
                    div()
                        .when(line.error, |this| this.text_color(theme.danger))
                        .child(line.text.clone())
                }))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(28.))
                    .px_3()
                    .gap_3()
                    .text_xs()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .id("pane-tabs")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .gap_3()
                            .overflow_x_scroll()
                            .child(pane_tab(
                                "pane-output".into(),
                                "Output".into(),
                                Pane::Output,
                            ))
                            .children((0..self.results.len()).map(|ix| {
                                pane_tab(
                                    ("pane-result", ix).into(),
                                    format!("Result {}", ix + 1).into(),
                                    Pane::Result(ix),
                                )
                            })),
                    )
                    .when_some(paused, |this, (number, pacer)| {
                        let skip = pacer.clone();
                        this.child(
                            div()
                                .flex_none()
                                .text_color(theme.muted_foreground)
                                .child(format!("Result {number} paused · later statements wait")),
                        )
                        .child(
                            Button::new("load-all")
                                .ghost()
                                .xsmall()
                                .label("Load all")
                                .tooltip("Load every remaining row of this result")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    pacer.load_all();
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("skip-rest")
                                .ghost()
                                .xsmall()
                                .label("Skip rest")
                                .tooltip("Discard the remaining rows and continue")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    skip.skip_rest();
                                    cx.notify();
                                })),
                        )
                    })
                    .children(summary.map(|s| {
                        div()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(s)
                    }))
                    .when_some(result_ix, |this, ix| {
                        this.child(div().w(px(160.)).child(Input::new(&self.filter).xsmall()))
                            .child(
                                Button::new("copy-result")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(IconName::Copy))
                                    .tooltip("Copy the shown rows as TSV (right-click a row for more formats)")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy_result(ix, cx)
                                    })),
                            )
                            .child(
                                Button::new("export-result")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(Lucide::Download))
                                    .tooltip("Export the shown rows to CSV…")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.export_result(ix, window, cx)
                                    })),
                            )
                    }),
            )
            .child(body)
    }
}

impl Render for QueryConsole {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::run))
            .on_action(cx.listener(Self::run_script))
            .size_full()
            .child(self.render_toolbar(cx))
            .child(
                div().flex_1().min_h_0().child(
                    v_resizable("console-split")
                        .child(
                            resizable_panel()
                                .child(Editor::new(&self.editor).bordered(false).h_full()),
                        )
                        .child(
                            resizable_panel()
                                .size(px(300.))
                                .size_range(px(120.)..px(2000.))
                                .child(self.render_results(cx)),
                        ),
                ),
            )
    }
}

/// "1 row", "1,234 rows".
fn count(n: usize, noun: &str) -> String {
    let digits = n.to_string();
    let mut grouped = String::new();
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(d);
    }
    let plural = if n == 1 { "" } else { "s" };
    format!("{grouped} {noun}{plural}")
}

/// "12 ms", "3.40 s".
fn duration(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{} ms", d.as_millis())
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}

/// Click handler for console controls whose feature lands later in M2, so
/// they say so instead of doing nothing.
fn coming_later(feature: &'static str) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    move |_, window, cx| {
        window.push_notification(
            Notification::info(format!("{feature} isn't available yet.")),
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{count, duration};

    #[test]
    fn formats_counts_and_durations() {
        assert_eq!(count(1, "row"), "1 row");
        assert_eq!(count(0, "row"), "0 rows");
        assert_eq!(count(1234567, "row"), "1,234,567 rows");
        assert_eq!(duration(Duration::from_millis(12)), "12 ms");
        assert_eq!(duration(Duration::from_millis(3400)), "3.40 s");
    }
}
