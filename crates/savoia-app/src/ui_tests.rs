//! Headless UI tests: real views in a test window, driven by clicks.
//! The live test needs `SAVOIA_PG_URL` (docker compose); it polls in real
//! time because database I/O runs on the Tokio runtime, not the test clock.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::table::TableDelegate as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Entity, TestAppContext, Window, WindowOptions,
};
use savoia_store::{ConnectionStore, MemorySecrets};

use crate::connection_form::ConnectionForm;
use crate::console::QueryConsole;
use crate::data_sources::{DataSources, DataSourcesEvent, RefreshState, SourceState};
use crate::data_view::DataView;
use crate::diagram::ErDiagram;
use crate::explorer::{Explorer, NodeRef};
use crate::history::HistoryPanel;
use crate::session::Session;
use crate::structure::StructureView;
use crate::transfer::{Direction, TransferView};
use crate::workspace::{NewConsole, Workspace};

fn mount(
    cx: &mut TestAppContext,
) -> (Entity<DataSources>, Entity<ConnectionForm>, AnyWindowHandle) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::apply(crate::theme::Appearance::Dark, cx);
    });
    let ds = cx.new(|_| {
        DataSources::with_stores(
            ConnectionStore::open_in_memory(),
            Arc::new(MemorySecrets::default()),
        )
    });
    let (window, form) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| ConnectionForm::new(ds.clone(), None, window, cx))
            })
        })
        .expect("window");
    (ds, form, window)
}

fn import(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    form: &Entity<ConnectionForm>,
    url: &str,
) {
    let url = url.to_owned();
    cx.update_window(window, |_, window, cx| {
        form.read(cx)
            .url_input()
            .update(cx, |input, cx| input.set_value(url, window, cx));
        window.render_frame(cx);
        window.click("import", cx);
    })
    .unwrap();
}

fn click(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
}

#[gpui_kit::test]
async fn saving_without_a_user_shows_an_error_and_saves_nothing(cx: &mut TestAppContext) {
    let (ds, form, window) = mount(cx);
    import(cx, window, &form, "postgres://localhost/db");
    click(cx, window, "save");

    let status = form.read_with(cx, |f, _| f.status_text());
    assert_eq!(status.as_deref(), Some("User is required"));
    assert!(ds.read_with(cx, |ds, _| ds.connections().is_empty()));
}

#[gpui_kit::test]
async fn bad_url_is_reported(cx: &mut TestAppContext) {
    let (_, form, window) = mount(cx);
    import(cx, window, &form, "redis://localhost");
    let status = form
        .read_with(cx, |f, _| f.status_text())
        .unwrap_or_default();
    assert!(status.contains("Unsupported scheme"), "{status}");
}

/// Polls in real time (I/O runs on the Tokio runtime) until `done` holds.
fn wait_until(
    cx: &mut TestAppContext,
    what: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    for _ in 0..300 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// Fills the form from `url`, clicks Save & Connect and waits for the
/// session. Returns the data sources and an explorer over them.
fn save_and_connect(cx: &mut TestAppContext, url: &str) -> (Entity<DataSources>, Entity<Explorer>) {
    cx.executor().allow_parking();
    let (ds, form, window) = mount(cx);
    let explorer = cx.new(|cx| Explorer::new(ds.clone(), cx));

    import(cx, window, &form, url);
    click(cx, window, "save-connect");

    let id = ds
        .read_with(cx, |ds, _| ds.connections().first().map(|c| c.id))
        .expect("saved");
    wait_until(cx, "the session", |cx| {
        ds.read_with(cx, |ds, _| match ds.state(id) {
            SourceState::Connected(_) => true,
            SourceState::Failed(err) => panic!("connect failed: {err}"),
            _ => false,
        })
    });
    (ds, explorer)
}

/// The visible tree rows once nothing shown is loading any more.
fn settled_labels(cx: &mut TestAppContext, explorer: &Entity<Explorer>) -> Vec<String> {
    let loading = |labels: &[String]| labels.iter().any(|l| l.ends_with('…'));
    wait_until(cx, "the explorer to load", |cx| {
        !loading(&explorer.read_with(cx, |e, _| e.visible_labels()))
    });
    explorer.read_with(cx, |e, _| e.visible_labels())
}

async fn save_connect_and_list(cx: &mut TestAppContext, url: &str) -> Vec<String> {
    let (_, explorer) = save_and_connect(cx, url);
    settled_labels(cx, &explorer)
}

/// Connects to `url` and opens a console on it, with the source selected.
fn console_on(
    cx: &mut TestAppContext,
    url: &str,
) -> (Entity<DataSources>, Entity<QueryConsole>, AnyWindowHandle) {
    let (ds, explorer) = save_and_connect(cx, url);
    explorer.update(cx, |e, cx| e.select_first_source(cx));
    cx.update(crate::console::init);
    let (window, console) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| QueryConsole::new(ds.clone(), explorer, None, window, cx))
            })
        })
        .expect("window");
    (ds, console, window)
}

/// Types `sql` into the console and clicks Run script.
fn run(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    console: &Entity<QueryConsole>,
    sql: &str,
) {
    let sql = sql.to_owned();
    cx.update_window(window, |_, window, cx| {
        console
            .read(cx)
            .editor()
            .clone()
            .update(cx, |editor, cx| editor.set_value(sql, window, cx));
    })
    .unwrap();
    click(cx, window, "run-script");
}

/// Row count and first row of result `ix` (negative: from the end).
fn grid_at(
    cx: &mut TestAppContext,
    console: &Entity<QueryConsole>,
    ix: isize,
) -> (usize, Vec<String>) {
    console.read_with(cx, |c, cx| {
        let results = c.results();
        let Some(table) = results.get(ix.rem_euclid(results.len().max(1) as isize) as usize) else {
            return (0, Vec::new());
        };
        let table = table.read(cx);
        let rows = table.delegate();
        let first = (0..rows.columns_count(cx))
            .filter(|_| rows.rows_count(cx) > 0)
            .map(|col| rows.cell_text(0, col, cx))
            .collect();
        (rows.len(), first)
    })
}

/// The last result.
fn grid(cx: &mut TestAppContext, console: &Entity<QueryConsole>) -> (usize, Vec<String>) {
    grid_at(cx, console, -1)
}

fn idle(cx: &mut TestAppContext, console: &Entity<QueryConsole>) {
    wait_until(cx, "the query to end", |cx| {
        console.read_with(cx, |c, _| !c.is_running())
    });
}

fn output(cx: &mut TestAppContext, console: &Entity<QueryConsole>) -> Vec<String> {
    console.read_with(cx, |c, _| c.output_lines())
}

fn runs_a_script_and_reports_each_statement(url: &str, cx: &mut TestAppContext) {
    let (_, console, window) = console_on(cx, url);
    run(
        cx,
        window,
        &console,
        "CREATE TEMPORARY TABLE ui_t (n INT);\n\
         INSERT INTO ui_t VALUES (1), (2);\n\
         SELECT 42 AS n, NULL AS x;",
    );
    idle(cx, &console);
    assert_eq!(
        grid(cx, &console),
        (1, vec!["1".into(), "42".into(), "NULL".into()])
    );
    let lines = output(cx, &console);
    assert!(lines[1].starts_with("2 rows affected · "), "{lines:?}");
    assert!(lines[2].starts_with("1 row · "), "{lines:?}");

    run(cx, window, &console, "SELECT * FROM no_such_table");
    idle(cx, &console);
    let lines = output(cx, &console);
    assert!(lines.last().unwrap().contains("no_such_table"), "{lines:?}");
}

#[gpui_kit::test]
async fn postgres_console_runs_a_script(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    runs_a_script_and_reports_each_statement(&url, cx);
}

#[gpui_kit::test]
async fn mysql_console_runs_a_script(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_MYSQL_URL") else {
        return;
    };
    runs_a_script_and_reports_each_statement(&url, cx);
}

/// A result that ends exactly where the grid stops asking still finishes:
/// the reader only waits when another page actually arrives.
#[gpui_kit::test]
async fn postgres_console_finishes_a_result_that_fills_the_grid(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (_, console, window) = console_on(cx, &url);
    run(
        cx,
        window,
        &console,
        "SELECT g FROM generate_series(1, 1000) g",
    );
    idle(cx, &console);
    assert_eq!(grid(cx, &console).0, 1000);
}

fn paused(cx: &mut TestAppContext, console: &Entity<QueryConsole>) -> bool {
    console.read_with(cx, |c, cx| {
        c.results()
            .last()
            .is_some_and(|t| t.read(cx).delegate().is_paused())
    })
}

/// Skip rest discards the paused result's remaining rows so the script goes on.
#[gpui_kit::test]
async fn postgres_console_skips_the_rest_of_a_paused_result(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (_, console, window) = console_on(cx, &url);
    run(
        cx,
        window,
        &console,
        "SELECT g FROM generate_series(1, 100000) g; SELECT 'after' AS a",
    );
    wait_until(cx, "the pause", |cx| paused(cx, &console));
    click(cx, window, "skip-rest");
    idle(cx, &console);

    // Each result keeps its own tab: the kept rows, then the next statement.
    assert_eq!(console.read_with(cx, |c, _| c.results().len()), 2);
    assert_eq!(grid_at(cx, &console, 0).0, 1000);
    assert_eq!(grid_at(cx, &console, 1).1[1], "after");
    let (tabs, run) = console.read_with(cx, |c, _| c.summaries());
    assert!(
        tabs[0].starts_with("1,000 rows (99,000 rows skipped) · "),
        "{tabs:?}"
    );
    assert!(tabs[1].starts_with("1 row · "), "{tabs:?}");
    assert!(
        run.as_deref().unwrap_or("").starts_with("2 results · "),
        "{run:?}"
    );
    let lines = output(cx, &console);
    assert!(
        lines[0].starts_with("1,000 rows (99,000 rows skipped) · "),
        "{lines:?}"
    );
    assert!(lines[1].starts_with("1 row · "), "{lines:?}");
}

/// Load all streams the rest of a paused result into the grid.
#[gpui_kit::test]
async fn mysql_console_loads_all_of_a_paused_result(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_MYSQL_URL") else {
        return;
    };
    let (_, console, window) = console_on(cx, &url);
    run(
        cx,
        window,
        &console,
        // Recursion stops at depth 1,000 by default; cross join to get more rows.
        "WITH RECURSIVE k (n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM k WHERE n < 999) \
         SELECT a.n * 1000 + b.n AS n FROM k a CROSS JOIN k b LIMIT 20000",
    );
    wait_until(cx, "the pause", |cx| paused(cx, &console));
    click(cx, window, "load-all");
    idle(cx, &console);
    assert_eq!(grid(cx, &console).0, 20000);
}

/// A big result loads only what the grid asked for; Cancel stops it and
/// frees the connection for the next run.
#[gpui_kit::test]
async fn postgres_console_pauses_big_results_and_cancels(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (_, console, window) = console_on(cx, &url);
    run(
        cx,
        window,
        &console,
        "SELECT g FROM generate_series(1, 100000) g",
    );
    wait_until(cx, "the first rows", |cx| grid(cx, &console).0 >= 1000);
    std::thread::sleep(Duration::from_millis(300));
    cx.run_until_parked();
    let (loaded, _) = grid(cx, &console);
    assert!(loaded < 100_000, "loaded everything: {loaded}");
    assert!(console.read_with(cx, |c, _| c.is_running()));

    click(cx, window, "cancel");
    assert!(!console.read_with(cx, |c, _| c.is_running()));
    assert!(output(cx, &console)[0].starts_with("Cancelled after "));

    run(cx, window, &console, "SELECT 'next'");
    idle(cx, &console);
    assert_eq!(grid(cx, &console).1[1], "next");
}

#[gpui_kit::test]
async fn postgres_save_and_connect_opens_the_default_schema(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let labels = save_connect_and_list(cx, &format!("{url}?sslmode=require")).await;
    // source → databases (current expanded) → schemas (public expanded) → groups
    assert_eq!(
        labels.first().map(String::as_str),
        Some("127.0.0.1:54317"),
        "{labels:?}"
    );
    let public = labels
        .iter()
        .position(|l| l == "public")
        .expect("public schema");
    assert_eq!(
        labels.get(public + 1).map(String::as_str),
        Some("tables"),
        "{labels:?}"
    );
}

#[gpui_kit::test]
async fn mysql_save_and_connect_shows_groups_under_the_database(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_MYSQL_URL") else {
        return;
    };
    let labels = save_connect_and_list(cx, &url).await;
    // No schema level in MySQL: source → databases (current expanded) → groups
    let db = labels
        .iter()
        .position(|l| l == "savoia")
        .expect("current database");
    assert_eq!(
        labels.get(db + 1).map(String::as_str),
        Some("tables"),
        "{labels:?}"
    );
}

fn session_of(cx: &mut TestAppContext, ds: &Entity<DataSources>) -> Option<Arc<Session>> {
    ds.read_with(cx, |ds, _| match ds.state(ds.connections()[0].id) {
        SourceState::Connected(session) => Some(session.clone()),
        _ => None,
    })
}

fn refresh_state(cx: &mut TestAppContext, ds: &Entity<DataSources>) -> Option<RefreshState> {
    ds.read_with(cx, |ds, _| ds.refresh_state(ds.connections()[0].id))
}

/// Refresh waits for the running query and then reloads the catalog on the
/// same connection; disconnecting abandons the wait.
#[gpui_kit::test]
async fn postgres_refresh_waits_for_the_running_query(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, console, window) = console_on(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    let before = session_of(cx, &ds).expect("connected");
    let big = "SELECT g FROM generate_series(1, 100000) g";

    run(cx, window, &console, big);
    wait_until(cx, "the pause", |cx| paused(cx, &console));
    ds.update(cx, |ds, cx| ds.refresh(id, cx));
    std::thread::sleep(Duration::from_millis(300));
    cx.run_until_parked();
    assert_eq!(refresh_state(cx, &ds), Some(RefreshState::Waiting));

    click(cx, window, "cancel");
    wait_until(cx, "the refresh", |cx| refresh_state(cx, &ds).is_none());
    let after = session_of(cx, &ds).expect("still connected");
    assert!(Arc::ptr_eq(&before, &after), "refresh reconnected");

    run(cx, window, &console, big);
    wait_until(cx, "the pause", |cx| paused(cx, &console));
    ds.update(cx, |ds, cx| {
        ds.refresh(id, cx);
        ds.disconnect(id, cx);
    });
    assert_eq!(refresh_state(cx, &ds), None);
    assert!(session_of(cx, &ds).is_none());
}

/// Runs `sql` to the end on the source's own session.
fn exec(session: Arc<Session>, sql: &str) {
    let sql = sql.to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    drop(crate::runtime::spawn(async move {
        let result = async {
            let mut query = session.execute(sql).await?;
            while let Some(event) = query.next().await {
                event?;
            }
            Ok::<_, savoia_core::AppError>(())
        }
        .await;
        drop(tx.send(result));
    }));
    rx.recv_timeout(Duration::from_secs(15))
        .expect("seed timed out")
        .expect("seed failed");
}

const UI_SCHEMA: &str = "DROP SCHEMA IF EXISTS it_ui CASCADE;
    CREATE SCHEMA it_ui;
    CREATE TABLE it_ui.customers (id serial PRIMARY KEY, name text NOT NULL);
    CREATE TABLE it_ui.orders (
      id serial PRIMARY KEY,
      customer_id int NOT NULL REFERENCES it_ui.customers (id),
      placed_at timestamptz);
    CREATE TABLE it_ui.notes (body text);";

/// Expanding a schema loads its object names; expanding a table loads its
/// columns and keys. The diagram of the schema joins the tables by their
/// foreign keys.
#[gpui_kit::test]
async fn postgres_explorer_loads_on_expand_and_draws_the_diagram(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, explorer) = save_and_connect(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    exec(session_of(cx, &ds).expect("connected"), UI_SCHEMA);
    ds.update(cx, |ds, cx| ds.refresh(id, cx));
    wait_until(cx, "the refresh", |cx| refresh_state(cx, &ds).is_none());

    let labels = settled_labels(cx, &explorer);
    let schema = labels.iter().position(|l| l == "it_ui").expect("it_ui");
    assert_ne!(
        labels.get(schema + 1).map(String::as_str),
        Some("tables"),
        "not loaded before expanding"
    );

    explorer.update(cx, |e, cx| e.expand(&["it_ui"], cx));
    let labels = settled_labels(cx, &explorer);
    assert_eq!(
        labels.get(schema + 1).map(String::as_str),
        Some("tables"),
        "{labels:?}"
    );

    explorer.update(cx, |e, cx| e.expand(&["it_ui", "tables"], cx));
    explorer.update(cx, |e, cx| e.expand(&["it_ui", "tables", "orders"], cx));
    let labels = settled_labels(cx, &explorer);
    let orders = labels.iter().position(|l| l == "orders").expect("orders");
    assert_eq!(
        labels[orders + 1..orders + 6],
        ["id", "customer_id", "placed_at", "foreign keys", "indexes"],
        "{labels:?}"
    );

    let node = NodeRef {
        connection: id,
        database: "savoia".into(),
        schema: "it_ui".into(),
        table: Some("orders".into()),
    };
    let (window, diagram) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|cx| ErDiagram::new(ds.clone(), node, cx))
            })
        })
        .expect("window");
    wait_until(cx, "the diagram", |cx| {
        diagram.read_with(cx, |d, _| d.loaded().is_some())
    });
    let (tables, lines) = diagram.read_with(cx, |d, _| d.loaded().unwrap());
    assert_eq!(tables, ["customers", "notes", "orders"]);
    assert_eq!(lines, 1);
    assert_eq!(
        diagram
            .read_with(cx, |d, _| d.focused().map(str::to_owned))
            .as_deref(),
        Some("orders")
    );
    // Paints boxes and lines without panicking, and survives a reset.
    click(cx, window, "erd-reset");
    let zoom = |cx: &mut TestAppContext| diagram.read_with(cx, |d, _| d.zoom());
    let fitted = zoom(cx);
    assert!(fitted <= 1., "fit never magnifies: {fitted}");

    click(cx, window, "erd-zoom-reset");
    assert!((zoom(cx) - 1.).abs() < 1e-4);
    click(cx, window, "erd-zoom-in");
    assert!((zoom(cx) - 1.25).abs() < 1e-4, "{}", zoom(cx));
    click(cx, window, "erd-zoom-out");
    click(cx, window, "erd-zoom-out");
    assert!((zoom(cx) - 0.8).abs() < 1e-4, "{}", zoom(cx));
    click(cx, window, "erd-fit");
    assert!((zoom(cx) - fitted).abs() < 1e-4);

    click(cx, window, "erd-collapse-all");
    assert_eq!(diagram.read_with(cx, |d, _| d.collapsed_count()), 3);
    click(cx, window, "erd-collapse-all");
    assert_eq!(diagram.read_with(cx, |d, _| d.collapsed_count()), 0);

    // Right after a frame, a burst of moves waits for the next 60 fps slot
    // instead of redrawing at once.
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert!(diagram.update(cx, |d, cx| d.drag_moves(50, cx)));
    std::thread::sleep(Duration::from_millis(30));
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// Memory benchmark over the Serie A sample (`samples/serie_a.postgres.sql`
/// loaded). Prints resident memory after each step:
/// `SAVOIA_PG_URL=… cargo test -p savoia-app --release memory_benchmark -- --ignored --nocapture`
/// The test platform draws nothing, so GPU-side memory of the real app isn't included.
#[gpui_kit::test]
#[ignore = "benchmark; run by hand"]
async fn memory_benchmark(cx: &mut TestAppContext) {
    use crate::memory::{format_bytes, resident_bytes};
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let mut rows: Vec<(&str, u64, Duration)> = Vec::new();
    let mut step = |name, started: std::time::Instant| {
        rows.push((name, resident_bytes().unwrap_or(0), started.elapsed()));
    };

    let t = std::time::Instant::now();
    step("start", t);
    let (ds, console, window) = console_on(cx, &url);
    step("connected, explorer open", t);

    let t = std::time::Instant::now();
    run(cx, window, &console, "SELECT * FROM serie_a.match_events");
    wait_until(cx, "the pause", |cx| paused(cx, &console));
    click(cx, window, "load-all");
    idle(cx, &console);
    assert_eq!(
        grid(cx, &console).0,
        150_000,
        "load samples/serie_a.postgres.sql first"
    );
    step("150,000 rows x 6 columns in the grid", t);

    let t = std::time::Instant::now();
    run(cx, window, &console, "SELECT 1");
    idle(cx, &console);
    step("result replaced by SELECT 1", t);

    let t = std::time::Instant::now();
    let node = NodeRef {
        connection: ds.read_with(cx, |ds, _| ds.connections()[0].id),
        database: "savoia".into(),
        schema: "serie_a".into(),
        table: None,
    };
    let (erd_window, diagram) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|cx| ErDiagram::new(ds.clone(), node, cx))
            })
        })
        .expect("window");
    wait_until(cx, "the diagram", |cx| {
        diagram.read_with(cx, |d, _| d.loaded().is_some())
    });
    for _ in 0..3 {
        cx.update_window(erd_window, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
    step("serie_a ER diagram (14 tables) drawn", t);

    println!("\n| Step | Resident memory | Δ | Took |\n| --- | --- | --- | --- |");
    let mut last = rows[0].1;
    for (name, bytes, took) in &rows {
        let delta = *bytes as i64 - last as i64;
        println!(
            "| {name} | {} | {}{} | {} ms |",
            format_bytes(*bytes),
            if delta < 0 { "-" } else { "+" },
            format_bytes(delta.unsigned_abs()),
            took.as_millis()
        );
        last = *bytes;
    }
}

/// "Open data" adds its SELECT after the user's SQL and runs only that.
#[gpui_kit::test]
async fn postgres_open_data_keeps_the_users_sql(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (_, console, window) = console_on(cx, &url);
    let object = crate::table_menu::ObjectRef::parse(
        &format!(
            "table:{}/postgres/pg_catalog/pg_namespace",
            savoia_core::ConnectionId::new()
        ),
        savoia_core::Engine::Postgres,
    )
    .unwrap();
    cx.update_window(window, |_, window, cx| {
        let editor = console.read(cx).editor().clone();
        editor.update(cx, |e, cx| e.set_value("SELECT 1 AS mine;", window, cx));
        console.update(cx, |c, cx| {
            c.insert_sql(&object.select_sql(), true, window, cx)
        });
    })
    .unwrap();
    idle(cx, &console);

    let text = console.read_with(cx, |c, cx| c.editor().read(cx).value().to_string());
    assert_eq!(
        text,
        "SELECT 1 AS mine;\n\nSELECT * FROM \"pg_catalog\".\"pg_namespace\" LIMIT 200;"
    );
    assert_eq!(console.read_with(cx, |c, _| c.results().len()), 1);
    assert!(grid(cx, &console).0 > 1);
}

/// Connecting without a stored password asks for one; a wrong one asks
/// again with the server's error; the right one connects and is kept.
#[gpui_kit::test]
async fn postgres_connect_asks_for_a_missing_password(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let without = url.replacen(":savoia@", "@", 1);
    assert_ne!(without, url, "SAVOIA_PG_URL has the password savoia");
    cx.executor().allow_parking();
    let (ds, form, window) = mount(cx);
    let asked = Rc::new(RefCell::new(Vec::new()));
    cx.update(|cx| {
        let asked = asked.clone();
        cx.subscribe(&ds, move |_, event, _| {
            if let DataSourcesEvent::NeedsPassword { error, .. } = event {
                asked.borrow_mut().push(error.clone());
            }
        })
        .detach();
    });
    import(cx, window, &form, &without);
    click(cx, window, "save-connect");
    let id = ds
        .read_with(cx, |ds, _| ds.connections().first().map(|c| c.id))
        .expect("saved");

    wait_until(cx, "the password prompt", |_| asked.borrow().len() == 1);
    assert_eq!(asked.borrow()[0], None, "nothing was tried yet");

    ds.update(cx, |ds, cx| {
        ds.connect_with_password(id, "wrong".into(), cx)
    });
    wait_until(cx, "the second prompt", |_| asked.borrow().len() == 2);
    let error = asked.borrow()[1].clone().expect("the server's error");
    assert!(error.contains("password"), "{error}");

    ds.update(cx, |ds, cx| {
        ds.connect_with_password(id, "savoia".into(), cx)
    });
    wait_until(cx, "the session", |cx| {
        ds.read_with(cx, |ds, _| {
            matches!(ds.state(id), SourceState::Connected(_))
        })
    });
    let secrets = ds.read_with(cx, |ds, _| ds.load_secrets(id)).await.unwrap();
    assert_eq!(secrets.password.as_deref(), Some("savoia"));
}

/// Expanding another Postgres database opens a connection to it and loads
/// its schemas, while a query holds the session's own connection. Refresh
/// keeps them loaded.
#[gpui_kit::test]
async fn postgres_explorer_loads_another_database_on_expand(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, console, window) = console_on(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    let session = session_of(cx, &ds).expect("connected");
    exec(
        session.clone(),
        "DROP DATABASE IF EXISTS it_other WITH (FORCE)",
    );
    exec(session.clone(), "CREATE DATABASE it_other");
    ds.update(cx, |ds, cx| ds.refresh(id, cx));
    wait_until(cx, "the refresh", |cx| refresh_state(cx, &ds).is_none());
    let explorer = cx.new(|cx| Explorer::new(ds.clone(), cx));
    settled_labels(cx, &explorer);

    // The console's paused result holds the session's connection.
    run(
        cx,
        window,
        &console,
        "SELECT g FROM generate_series(1, 100000) g",
    );
    wait_until(cx, "the pause", |cx| paused(cx, &console));
    explorer.update(cx, |e, cx| e.expand(&["it_other"], cx));
    let labels = settled_labels(cx, &explorer);
    let db = labels
        .iter()
        .position(|l| l == "it_other")
        .expect("it_other");
    assert_eq!(
        labels.get(db + 1).map(String::as_str),
        Some("public"),
        "{labels:?}"
    );
    assert!(session.is_busy(), "loaded beside the running query");
    click(cx, window, "cancel");

    explorer.update(cx, |e, cx| e.expand(&["it_other", "public"], cx));
    ds.update(cx, |ds, cx| ds.refresh(id, cx));
    wait_until(cx, "the refresh", |cx| refresh_state(cx, &ds).is_none());
    let labels = settled_labels(cx, &explorer);
    let db = labels
        .iter()
        .position(|l| l == "it_other")
        .expect("it_other");
    assert_eq!(
        labels[db + 1..db + 4],
        ["public", "tables", "views"],
        "{labels:?}"
    );

    // FORCE closes the explorer's connection to it.
    exec(session, "DROP DATABASE it_other WITH (FORCE)");
}

/// Each console tab keeps its own editor, runs on the source it was opened
/// on, and closing it cancels its query.
#[gpui_kit::test]
async fn postgres_console_tabs_are_independent(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, _) = save_and_connect(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    let session = session_of(cx, &ds).expect("connected");
    cx.update(|cx| {
        crate::console::init(cx);
        crate::commands::init(cx);
    });
    let (window, workspace) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::with_data_sources(ds.clone(), window, cx))
            })
        })
        .expect("window");
    let consoles = |cx: &mut TestAppContext| {
        workspace.read_with(cx, |w, _| w.consoles().cloned().collect::<Vec<_>>())
    };
    workspace.update(cx, |w, cx| {
        w.explorer()
            .clone()
            .update(cx, |e, cx| e.select_first_source(cx))
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |w, cx| w.new_console(&NewConsole, window, cx));
    })
    .unwrap();
    let [first, second] = consoles(cx).try_into().expect("two consoles");
    assert_eq!(second.read_with(cx, |c, cx| c.source(cx)), Some(id));

    // Only the active tab renders, so run each console directly.
    let run_in = |cx: &mut TestAppContext, console: &Entity<QueryConsole>, sql: &str| {
        let sql = sql.to_owned();
        cx.update_window(window, |_, window, cx| {
            console.update(cx, |c, cx| {
                c.editor()
                    .clone()
                    .update(cx, |editor, cx| editor.set_value(sql, window, cx));
                c.run(&crate::console::RunQuery, window, cx);
            })
        })
        .unwrap();
    };
    run_in(cx, &first, "SELECT 1");
    wait_until(cx, "the first run", |cx| {
        !first.read_with(cx, |c, _| c.is_running())
    });
    run_in(cx, &second, "SELECT g FROM generate_series(1, 100000) g");
    wait_until(cx, "the pause", |cx| paused(cx, &second));
    assert!(session.is_busy());

    workspace.update(cx, |w, cx| w.close(1, cx));
    drop(second);
    wait_until(cx, "the cancel", |_| !session.is_busy());
    let [first] = consoles(cx).try_into().expect("one console");
    let text = first.read_with(cx, |c, cx| c.editor().read(cx).value().to_string());
    assert_eq!(text, "SELECT 1");
}

/// Run takes the statement at the caret; a selection wins over it.
#[gpui_kit::test]
async fn postgres_run_takes_the_statement_at_the_caret(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (_, console, window) = console_on(cx, &url);
    let sql = "SELECT 1 AS one;\nSELECT 2 AS two;\nSELECT 3 AS three;";
    let run_at = |cx: &mut TestAppContext, range: std::ops::Range<usize>| {
        cx.update_window(window, |_, window, cx| {
            console.update(cx, |c, cx| {
                c.editor().clone().update(cx, |editor, cx| {
                    editor.set_value(sql, window, cx);
                    editor.set_selected_range(range, cx);
                });
                c.run(&crate::console::RunQuery, window, cx);
            })
        })
        .unwrap();
        wait_until(cx, "the run", |cx| {
            !console.read_with(cx, |c, _| c.is_running())
        });
        let results = console.read_with(cx, |c, _| c.results().len());
        (results, grid(cx, &console).1.last().cloned())
    };
    let caret = sql.find("2 AS").unwrap();
    assert_eq!(run_at(cx, caret..caret), (1, Some("2".into())));
    let three = sql.find("SELECT 3").unwrap();
    assert_eq!(run_at(cx, three..sql.len()), (1, Some("3".into())));
}

/// The quick filter hides rows of the result; Copy takes the rows shown,
/// with NULL kept apart from the empty string.
#[gpui_kit::test]
async fn postgres_result_filter_and_copy(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (_, console, window) = console_on(cx, &url);
    run(
        cx,
        window,
        &console,
        "SELECT * FROM (VALUES (1, 'Roma'), (2, 'Torino'), (3, NULL), (4, '')) t(n, name)",
    );
    idle(cx, &console);
    cx.update_window(window, |_, window, cx| {
        let filter = console.read(cx).filter().clone();
        filter.update(cx, |input, cx| input.set_value("tor", window, cx));
        // Typing emits a change; `set_value` doesn't.
        console.update(cx, |c, cx| c.apply_filter(cx));
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        grid(cx, &console).1.last().map(String::as_str),
        Some("Torino")
    );

    cx.update_window(window, |_, window, cx| {
        let filter = console.read(cx).filter().clone();
        filter.update(cx, |input, cx| input.set_value("", window, cx));
        console.update(cx, |c, cx| c.apply_filter(cx));
    })
    .unwrap();
    cx.run_until_parked();
    console.update(cx, |c, cx| c.copy_result(0, cx));
    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|c| c.text()));
    assert_eq!(
        copied.as_deref(),
        Some("n\tname\n1\tRoma\n2\tTorino\n3\t\\N\n4\t\n")
    );
}

/// Every run lands in the history, failed ones with their error; search
/// narrows it, and picking an entry hands its SQL back.
#[gpui_kit::test]
async fn postgres_runs_are_kept_in_the_history(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, console, window) = console_on(cx, &url);
    run(cx, window, &console, "SELECT 41 + 1 AS answer");
    idle(cx, &console);
    run(cx, window, &console, "SELECT no_such_column");
    idle(cx, &console);

    let picked = Rc::new(RefCell::new(None));
    let panel = cx
        .update_window(window, |_, window, cx| {
            let picked = picked.clone();
            cx.new(|cx| {
                HistoryPanel::new(
                    ds.clone(),
                    Rc::new(move |sql: String, _: &mut Window, _: &mut App| {
                        *picked.borrow_mut() = Some(sql)
                    }),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    let entries = panel.read_with(cx, |p, _| p.entries().to_vec());
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].sql, "SELECT no_such_column");
    assert!(
        entries[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("no_such_column"))
    );
    assert_eq!((entries[1].rows, entries[1].error.as_deref()), (1, None));

    cx.update_window(window, |_, window, cx| {
        let search = panel.read(cx).search().clone();
        search.update(cx, |input, cx| input.set_value("41", window, cx));
        panel.update(cx, |p, cx| p.reload(cx));
        panel.update(cx, |p, cx| p.pick(0, window, cx));
    })
    .unwrap();
    assert_eq!(panel.read_with(cx, |p, _| p.entries().len()), 1);
    assert_eq!(picked.borrow().as_deref(), Some("SELECT 41 + 1 AS answer"));
}

/// The Structure tab lists a table's columns, keys and indexes, and the
/// DDL rebuilt from them.
#[gpui_kit::test]
async fn postgres_structure_shows_columns_keys_and_ddl(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, _) = save_and_connect(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    exec(
        session_of(cx, &ds).expect("connected"),
        "DROP SCHEMA IF EXISTS it_structure CASCADE;
         CREATE SCHEMA it_structure;
         CREATE TABLE it_structure.teams (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE it_structure.players (
           id int PRIMARY KEY,
           team_id int REFERENCES it_structure.teams (id),
           shirt int DEFAULT 10);
         CREATE INDEX players_shirt ON it_structure.players (shirt);",
    );
    let node = NodeRef {
        connection: id,
        database: "savoia".into(),
        schema: "it_structure".into(),
        table: Some("players".into()),
    };
    let (window, view) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|cx| StructureView::new(ds.clone(), node, cx))
            })
        })
        .expect("window");
    wait_until(cx, "the structure", |cx| {
        view.read_with(cx, |v, _| v.loaded().is_some())
    });
    let info = view.read_with(cx, |v, _| v.loaded().unwrap());
    let columns: Vec<_> = info.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["id", "team_id", "shirt"]);
    let ddl = view.read_with(cx, |v, cx| v.ddl(cx).unwrap());
    assert!(ddl.contains("\"shirt\" integer DEFAULT 10"), "{ddl}");
    assert!(
        ddl.contains("REFERENCES \"it_structure\".\"teams\" (\"id\")"),
        "{ddl}"
    );
    assert!(ddl.contains("CREATE INDEX \"players_shirt\""), "{ddl}");
    assert_eq!(info.foreign_keys.len(), 1);
    // Draws without panicking.
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// Completion loads what it needs from the catalog, then suggests columns
/// by alias and whole JOIN clauses from foreign keys.
#[gpui_kit::test]
async fn postgres_completion_suggests_columns_and_joins(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, console, window) = console_on(cx, &url);
    exec(
        session_of(cx, &ds).expect("connected"),
        "DROP SCHEMA IF EXISTS it_complete CASCADE;
         CREATE SCHEMA it_complete;
         CREATE TABLE it_complete.customers (id int PRIMARY KEY, name text);
         CREATE TABLE it_complete.orders (
           id int PRIMARY KEY,
           customer_id int REFERENCES it_complete.customers (id),
           total numeric);",
    );
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    ds.update(cx, |ds, cx| ds.refresh(id, cx));
    wait_until(cx, "the refresh", |cx| refresh_state(cx, &ds).is_none());

    let complete = |cx: &mut TestAppContext, sql: &str| -> Vec<String> {
        use gpui_kit::component::input::CompletionProvider as _;
        let provider = crate::completion::SqlCompletion {
            data_sources: ds.clone(),
            console: console.downgrade(),
        };
        let rope = gpui_kit::component::Rope::from(sql);
        let context = lsp_types::CompletionContext {
            trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
            trigger_character: None,
        };
        let task = cx
            .update_window(window, |_, window, cx| {
                provider.completions(&rope, sql.len(), context, window, cx)
            })
            .unwrap();
        let done = Rc::new(RefCell::new(None));
        let slot = done.clone();
        cx.spawn(async move |_| *slot.borrow_mut() = Some(task.await))
            .detach();
        wait_until(cx, "the completions", |_| done.borrow().is_some());
        let items = match done.take().unwrap().unwrap() {
            lsp_types::CompletionResponse::Array(items) => items,
            lsp_types::CompletionResponse::List(list) => list.items,
        };
        items.into_iter().map(|i| i.label).collect()
    };

    let columns = complete(cx, "SELECT * FROM it_complete.orders o WHERE o.");
    assert_eq!(columns, ["id", "customer_id", "total"]);
    let joins = complete(cx, "SELECT * FROM it_complete.orders o JOIN ");
    assert_eq!(
        joins.first().map(String::as_str),
        Some("it_complete.customers c ON c.id = o.customer_id"),
        "{joins:?}"
    );
}

/// The data view pages through a table on the server, sorts and filters
/// there, and refuses databases its session isn't connected to.
#[gpui_kit::test]
async fn postgres_data_view_pages_sorts_and_filters(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, _) = save_and_connect(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    exec(
        session_of(cx, &ds).expect("connected"),
        "DROP SCHEMA IF EXISTS it_data CASCADE;
         CREATE SCHEMA it_data;
         CREATE TABLE it_data.players (id int PRIMARY KEY, name text, goals int);
         INSERT INTO it_data.players
           SELECT g, 'player ' || g, g % 7 FROM generate_series(1, 450) g;",
    );
    let node = |database: &str| NodeRef {
        connection: id,
        database: database.into(),
        schema: "it_data".into(),
        table: Some("players".into()),
    };
    let (_, view) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| DataView::new(ds.clone(), node("savoia"), Vec::new(), window, cx))
            })
        })
        .expect("window");
    let rows = |cx: &mut TestAppContext, what: &str, want: (usize, bool)| {
        wait_until(cx, what, |cx| {
            view.read_with(cx, |v, cx| v.rows(cx)) == Some(want)
        });
    };
    let first = |cx: &mut TestAppContext| {
        view.read_with(cx, |v, cx| {
            let grid = v.grid().unwrap();
            let row = grid.read(cx).delegate().row(0).clone();
            row.iter()
                .map(|c| c.as_deref().unwrap_or("NULL").to_owned())
                .collect::<Vec<_>>()
        })
    };
    rows(cx, "the first page", (200, true));
    assert_eq!(first(cx), ["1", "player 1", "1"]);
    view.update(cx, |v, cx| v.load_next(cx));
    rows(cx, "the second page", (400, true));
    view.update(cx, |v, cx| v.load_next(cx));
    rows(cx, "the last page", (450, false));

    // Sorting starts over, on the server: ties on goals keep key order.
    view.update(cx, |v, cx| v.sort_by(Some((2, true)), cx));
    rows(cx, "the sorted page", (200, true));
    assert_eq!(first(cx), ["6", "player 6", "6"]);

    view.update(cx, |v, cx| {
        v.push_filter(
            savoia_core::data_query::Filter {
                column: savoia_core::data_query::Source::Base("name".into()),
                op: savoia_core::data_query::Op::Contains,
                value: "R 44".into(),
            },
            cx,
        )
    });
    rows(cx, "the filtered rows", (11, false));
    assert!(view.read_with(cx, |v, _| v.error().is_none()));

    let (_, other) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| DataView::new(ds.clone(), node("postgres"), Vec::new(), window, cx))
            })
        })
        .expect("window");
    assert!(other.read_with(cx, |v, _| v.query().is_none()));
}

/// Reads `sql`'s rows as text, on the source's session.
fn fetch(session: Arc<Session>, sql: &str) -> Vec<Vec<Option<String>>> {
    let sql = sql.to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    drop(crate::runtime::spawn(async move {
        drop(tx.send(session.fetch(sql).await));
    }));
    let (_, rows) = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("fetch timed out")
        .expect("fetch failed");
    rows.into_iter()
        .map(|r| r.iter().map(|c| c.as_deref().map(str::to_owned)).collect())
        .collect()
}

/// Edits, a new row and a deletion commit in one transaction; a change
/// that no longer matches its row rolls everything back and stays pending.
async fn data_view_edits_and_commits(url: &str, cx: &mut TestAppContext) {
    let (ds, _) = save_and_connect(cx, url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    let session = session_of(cx, &ds).expect("connected");
    // MySQL's test user can't create databases, so the table goes in `savoia`.
    let mysql = url.starts_with("mysql");
    let (schema, table) = if mysql {
        ("savoia", "savoia.it_edit_players")
    } else {
        ("it_edit", "it_edit.players")
    };
    if !mysql {
        exec(
            session.clone(),
            "DROP SCHEMA IF EXISTS it_edit CASCADE; CREATE SCHEMA it_edit;",
        );
    }
    exec(
        session.clone(),
        &format!(
            "DROP TABLE IF EXISTS {table};
             CREATE TABLE {table} (id int PRIMARY KEY, name varchar(40), goals int DEFAULT 0);
             INSERT INTO {table} VALUES (1, 'Baggio', 1), (2, 'Rivera', 2), (3, 'Riva', 3);"
        ),
    );
    ds.update(cx, |ds, cx| ds.refresh(id, cx));
    wait_until(cx, "the refresh", |cx| refresh_state(cx, &ds).is_none());
    let node = NodeRef {
        connection: id,
        database: "savoia".into(),
        schema: schema.into(),
        table: Some(table.rsplit('.').next().unwrap().into()),
    };
    let (window, view) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| DataView::new(ds.clone(), node, Vec::new(), window, cx))
            })
        })
        .expect("window");
    let rows = |cx: &mut TestAppContext, want: usize| {
        wait_until(cx, "the rows", |cx| {
            view.read_with(cx, |v, cx| v.rows(cx)) == Some((want, false))
        });
    };
    rows(cx, 3);
    let grid = view.read_with(cx, |v, _| v.grid().unwrap());
    grid.update(cx, |g, _| {
        let rows = g.delegate_mut();
        rows.set(0, 1, Some("Totti".into()));
        rows.set(1, 2, None);
        rows.add_row();
        rows.set(0, 0, Some("4".into()));
        rows.set(0, 1, Some("Del Piero".into()));
        // Loaded rows now start after the new one.
        rows.toggle_delete(3);
    });
    let review = view
        .read_with(cx, |v, cx| v.review_sql(cx))
        .expect("changes");
    assert_eq!(review.lines().count(), 6, "{review}");
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| v.commit(window, cx))
    })
    .unwrap();
    wait_until(cx, "the commit", |cx| {
        view.read_with(cx, |v, cx| !v.is_committing() && v.review_sql(cx).is_none())
    });
    rows(cx, 3);
    assert_eq!(
        fetch(
            session.clone(),
            &format!("SELECT id, name, goals FROM {table} ORDER BY id")
        ),
        [
            vec![Some("1".into()), Some("Totti".into()), Some("1".into())],
            vec![Some("2".into()), Some("Rivera".into()), None],
            vec![Some("4".into()), Some("Del Piero".into()), Some("0".into())],
        ]
    );

    // Someone else deletes row 2 before our edit to it commits.
    grid.update(cx, |g, _| {
        g.delegate_mut().set(1, 1, Some("Mazzola".into()))
    });
    grid.update(cx, |g, _| {
        g.delegate_mut().set(0, 1, Some("Francesco".into()))
    });
    exec(
        session.clone(),
        &format!("DELETE FROM {table} WHERE id = 2"),
    );
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| v.commit(window, cx))
    })
    .unwrap();
    wait_until(cx, "the rollback", |cx| {
        view.read_with(cx, |v, _| {
            v.error().is_some_and(|e| e.starts_with("Rolled back"))
        })
    });
    assert!(
        view.read_with(cx, |v, cx| v.review_sql(cx)).is_some(),
        "still pending"
    );
    assert_eq!(
        fetch(
            session.clone(),
            &format!("SELECT name FROM {table} WHERE id = 1")
        ),
        [vec![Some("Totti".to_string())]],
        "the other change was rolled back too"
    );
    exec(session, &format!("DROP TABLE {table}"));
}

#[gpui_kit::test]
async fn postgres_data_view_edits_and_commits(cx: &mut TestAppContext) {
    if let Ok(url) = std::env::var("SAVOIA_PG_URL") {
        data_view_edits_and_commits(&url, cx).await;
    }
}

#[gpui_kit::test]
async fn mysql_data_view_edits_and_commits(cx: &mut TestAppContext) {
    if let Ok(url) = std::env::var("SAVOIA_MYSQL_URL") {
        data_view_edits_and_commits(&url, cx).await;
    }
}

const REL_SCHEMA: &str = "DROP SCHEMA IF EXISTS it_rel CASCADE;
    CREATE SCHEMA it_rel;
    CREATE TABLE it_rel.countries (code char(2) PRIMARY KEY, name text NOT NULL);
    CREATE TABLE it_rel.customers (
      id int PRIMARY KEY, name text NOT NULL,
      country_code char(2) REFERENCES it_rel.countries (code));
    CREATE TABLE it_rel.orders (
      id int PRIMARY KEY, customer_id int REFERENCES it_rel.customers (id), total numeric);
    CREATE TABLE it_rel.order_items (
      order_id int REFERENCES it_rel.orders (id), qty int, PRIMARY KEY (order_id, qty));
    INSERT INTO it_rel.countries VALUES ('IT', 'Italy'), ('FR', 'France');
    INSERT INTO it_rel.customers VALUES (1, 'Juve', 'IT'), (2, 'PSG', 'FR'), (3, 'Toro', 'IT');
    INSERT INTO it_rel.orders VALUES (10, 1, 5), (11, 2, 7), (12, NULL, 1), (13, 3, 2);
    INSERT INTO it_rel.order_items VALUES (10, 1), (10, 2), (11, 5), (13, 4);";

/// Lookup and summary columns keep one row per order; filters and sorts
/// work on them; the row menu opens parents and children; FK cells pick
/// values by name.
#[gpui_kit::test]
async fn postgres_data_view_follows_relationships(cx: &mut TestAppContext) {
    use savoia_core::data_query::{Agg, Children, Filter, Hop, Op, Source};

    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, _) = save_and_connect(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    let session = session_of(cx, &ds).expect("connected");
    exec(session.clone(), REL_SCHEMA);
    let node = NodeRef {
        connection: id,
        database: "savoia".into(),
        schema: "it_rel".into(),
        table: Some("orders".into()),
    };
    let (window, view) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| DataView::new(ds.clone(), node, Vec::new(), window, cx))
            })
        })
        .expect("window");
    let opened = Rc::new(RefCell::new(Vec::new()));
    cx.update(|cx| {
        let opened = opened.clone();
        cx.subscribe(&view, move |_, event, _| {
            if let crate::data_view::DataViewEvent::Open { node, filters } = event {
                opened
                    .borrow_mut()
                    .push((node.table.clone(), filters.clone()));
            }
        })
        .detach();
    });
    wait_until(cx, "the rows and relations", |cx| {
        view.read_with(cx, |v, cx| {
            v.rows(cx) == Some((4, false)) && v.links().len() == 2
        })
    });

    let customer = Hop {
        columns: vec!["customer_id".into()],
        schema: "it_rel".into(),
        table: "customers".into(),
        ref_columns: vec!["id".into()],
    };
    let country = Hop {
        columns: vec!["country_code".into()],
        schema: "it_rel".into(),
        table: "countries".into(),
        ref_columns: vec!["code".into()],
    };
    let items = Children {
        schema: "it_rel".into(),
        table: "order_items".into(),
        columns: vec!["order_id".into()],
        ref_columns: vec!["id".into()],
    };
    let country_name = Source::Lookup {
        path: vec![customer.clone(), country],
        column: "name".into(),
    };
    let item_count = Source::Summary {
        children: items.clone(),
        agg: Agg::Count,
        column: None,
    };
    for source in [
        Source::Lookup {
            path: vec![customer],
            column: "name".into(),
        },
        country_name.clone(),
        item_count.clone(),
        Source::Summary {
            children: items,
            agg: Agg::Sum,
            column: Some("qty".into()),
        },
    ] {
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |v, cx| v.toggle_column(source, window, cx))
        })
        .unwrap();
    }
    wait_until(cx, "the related columns", |cx| {
        view.read_with(cx, |v, cx| v.rows(cx) == Some((4, false)))
    });
    let table = |cx: &mut TestAppContext| {
        view.read_with(cx, |v, cx| {
            let grid = v.grid().unwrap();
            let rows = grid.read(cx).delegate();
            (0..rows.len())
                .map(|i| {
                    rows.row(i)
                        .iter()
                        .map(|c| c.as_deref().unwrap_or("NULL").to_owned())
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        table(cx),
        [
            "10|1|5|Juve|Italy|2|3",
            "11|2|7|PSG|France|1|5",
            "12|NULL|1|NULL|NULL|0|NULL",
            "13|3|2|Toro|Italy|1|4",
        ],
        "one row per order, orders without a customer kept"
    );

    // Filter on a lookup, sort on a summary.
    view.update(cx, |v, cx| {
        v.push_filter(
            Filter {
                column: country_name,
                op: Op::Eq,
                value: "Italy".into(),
            },
            cx,
        );
        v.sort_by(Some((5, true)), cx);
    });
    wait_until(cx, "the filtered rows", |cx| {
        view.read_with(cx, |v, cx| v.rows(cx) == Some((2, false)))
    });
    assert_eq!(
        table(cx),
        ["10|1|5|Juve|Italy|2|3", "13|3|2|Toro|Italy|1|4"]
    );

    // Row menu: the order's customer, then its items.
    view.update(cx, |v, cx| {
        v.open_related(0, 0, cx);
        v.open_related(0, 1, cx);
    });
    let opened = opened.borrow().clone();
    assert_eq!(
        opened,
        [
            (
                Some("customers".to_string()),
                vec![Filter {
                    column: Source::Base("id".into()),
                    op: Op::Eq,
                    value: "1".into()
                }]
            ),
            (
                Some("order_items".to_string()),
                vec![Filter {
                    column: Source::Base("order_id".into()),
                    op: Op::Eq,
                    value: "10".into()
                }]
            ),
        ]
    );

    // The customer picker searches by name and hands back the id.
    let picked = Rc::new(RefCell::new(None));
    let picker = cx
        .update_window(window, |_, window, cx| {
            let picked = picked.clone();
            cx.new(|cx| {
                crate::data_pickers::RowPicker::new(
                    session.clone(),
                    savoia_core::Engine::Postgres,
                    "it_rel".into(),
                    "customers".into(),
                    "id".into(),
                    "name".into(),
                    Rc::new(move |value, _, _| *picked.borrow_mut() = Some(value)),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    wait_until(cx, "the picker rows", |cx| {
        picker.read_with(cx, |p, _| p.rows().len() == 3)
    });
    cx.update_window(window, |_, window, cx| {
        picker.update(cx, |p, cx| p.pick(1, window, cx))
    })
    .unwrap();
    assert_eq!(
        *picked.borrow(),
        Some(Some("2".to_string())),
        "PSG, by name order"
    );
}

/// "Join another table…" preselects the likely column pair and warns when
/// the target column isn't unique.
#[gpui_kit::test]
async fn join_dialog_suggests_a_pair(cx: &mut TestAppContext) {
    use savoia_core::{ColumnInfo, TableInfo, TableKind};

    let (_, _, window) = mount(cx);
    let table = |name: &str, columns: &[&str], pk: &str| {
        let mut t = TableInfo::new(name, TableKind::Table);
        t.columns = columns
            .iter()
            .map(|c| ColumnInfo {
                name: (*c).into(),
                data_type: "integer".into(),
                nullable: true,
                default: None,
            })
            .collect();
        t.primary_key = vec![pk.into()];
        Arc::new(t)
    };
    let orders = table("orders", &["id", "region", "customer_id"], "id");
    let tables = vec![
        (
            "shop".to_string(),
            table("customers", &["id", "name"], "id"),
        ),
        (
            "shop".to_string(),
            table("regions", &["code", "region"], "code"),
        ),
    ];
    let dialog = cx
        .update_window(window, |_, window, cx| {
            cx.new(|cx| {
                crate::data_pickers::JoinDialog::new(
                    orders,
                    tables,
                    Rc::new(|_, _, _| {}),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    let (hop, unique) = dialog.read_with(cx, |d, cx| d.hop(cx)).expect("a pair");
    assert_eq!(
        (hop.columns, hop.table, hop.ref_columns, unique),
        (
            vec!["customer_id".to_string()],
            "customers".to_string(),
            vec!["id".to_string()],
            true
        )
    );
}

/// Summarize groups the view's rows, lookups included, with aggregates;
/// "Back to rows" returns to the rows.
#[gpui_kit::test]
async fn postgres_data_view_summarizes(cx: &mut TestAppContext) {
    use savoia_core::data_query::{Agg, Hop, Source, Summary};

    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, _) = save_and_connect(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    exec(
        session_of(cx, &ds).expect("connected"),
        &REL_SCHEMA.replace("it_rel", "it_sum"),
    );
    let node = NodeRef {
        connection: id,
        database: "savoia".into(),
        schema: "it_sum".into(),
        table: Some("orders".into()),
    };
    let (window, view) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| DataView::new(ds.clone(), node, Vec::new(), window, cx))
            })
        })
        .expect("window");
    wait_until(cx, "the rows", |cx| {
        view.read_with(cx, |v, cx| v.rows(cx) == Some((4, false)))
    });
    let country = Source::Lookup {
        path: vec![
            Hop {
                columns: vec!["customer_id".into()],
                schema: "it_sum".into(),
                table: "customers".into(),
                ref_columns: vec!["id".into()],
            },
            Hop {
                columns: vec!["country_code".into()],
                schema: "it_sum".into(),
                table: "countries".into(),
                ref_columns: vec!["code".into()],
            },
        ],
        column: "name".into(),
    };
    let summary = Summary {
        by: vec![country],
        aggregates: vec![
            (Agg::Count, None),
            (Agg::Sum, Some(Source::Base("total".into()))),
        ],
        sort: Some((1, true)),
    };
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| v.summarize(Some(summary), window, cx))
    })
    .unwrap();
    wait_until(cx, "the groups", |cx| {
        view.read_with(cx, |v, cx| v.rows(cx) == Some((3, false)))
    });
    let groups = view.read_with(cx, |v, cx| {
        let grid = v.grid().unwrap();
        let rows = grid.read(cx).delegate();
        (0..rows.len())
            .map(|i| {
                rows.row(i)
                    .iter()
                    .map(|c| c.as_deref().unwrap_or("NULL").to_owned())
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(groups, ["Italy|2|7", "France|1|7", "NULL|1|1"]);

    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| v.summarize(None, window, cx))
    })
    .unwrap();
    wait_until(cx, "the rows again", |cx| {
        view.read_with(cx, |v, cx| v.rows(cx) == Some((4, false)))
    });
}

/// The column picker lists the table's relationships once they load, and
/// draws both panes.
#[gpui_kit::test]
async fn postgres_column_picker_lists_relationships(cx: &mut TestAppContext) {
    use crate::relations::Kind;

    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, _) = save_and_connect(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    exec(
        session_of(cx, &ds).expect("connected"),
        &REL_SCHEMA.replace("it_rel", "it_pick"),
    );
    let node = NodeRef {
        connection: id,
        database: "savoia".into(),
        schema: "it_pick".into(),
        table: Some("orders".into()),
    };
    let (window, view) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| DataView::new(ds.clone(), node, Vec::new(), window, cx))
            })
        })
        .expect("window");
    wait_until(cx, "the relationships", |cx| {
        view.read_with(cx, |v, _| !v.picker_data().loading && v.links().len() == 2)
    });
    let kinds: Vec<(Kind, String)> = view.read_with(cx, |v, _| {
        v.picker_data()
            .sections
            .into_iter()
            .map(|s| (s.kind, s.title))
            .collect()
    });
    assert_eq!(
        kinds,
        [
            (Kind::Own, "orders".to_string()),
            (Kind::BelongsTo, "customer".to_string()),
            (Kind::BelongsTo, "customer › countries".to_string()),
            (Kind::HasMany, "order_items".to_string()),
        ]
    );
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| v.open_columns(window, cx));
        window.render_frame(cx);
    })
    .unwrap();
}

/// Opens the dump or import wizard on `node` and waits for its tools and objects.
fn open_wizard(
    cx: &mut TestAppContext,
    ds: &Entity<DataSources>,
    node: NodeRef,
    direction: Direction,
) -> (AnyWindowHandle, Entity<TransferView>) {
    let (window, view) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| TransferView::new(ds.clone(), node, direction, window, cx))
            })
        })
        .expect("window");
    wait_until(cx, "the wizard to load", |cx| {
        view.read_with(cx, |v, _| v.ready_for_test())
    });
    (window, view)
}

fn set_path(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<TransferView>,
    path: &str,
) {
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| v.set_path_for_test(path, window, cx));
    })
    .unwrap();
}

/// Clicks Next/Start until the job ends; returns how it ended and its log.
fn run_wizard(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<TransferView>,
    steps: usize,
) -> (crate::transfer::JobEnd, Vec<String>) {
    for _ in 0..steps {
        click(cx, window, "transfer-next");
    }
    for _ in 0..600 {
        cx.run_until_parked();
        if view.read_with(cx, |v, _| v.state_for_test().0.is_some()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let (end, log) = view.read_with(cx, |v, _| v.state_for_test());
    let end = end.unwrap_or_else(|| panic!("the job didn't end; log:\n{}", log.join("\n")));
    (end, log)
}

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "savoia-ui-{}-{name}",
        savoia_core::ConnectionId::new()
    ))
}

/// The export wizard dumps a schema with pg_dump, then with the built-in
/// engine; the import wizard maps a CSV's columns by name and loads it.
#[gpui_kit::test]
async fn postgres_wizard_exports_and_imports(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, _) = save_and_connect(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    exec(
        session_of(cx, &ds).expect("connected"),
        "DROP SCHEMA IF EXISTS it_wizard CASCADE;
         CREATE SCHEMA it_wizard;
         CREATE TABLE it_wizard.people (id int PRIMARY KEY, name text, email text);
         INSERT INTO it_wizard.people VALUES (1, 'Ada', NULL), (2, 'Grace', '');",
    );
    let schema = NodeRef {
        connection: id,
        database: "savoia".into(),
        schema: "it_wizard".into(),
        table: None,
    };

    // pg_dump, when installed: Objects → Options → Destination → Export.
    let tool = savoia_transfer::tools::detect(
        savoia_transfer::tools::Tool::PgDump,
        &savoia_transfer::tools::ToolSearch::system(None),
    );
    if tool.is_some() {
        let (window, view) = open_wizard(cx, &ds, schema.clone(), Direction::Export);
        let out = temp_path("tool.sql");
        set_path(cx, window, &view, &out.display().to_string());
        let (end, log) = run_wizard(cx, window, &view, 3);
        assert!(
            matches!(end, crate::transfer::JobEnd::Succeeded(_)),
            "{end:?}\n{}",
            log.join("\n")
        );
        let dump = std::fs::read_to_string(&out).unwrap();
        assert!(dump.contains("CREATE TABLE it_wizard.people"), "{dump}");
        assert!(
            log.iter().any(|l| l.starts_with("$ ")),
            "the command line is logged"
        );
    }

    // Built-in engine, picked on the options step.
    let (window, view) = open_wizard(cx, &ds, schema.clone(), Direction::Export);
    let out = temp_path("builtin.sql");
    set_path(cx, window, &view, &out.display().to_string());
    click(cx, window, "transfer-next");
    click(cx, window, "use-built-in");
    let (end, log) = run_wizard(cx, window, &view, 2);
    assert!(
        matches!(end, crate::transfer::JobEnd::Succeeded(_)),
        "{end:?}\n{}",
        log.join("\n")
    );
    assert!(
        std::fs::read_to_string(&out)
            .unwrap()
            .contains("INSERT INTO \"it_wizard\".\"people\"")
    );

    // CSV import into the table, with its columns in another order.
    let csv = temp_path("people.csv");
    std::fs::write(
        &csv,
        "EMAIL,id,name,ignored\nlin@example.com,3,Lin,x\n,4,Mo,y\n",
    )
    .unwrap();
    let table = NodeRef {
        table: Some("people".into()),
        ..schema
    };
    let (window, view) = open_wizard(cx, &ds, table, Direction::Import);
    set_path(cx, window, &view, &csv.display().to_string());
    wait_until(cx, "the column mapping", |cx| {
        view.read_with(cx, |v, cx| v.mapping_for_test(cx).len() == 4)
    });
    // CSV columns → table columns (id=1, name=2, email=3; 0 = skip).
    assert_eq!(
        view.read_with(cx, |v, cx| v.mapping_for_test(cx)),
        [3, 1, 2, 0]
    );
    let (end, log) = run_wizard(cx, window, &view, 2);
    assert!(
        matches!(end, crate::transfer::JobEnd::Succeeded(_)),
        "{end:?}\n{}",
        log.join("\n")
    );
    let rows = fetch(
        session_of(cx, &ds).unwrap(),
        "SELECT id::text, name, email FROM it_wizard.people ORDER BY id",
    );
    assert_eq!(
        rows[2],
        [
            Some("3".into()),
            Some("Lin".into()),
            Some("lin@example.com".into())
        ]
    );
    assert_eq!(rows[3], [Some("4".into()), Some("Mo".into()), None]);
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// The MySQL export wizard dumps one table with mysqldump (or the built-in
/// engine without it), from the table menu's starting point.
#[gpui_kit::test]
async fn mysql_wizard_exports_a_table(cx: &mut TestAppContext) {
    let Ok(url) = std::env::var("SAVOIA_MYSQL_URL") else {
        return;
    };
    let (ds, _) = save_and_connect(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    exec(
        session_of(cx, &ds).expect("connected"),
        "DROP TABLE IF EXISTS it_wizard_people;
         CREATE TABLE it_wizard_people (id int PRIMARY KEY, name text);
         INSERT INTO it_wizard_people VALUES (1, 'Ada');",
    );
    let node = NodeRef {
        connection: id,
        database: "savoia".into(),
        schema: "savoia".into(),
        table: Some("it_wizard_people".into()),
    };
    let (window, view) = open_wizard(cx, &ds, node, Direction::Export);
    let out = temp_path("my.sql");
    set_path(cx, window, &view, &out.display().to_string());
    let (end, log) = run_wizard(cx, window, &view, 3);
    assert!(
        matches!(end, crate::transfer::JobEnd::Succeeded(_)),
        "{end:?}\n{}",
        log.join("\n")
    );
    let dump = std::fs::read_to_string(&out).unwrap();
    assert!(dump.contains("it_wizard_people"), "{dump}");
    assert!(!dump.contains("it_parts"), "only the picked table: {dump}");
}

fn mount_workspace(
    cx: &mut TestAppContext,
) -> (Entity<DataSources>, Entity<Workspace>, AnyWindowHandle) {
    let (ds, _, _) = mount(cx);
    cx.update(|cx| {
        crate::console::init(cx);
        crate::commands::init(cx);
    });
    let (window, workspace) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::with_data_sources(ds.clone(), window, cx))
            })
        })
        .expect("window");
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    (ds, workspace, window)
}

/// The palette runs the chosen command on the workspace, once.
#[gpui_kit::test]
async fn the_palette_runs_a_command_once(cx: &mut TestAppContext) {
    let (_, workspace, window) = mount_workspace(cx);
    let consoles = |cx: &mut TestAppContext| workspace.read_with(cx, |w, _| w.consoles().count());
    assert_eq!(consoles(cx), 1);

    // Keys only reach elements of a drawn frame.
    let frame = |cx: &mut TestAppContext| {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
    };
    cx.simulate_keystrokes(window, "secondary-shift-p");
    frame(cx);
    cx.simulate_input(window, "new console");
    frame(cx);
    cx.simulate_keystrokes(window, "enter");
    frame(cx);

    assert_eq!(consoles(cx), 2);
}

/// ⌘, opens Settings, and a second ⌘, brings the same tab forward.
#[gpui_kit::test]
async fn settings_opens_once(cx: &mut TestAppContext) {
    let (_, workspace, window) = mount_workspace(cx);
    cx.simulate_keystrokes(window, "secondary-,");
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.simulate_keystrokes(window, "secondary-,");
    cx.run_until_parked();
    assert_eq!(workspace.read_with(cx, |w, _| w.settings_tabs()), 1);
}

/// Theme commands switch the palette at once and are remembered.
#[gpui_kit::test]
async fn theme_commands_switch_and_persist(cx: &mut TestAppContext) {
    use gpui_kit::component::ActiveTheme as _;

    let (ds, _, window) = mount_workspace(cx);
    let dispatch = |cx: &mut TestAppContext, action: Box<dyn gpui_kit::Action>| {
        cx.update_window(window, |_, window, cx| window.dispatch_action(action, cx))
            .unwrap();
        cx.run_until_parked();
    };

    dispatch(cx, Box::new(crate::commands::UseLightAppearance));
    assert!(cx.update(|cx| !cx.theme().is_dark()));
    assert_eq!(
        ds.read_with(cx, |ds, _| ds.setting("ui.appearance"))
            .as_deref(),
        Some("light")
    );

    dispatch(cx, Box::new(crate::commands::UseDarkAppearance));
    assert!(cx.update(|cx| cx.theme().is_dark()));
    assert_eq!(
        ds.read_with(cx, |ds, _| ds.setting("ui.appearance"))
            .as_deref(),
        Some("dark")
    );
}

/// Double-clicking a table in the explorer opens its data, as Open data in
/// its menu does, and leaves the row folded as it was.
#[gpui_kit::test]
async fn postgres_double_click_opens_a_tables_data(cx: &mut TestAppContext) {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Bounds, WindowBounds, point, px, size};

    let Ok(url) = std::env::var("SAVOIA_PG_URL") else {
        return;
    };
    let (ds, _) = save_and_connect(cx, &url);
    let id = ds.read_with(cx, |ds, _| ds.connections()[0].id);
    // A schema of its own: tests run at once, on one database.
    exec(
        session_of(cx, &ds).expect("connected"),
        "DROP SCHEMA IF EXISTS it_click CASCADE;
         CREATE SCHEMA it_click;
         CREATE TABLE it_click.orders (id int PRIMARY KEY, placed_at timestamptz);",
    );
    ds.update(cx, |ds, cx| ds.refresh(id, cx));
    wait_until(cx, "the refresh", |cx| refresh_state(cx, &ds).is_none());
    cx.update(|cx| {
        crate::console::init(cx);
        crate::commands::init(cx);
    });
    // Tall enough to show every row the test opens.
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(0.), px(0.)),
            size(px(1200.), px(4000.)),
        ))),
        ..WindowOptions::default()
    };
    let (window, workspace) = cx
        .update(|cx| {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::with_data_sources(ds.clone(), window, cx))
            })
        })
        .expect("window");
    let explorer = workspace.read_with(cx, |w, _| w.explorer().clone());
    explorer.update(cx, |e, cx| e.expand(&["it_click"], cx));
    settled_labels(cx, &explorer);
    explorer.update(cx, |e, cx| e.expand(&["it_click", "tables"], cx));
    let before = settled_labels(cx, &explorer);

    let row = gpui_kit::SharedString::from(format!("table:{id}/savoia/it_click/orders"));
    cx.update_window(window, |_, window, cx| window.double_click(row, cx))
        .unwrap();
    let node = NodeRef {
        connection: id,
        database: "savoia".into(),
        schema: "it_click".into(),
        table: Some("orders".into()),
    };
    let shown = workspace.read_with(cx, |w, cx| {
        w.data_views()
            .map(|view| view.read(cx).shows(&node))
            .collect::<Vec<_>>()
    });
    assert_eq!(shown, [true], "one data view, of it_click.orders");
    assert_eq!(settled_labels(cx, &explorer), before);
}
