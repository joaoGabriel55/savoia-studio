//! Database Explorer: saved data sources → databases → schemas → object groups.
//!
//! Tree ids are `<kind>:<connection id>[/<path>]`; the kind picks the icon and
//! the connection id routes toolbar and context-menu actions.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::tree::{TreeEntry, TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use savoia_core::{ConnectionConfig, ConnectionId, DatabaseNode, ObjectCounts, SchemaNode};
use savoia_tunnel::HostKeyPolicy;

use crate::connection_form::ConnectionForm;
use crate::data_sources::{
    DataSources, DataSourcesEvent, LoadState, LoadTarget, RefreshState, SourceState,
};
use crate::session::Session;
use crate::table_menu::{self, ObjectRef, Origin, TableAction};
use crate::theme::{self, BandDisabled as _};

/// Per-source decoration for the tree rows.
#[derive(Clone)]
struct SourceMeta {
    color: Option<u32>,
    status: SourceStatus,
}

#[derive(Clone)]
enum SourceStatus {
    Idle,
    /// Connecting or refreshing; the text says which.
    Pending(&'static str),
    Connected(SharedString),
    Failed,
}

pub struct Explorer {
    data_sources: Entity<DataSources>,
    tree: Entity<TreeState>,
    /// Last items handed to the tree. Items share expansion state with the
    /// tree, so this is how expansion survives a rebuild.
    items: Vec<TreeItem>,
    meta: Rc<HashMap<ConnectionId, SourceMeta>>,
    /// What schema-level rows refer to, by row id.
    nodes: Rc<HashMap<SharedString, NodeRef>>,
    /// Muted text after a row's label, by row id.
    suffixes: Rc<HashMap<SharedString, SharedString>>,
    /// Rows whose children load when they are expanded, with what loads them.
    unloaded: HashMap<SharedString, (ConnectionId, LoadTarget)>,
    /// Sessions whose current database was already expanded automatically.
    auto_opened: HashSet<ConnectionId>,
    _subscriptions: Vec<Subscription>,
}

/// The connection a tree id belongs to.
fn connection_of(id: &str) -> Option<ConnectionId> {
    let rest = id.split_once(':')?.1;
    let uuid = rest.split('/').next()?;
    uuid.parse().ok().map(ConnectionId)
}

fn kind_of(id: &str) -> &str {
    id.split_once(':').map_or(id, |(kind, _)| kind)
}

fn collect_expanded(items: &[TreeItem], out: &mut HashSet<SharedString>) {
    for item in items {
        if item.is_expanded() {
            out.insert(item.id.clone());
        }
        collect_expanded(&item.children, out);
    }
}

/// The schema (and table) a tree row belongs to: what it loads on expand and
/// what "Show diagram" opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRef {
    pub connection: ConnectionId,
    pub database: String,
    pub schema: String,
    pub table: Option<String>,
}

impl NodeRef {
    fn with_table(&self, table: &str) -> Self {
        Self {
            table: Some(table.to_owned()),
            ..self.clone()
        }
    }

    /// What expanding this row loads, unless it is loaded already.
    fn target(&self) -> LoadTarget {
        let (database, schema) = (self.database.clone(), self.schema.clone());
        match &self.table {
            Some(table) => LoadTarget::Table {
                database,
                schema,
                table: table.clone(),
            },
            None => LoadTarget::Objects { database, schema },
        }
    }
}

pub enum ExplorerEvent {
    ShowDiagram(NodeRef),
    /// Open the structure of a table or view; the node has a table.
    ShowStructure(NodeRef),
    /// Browse a table's or view's rows; the node has a table.
    OpenData(NodeRef),
    /// Open the dump or import wizard on the node's schema (and table, if any).
    Transfer(NodeRef, crate::transfer::Direction),
    /// SQL for a query console on `connection`.
    Sql {
        connection: ConnectionId,
        sql: String,
        /// Run it now, or only place it in the editor.
        run: bool,
        /// Refresh the catalog once the statement has been sent.
        refresh: bool,
    },
}

/// Builds the tree rows of one connected source, noting what each row
/// refers to and which rows still need a load.
struct Builder<'a> {
    ds: &'a DataSources,
    session: &'a Session,
    nodes: &'a mut HashMap<SharedString, NodeRef>,
    /// Muted text after a row's label: counts, column types.
    suffixes: &'a mut HashMap<SharedString, SharedString>,
    /// Rows whose children aren't loaded, with what loads them.
    unloaded: &'a mut HashMap<SharedString, (ConnectionId, LoadTarget)>,
}

impl Builder<'_> {
    fn item(&mut self, id: String, label: impl Into<SharedString>, node: &NodeRef) -> TreeItem {
        let item = TreeItem::new(id, label);
        self.nodes.insert(item.id.clone(), node.clone());
        item
    }

    /// The one child of a row whose children are still loading.
    fn pending(
        &mut self,
        parent: &TreeItem,
        connection: ConnectionId,
        target: LoadTarget,
    ) -> TreeItem {
        let text = match self.ds.load_state(connection, &target) {
            Some(LoadState::Waiting) => "waiting for the running query…".to_string(),
            Some(LoadState::Failed(err)) => format!("failed: {err}"),
            Some(LoadState::Loading) | None => "loading…".to_string(),
        };
        self.unloaded
            .insert(parent.id.clone(), (connection, target));
        TreeItem::new(format!("info:{}", parent.id), text)
    }

    /// The object groups of a schema, or a pending row until they load.
    fn schema_children(
        &mut self,
        parent: &TreeItem,
        prefix: &str,
        schema: &SchemaNode,
        node: &NodeRef,
    ) -> Vec<TreeItem> {
        self.suffixes
            .insert(parent.id.clone(), count_summary(&schema.counts).into());
        let Some(objects) = &schema.objects else {
            return vec![self.pending(parent, node.connection, node.target())];
        };
        let mut out = vec![
            self.group(prefix, "tables", "table", &objects.tables, node),
            self.group(prefix, "views", "view", &objects.views, node),
        ];
        if !objects.functions.is_empty() {
            out.push(self.group(prefix, "functions", "function", &objects.functions, node));
        }
        if !objects.sequences.is_empty() {
            out.push(self.group(prefix, "sequences", "sequence", &objects.sequences, node));
        }
        out
    }

    fn group(
        &mut self,
        prefix: &str,
        label: &str,
        kind: &str,
        names: &[String],
        node: &NodeRef,
    ) -> TreeItem {
        let children: Vec<TreeItem> = names
            .iter()
            .map(|name| {
                let id = format!("{kind}:{prefix}/{name}");
                if matches!(kind, "table" | "view") {
                    let node = node.with_table(name);
                    let item = self.item(id, name.clone(), &node);
                    let children = self.table_children(&item, &node);
                    item.children(children)
                } else {
                    TreeItem::new(id, name.clone())
                }
            })
            .collect();
        let group = self.item(format!("group:{prefix}/{label}"), label.to_owned(), node);
        self.suffixes
            .insert(group.id.clone(), names.len().to_string().into());
        group.children(children)
    }

    /// Columns, then foreign keys and indexes, or a pending row until they load.
    fn table_children(&mut self, parent: &TreeItem, node: &NodeRef) -> Vec<TreeItem> {
        let table = node.table.as_deref().unwrap_or_default();
        let Some(info) = self.session.table(&node.database, &node.schema, table) else {
            return vec![self.pending(parent, node.connection, node.target())];
        };
        let prefix = &parent.id[parent.id.find(':').map_or(0, |i| i + 1)..];
        let mut out: Vec<TreeItem> = info
            .columns
            .iter()
            .map(|c| {
                let kind = if info.is_key_column(&c.name) {
                    "pkcolumn"
                } else if info.is_foreign_column(&c.name) {
                    "fkcolumn"
                } else {
                    "column"
                };
                let item = TreeItem::new(format!("{kind}:{prefix}/{}", c.name), c.name.clone());
                let mut suffix = c.data_type.clone();
                if !c.nullable {
                    suffix.push_str(" not null");
                }
                self.suffixes.insert(item.id.clone(), suffix.into());
                item
            })
            .collect();
        let mut sub_group = |label: &str, rows: Vec<(String, String)>, kind: &str| {
            let group = TreeItem::new(format!("group:{prefix}/{label}"), label.to_owned());
            self.suffixes
                .insert(group.id.clone(), rows.len().to_string().into());
            group.children(rows.into_iter().map(|(name, suffix)| {
                let item = TreeItem::new(format!("{kind}:{prefix}/{name}"), name);
                self.suffixes.insert(item.id.clone(), suffix.into());
                item
            }))
        };
        if !info.foreign_keys.is_empty() {
            let rows = info
                .foreign_keys
                .iter()
                .map(|fk| {
                    let target = format!(
                        "({}) → {}({})",
                        fk.columns.join(", "),
                        fk.ref_table,
                        fk.ref_columns.join(", ")
                    );
                    (fk.name.clone(), target)
                })
                .collect();
            out.push(sub_group("foreign keys", rows, "fk"));
        }
        if !info.indexes.is_empty() {
            let rows = info
                .indexes
                .iter()
                .map(|i| {
                    let mut text = format!("({})", i.columns.join(", "));
                    if i.primary {
                        text.push_str(" primary");
                    } else if i.unique {
                        text.push_str(" unique");
                    }
                    (i.name.clone(), text)
                })
                .collect();
            out.push(sub_group("indexes", rows, "index"));
        }
        out
    }

    fn database(&mut self, config: &ConnectionConfig, db: &DatabaseNode) -> TreeItem {
        let prefix = format!("{}/{}", config.id, db.name);
        let id = format!("database:{prefix}");
        if config.engine.has_schemas() {
            let item = TreeItem::new(id, db.name.clone());
            let Some(schemas) = &db.schemas else {
                // Another Postgres database: its schemas load on expand.
                let target = LoadTarget::Schemas {
                    database: db.name.clone(),
                };
                let pending = self.pending(&item, config.id, target);
                return item.children(vec![pending]);
            };
            let children: Vec<TreeItem> = schemas
                .iter()
                .map(|schema| {
                    let prefix = format!("{prefix}/{}", schema.name);
                    let node = NodeRef {
                        connection: config.id,
                        database: db.name.clone(),
                        schema: schema.name.clone(),
                        table: None,
                    };
                    let item = self.item(format!("schema:{prefix}"), schema.name.clone(), &node);
                    let children = self.schema_children(&item, &prefix, schema, &node);
                    item.children(children)
                })
                .collect();
            item.children(children)
        } else {
            // MySQL: the database is its own one schema.
            let Some(schema) = db.schemas.iter().flatten().next() else {
                return TreeItem::new(id, db.name.clone());
            };
            let node = NodeRef {
                connection: config.id,
                database: db.name.clone(),
                schema: schema.name.clone(),
                table: None,
            };
            let item = self.item(id, db.name.clone(), &node);
            let children = self.schema_children(&item, &prefix, schema, &node);
            item.children(children)
        }
    }
}

/// e.g. "12 tables · 3 views"; empty kinds are left out.
fn count_summary(counts: &ObjectCounts) -> String {
    let parts: Vec<String> = [
        (counts.tables, "table", "tables"),
        (counts.views, "view", "views"),
        (counts.functions, "function", "functions"),
        (counts.sequences, "sequence", "sequences"),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| format!("{n} {}", if n == 1 { one } else { many }))
    .collect();
    parts.join(" · ")
}

/// `public` if present, else the only schema.
fn default_schema(database: &TreeItem) -> Option<SharedString> {
    let schemas: Vec<_> = database
        .children
        .iter()
        .filter(|c| kind_of(&c.id) == "schema")
        .collect();
    schemas
        .iter()
        .find(|s| s.label.as_ref() == "public")
        .or(if schemas.len() == 1 {
            schemas.first()
        } else {
            None
        })
        .map(|s| s.id.clone())
}

impl EventEmitter<ExplorerEvent> for Explorer {}

impl Explorer {
    pub fn new(data_sources: Entity<DataSources>, cx: &mut Context<Self>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        let subscriptions = vec![
            cx.subscribe(&data_sources, |this, _, event, cx| {
                if let DataSourcesEvent::Changed = event {
                    this.rebuild(cx);
                }
            }),
            cx.observe(&tree, |_, _, cx| cx.notify()),
            cx.subscribe(&tree, |this, _, event, cx| {
                if let TreeEvent::Expanded(id) = event {
                    this.load(id, cx);
                }
            }),
        ];
        let mut this = Self {
            data_sources,
            tree,
            items: Vec::new(),
            meta: Rc::default(),
            nodes: Rc::default(),
            suffixes: Rc::default(),
            unloaded: HashMap::new(),
            auto_opened: HashSet::new(),
            _subscriptions: subscriptions,
        };
        this.rebuild(cx);
        this
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let mut expanded = HashSet::new();
        collect_expanded(&self.items, &mut expanded);

        let ds = self.data_sources.read(cx);
        let mut meta = HashMap::new();
        let (mut nodes, mut suffixes, mut unloaded) =
            (HashMap::new(), HashMap::new(), HashMap::new());
        let items: Vec<TreeItem> = ds
            .connections()
            .iter()
            .map(|config| {
                let source = TreeItem::new(format!("source:{}", config.id), config.display_name());
                let status = match ds.state(config.id) {
                    SourceState::Disconnected => SourceStatus::Idle,
                    SourceState::Connecting => SourceStatus::Pending("connecting…"),
                    SourceState::Failed(_) => SourceStatus::Failed,
                    SourceState::Connected(session) => match ds.refresh_state(config.id) {
                        Some(RefreshState::Waiting) => {
                            SourceStatus::Pending("waiting for the running query…")
                        }
                        Some(RefreshState::Loading) => SourceStatus::Pending("refreshing…"),
                        None => {
                            SourceStatus::Connected(session.catalog().server.version.clone().into())
                        }
                    },
                };
                meta.insert(
                    config.id,
                    SourceMeta {
                        color: config.color.map(|c| c.rgb()),
                        status,
                    },
                );
                match ds.state(config.id) {
                    SourceState::Connected(session) => {
                        // Once per session, open the path to the current database
                        // and its default schema, like DataGrip does.
                        let auto_open = self.auto_opened.insert(config.id);
                        let mut builder = Builder {
                            ds,
                            session,
                            nodes: &mut nodes,
                            suffixes: &mut suffixes,
                            unloaded: &mut unloaded,
                        };
                        source.children(session.catalog().databases.iter().map(|db| {
                            let item = builder.database(config, db);
                            if db.is_current && auto_open {
                                expanded.insert(format!("source:{}", config.id).into());
                                expanded.insert(item.id.clone());
                                if let Some(schema) = default_schema(&item) {
                                    expanded.insert(schema);
                                }
                            }
                            item
                        }))
                    }
                    _ => {
                        self.auto_opened.remove(&config.id);
                        source
                    }
                }
            })
            .collect();

        fn apply(items: Vec<TreeItem>, expanded: &HashSet<SharedString>) -> Vec<TreeItem> {
            items
                .into_iter()
                .map(|mut item| {
                    let children = apply(std::mem::take(&mut item.children), expanded);
                    let open = expanded.contains(&item.id);
                    item.children(children).expanded(open)
                })
                .collect()
        }
        let items = apply(items, &expanded);

        self.meta = Rc::new(meta);
        self.nodes = Rc::new(nodes);
        self.suffixes = Rc::new(suffixes);
        self.unloaded = unloaded;
        self.items = items.clone();
        // `set_items` clears the selection, and the console runs against the
        // selected source, so carry it over: the same row if it still exists,
        // else its data source.
        self.tree.update(cx, |tree, cx| {
            let selected = tree.selected_item().map(|item| item.id.clone());
            tree.set_items(items, cx);
            let Some(selected) = selected else { return };
            let source = connection_of(&selected).map(|id| format!("source:{id}"));
            for id in [Some(selected.to_string()), source].into_iter().flatten() {
                tree.set_selected_item(Some(&TreeItem::new(id, "")), cx);
                if tree.selected_index().is_some() {
                    break;
                }
            }
        });
        self.load_expanded(cx);
        cx.notify();
    }

    /// Starts the load of `id`'s children if they aren't loaded.
    fn load(&mut self, id: &SharedString, cx: &mut Context<Self>) {
        let Some((connection, target)) = self.unloaded.get(id).cloned() else {
            return;
        };
        // Not from inside a `DataSources` event, which rebuilds call us from.
        let data_sources = self.data_sources.clone();
        cx.defer(move |cx| data_sources.update(cx, |ds, cx| ds.load(connection, target, cx)));
    }

    /// Loads for rows left expanded with nothing loaded: auto-opened ones and
    /// ones that stayed open across a Refresh.
    fn load_expanded(&mut self, cx: &mut Context<Self>) {
        fn walk(items: &[TreeItem], out: &mut Vec<SharedString>) {
            for item in items.iter().filter(|i| i.is_expanded()) {
                out.push(item.id.clone());
                walk(&item.children, out);
            }
        }
        let mut open = Vec::new();
        walk(&self.items, &mut open);
        for id in open {
            self.load(&id, cx);
        }
    }

    /// What the selected row refers to, if it is in a schema.
    pub fn selected_node(&self, cx: &App) -> Option<NodeRef> {
        let item = self.tree.read(cx).selected_item()?;
        self.nodes.get(&item.id).cloned()
    }

    fn show_diagram(&mut self, node: NodeRef, cx: &mut Context<Self>) {
        cx.emit(ExplorerEvent::ShowDiagram(node));
    }

    /// The connection of the selected row, if any.
    pub fn selected_connection(&self, cx: &App) -> Option<ConnectionId> {
        self.tree
            .read(cx)
            .selected_item()
            .and_then(|item| connection_of(&item.id))
    }

    pub fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        open_form(self.data_sources.clone(), None, window, cx);
    }

    fn edit(&mut self, id: ConnectionId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.data_sources.read(cx).get(id).cloned() else {
            return;
        };
        let secrets = self.data_sources.read(cx).load_secrets(id);
        let data_sources = self.data_sources.clone();
        cx.spawn_in(window, async move |_, cx| {
            let secrets = secrets.await.unwrap_or_default();
            cx.update(|window, cx| open_form(data_sources, Some((config, secrets)), window, cx))
                .ok();
        })
        .detach();
    }

    fn delete(&mut self, id: ConnectionId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.data_sources.read(cx).get(id).map(|c| c.display_name()) else {
            return;
        };
        let data_sources = self.data_sources.clone();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let data_sources = data_sources.clone();
            alert
                .icon(Icon::new(IconName::TriangleAlert).text_color(cx.theme().warning))
                .title(format!("Delete “{name}”?"))
                .description("The saved connection and its stored passwords will be removed.")
                .show_cancel(true)
                .on_ok(move |_, _, cx| {
                    data_sources.update(cx, |ds, cx| ds.delete(id, cx));
                    true
                })
        });
    }

    fn connect(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.data_sources
            .update(cx, |ds, cx| ds.connect(id, HostKeyPolicy::KnownOnly, cx));
    }

    fn disconnect(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.data_sources.update(cx, |ds, cx| ds.disconnect(id, cx));
    }

    fn refresh(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.data_sources.update(cx, |ds, cx| ds.refresh(id, cx));
    }

    /// What a double-click on a row does: a data source connects, and a
    /// table or view opens its data, as "Open data" does.
    fn activate(&mut self, row: &SharedString, cx: &mut Context<Self>) {
        match kind_of(row) {
            "source" => {
                if let Some(id) = connection_of(row) {
                    self.connect(id, cx);
                }
            }
            "table" | "view" => {
                if let Some(node) = self.nodes.get(row).cloned() {
                    cx.emit(ExplorerEvent::OpenData(node));
                }
            }
            _ => {}
        }
    }

    /// Runs an action from the table menu. The row is selected first, since
    /// the console runs against the explorer's selection.
    fn table_action(
        &mut self,
        row: SharedString,
        object: ObjectRef,
        action: TableAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tree.update(cx, |tree, cx| {
            tree.set_selected_item(Some(&TreeItem::new(row.clone(), "")), cx)
        });
        let connection = object.connection;
        let sql = |sql: String, run| ExplorerEvent::Sql {
            connection,
            sql,
            run,
            refresh: false,
        };
        match action {
            TableAction::OpenData => {
                if let Some(node) = self.nodes.get(&row).cloned() {
                    cx.emit(ExplorerEvent::OpenData(node));
                }
            }
            TableAction::NewSelect => cx.emit(sql(object.select_sql(), false)),
            TableAction::ShowDiagram => {
                if let Some(node) = self.nodes.get(&row).cloned() {
                    self.show_diagram(node, cx);
                }
            }
            TableAction::Structure => {
                if let Some(node) = self.nodes.get(&row).cloned() {
                    cx.emit(ExplorerEvent::ShowStructure(node));
                }
            }
            TableAction::Export | TableAction::Import => {
                if let Some(node) = self.nodes.get(&row).cloned() {
                    let direction = match action {
                        TableAction::Export => crate::transfer::Direction::Export,
                        _ => crate::transfer::Direction::Import,
                    };
                    cx.emit(ExplorerEvent::Transfer(node, direction));
                }
            }
            TableAction::CopyName => {
                cx.write_to_clipboard(ClipboardItem::new_string(object.name.clone()))
            }
            TableAction::CopyQualifiedName => {
                cx.write_to_clipboard(ClipboardItem::new_string(object.qualified_name()))
            }
            TableAction::Truncate | TableAction::Drop => {
                self.confirm_destructive(object, action, window, cx)
            }
        }
    }

    /// Shows the exact statement and where it runs; nothing runs until OK.
    fn confirm_destructive(
        &mut self,
        object: ObjectRef,
        action: TableAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self
            .data_sources
            .read(cx)
            .get(object.connection)
            .map(|c| c.display_name())
        else {
            return;
        };
        let drop = action == TableAction::Drop;
        let (sql, title, ok, consequence) = if drop {
            let noun = object.kind.noun();
            (
                object.drop_sql(),
                format!("Drop {noun} “{}”?", object.name),
                format!("Drop {noun}"),
                format!("The {noun} and everything in it are removed. This can't be undone."),
            )
        } else {
            (
                object.truncate_sql(),
                format!("Truncate “{}”?", object.name),
                "Truncate".to_string(),
                "Every row is deleted; the table itself stays. This can't be undone.".to_string(),
            )
        };
        let this = cx.entity().downgrade();
        let (source, refresh) = (object.connection, drop);
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let theme = cx.theme();
            let this = this.clone();
            let run = sql.clone();
            alert
                .icon(Icon::new(IconName::TriangleAlert).text_color(theme.danger))
                .title(title.clone())
                .description(
                    v_flex()
                        .gap_3()
                        .child(format!("Runs on {connection}. {consequence}"))
                        .child(
                            div()
                                .p_2()
                                .rounded(px(6.))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.background)
                                .font_family("monospace")
                                .text_sm()
                                .text_color(theme.foreground)
                                .child(sql.clone()),
                        ),
                )
                .show_cancel(true)
                .ok_text(ok.clone())
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let sql = run.clone();
                    this.update(cx, |_, cx| {
                        cx.emit(ExplorerEvent::Sql {
                            connection: source,
                            sql,
                            run: true,
                            refresh,
                        })
                    })
                    .ok();
                    true
                })
        });
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_connection(cx);
        let node = self.selected_node(cx);
        let has_node = node.is_some();
        let (dump_node, import_node) = (node.clone(), node.clone());
        let connected = selected.is_some_and(|id| {
            matches!(
                self.data_sources.read(cx).state(id),
                SourceState::Connected(_)
            )
        });
        let band = theme::band_button(cx);
        let button = move |id: &'static str, icon: Icon, tooltip: &'static str| {
            Button::new(id)
                .custom(band)
                .small()
                .icon(icon)
                .tooltip(tooltip)
        };

        // The explorer's Ivrea band, level with the console's.
        h_flex()
            .h(px(34.))
            .px_2()
            .gap_0p5()
            .bg(theme::c(theme::IVREA_GREEN))
            .text_color(theme::c(theme::BAND_INK))
            .child(
                button("ex-add", Icon::new(IconName::Plus), "New data source")
                    .on_click(cx.listener(|this, _, window, cx| this.new_connection(window, cx))),
            )
            .child(
                button(
                    "ex-refresh",
                    Icon::new(IconName::RefreshCw),
                    "Refresh (reconnects if needed)",
                )
                .band_disabled(selected.is_none())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(id) = selected {
                        this.refresh(id, cx);
                    }
                })),
            )
            .child(
                button("ex-disconnect", Icon::new(Lucide::Unplug), "Disconnect")
                    .band_disabled(!connected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(id) = selected {
                            this.disconnect(id, cx);
                        }
                    })),
            )
            .child(
                button(
                    "ex-edit",
                    Icon::new(IconName::Settings),
                    "Data source properties",
                )
                .band_disabled(selected.is_none())
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(id) = selected {
                        this.edit(id, window, cx);
                    }
                })),
            )
            .child(
                div()
                    .w(px(1.))
                    .h(px(16.))
                    .mx_1()
                    .bg(theme::c(theme::BAND_RULE)),
            )
            .child(
                button(
                    "ex-console",
                    Icon::new(IconName::SquareTerminal),
                    "New query console (M2)",
                )
                .band_disabled(true),
            )
            .child(
                button("ex-ddl", Icon::new(Lucide::FileCode), "Show DDL (M2)").band_disabled(true),
            )
            .child(
                button("ex-diagram", Icon::new(Lucide::Workflow), "Show diagram")
                    .band_disabled(node.is_none())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(node) = node.clone() {
                            this.show_diagram(node, cx);
                        }
                    })),
            )
            .child(
                button("ex-dump", Icon::new(Lucide::Download), "Dump the schema…")
                    .band_disabled(!has_node || !connected)
                    .on_click(cx.listener({
                        move |_, _, _, cx| {
                            if let Some(node) = dump_node.clone() {
                                let schema = NodeRef {
                                    table: None,
                                    ..node
                                };
                                cx.emit(ExplorerEvent::Transfer(
                                    schema,
                                    crate::transfer::Direction::Export,
                                ));
                            }
                        }
                    })),
            )
            .child(
                button(
                    "ex-import",
                    Icon::new(Lucide::Upload),
                    "Import into the schema…",
                )
                .band_disabled(!has_node || !connected)
                .on_click(cx.listener(move |_, _, _, cx| {
                    if let Some(node) = import_node.clone() {
                        cx.emit(ExplorerEvent::Transfer(
                            node,
                            crate::transfer::Direction::Import,
                        ));
                    }
                })),
            )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .p_4()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(Icon::new(Lucide::Database).large())
            .child("No data sources yet")
            .when_some(
                self.data_sources.read(cx).storage_error.clone(),
                |this, err| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(format!("Storage error: {err}")),
                    )
                },
            )
            .child(
                Button::new("empty-add")
                    .small()
                    .primary()
                    .label("New data source")
                    .on_click(cx.listener(|this, _, window, cx| this.new_connection(window, cx))),
            )
    }
}

fn open_form(
    data_sources: Entity<DataSources>,
    existing: Option<(ConnectionConfig, savoia_core::Secrets)>,
    window: &mut Window,
    cx: &mut App,
) {
    let title = if existing.is_some() {
        "Data source properties"
    } else {
        "New data source"
    };
    let form = cx.new(|cx| ConnectionForm::new(data_sources, existing, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title).w(px(640.)).child(form.clone())
    });
}

fn node_icon(entry: &TreeEntry) -> Icon {
    match kind_of(&entry.item().id) {
        "source" => Icon::new(Lucide::Server),
        "database" => Icon::new(Lucide::Database),
        "schema" => Icon::new(Lucide::Layers),
        "group" if entry.is_expanded() => Icon::new(IconName::FolderOpen),
        "group" => Icon::new(IconName::Folder),
        "table" => Icon::new(Lucide::Table),
        "column" => Icon::new(Lucide::Columns3),
        "pkcolumn" => Icon::new(Lucide::KeyRound),
        "fkcolumn" | "fk" => Icon::new(Lucide::Link2),
        "index" => Icon::new(Lucide::Key),
        "view" => Icon::new(IconName::Eye),
        "function" => Icon::new(Lucide::Braces),
        "sequence" => Icon::new(Lucide::ListOrdered),
        _ => Icon::new(IconName::File),
    }
}

impl Render for Explorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let empty = self.data_sources.read(cx).connections().is_empty();
        let meta = self.meta.clone();
        let suffixes = self.suffixes.clone();
        let nodes = self.nodes.clone();
        let this = cx.entity().downgrade();
        let menu_target = cx.entity().downgrade();

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                // Same height as the console's tab strip, so both green tool
                // bands start on one line across the panel seam.
                h_flex()
                    .h(px(36.))
                    .px_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_sm()
                    .font_semibold()
                    .child("Database Explorer"),
            )
            .child(self.render_toolbar(cx))
            .when(empty, |this| this.child(self.render_empty(cx)))
            .when(!empty, |el| {
                el.child(
                    div().flex_1().min_h_0().py_1().child(
                        tree(&self.tree, move |_, entry, _selected, _, _| {
                            let id = entry.item().id.clone();
                            let depth = entry.depth() as f32;
                            let muted = theme.muted_foreground;
                            let chevron = if entry.is_folder() {
                                let icon = if entry.is_expanded() {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                };
                                Icon::new(icon)
                                    .xsmall()
                                    .text_color(muted)
                                    .into_any_element()
                            } else {
                                div().w(px(12.)).into_any_element()
                            };
                            let source = (kind_of(&id) == "source")
                                .then(|| connection_of(&id))
                                .flatten();
                            let source_meta = source.and_then(|c| meta.get(&c).cloned());
                            let suffix = suffixes.get(&id).cloned();
                            let info = kind_of(&id) == "info";
                            let icon = node_icon(entry).small().text_color(
                                source_meta
                                    .as_ref()
                                    .and_then(|m| m.color)
                                    .map_or(muted, |c| rgb(c).into()),
                            );
                            let this = this.clone();

                            // The row's own id, not its index: rows shift as
                            // others expand.
                            ListItem::new(id.clone())
                                .py_0p5()
                                .pl(px(6. + depth * 14.))
                                .on_click(move |event, _, cx| {
                                    if event.click_count() == 2 {
                                        this.update(cx, |this, cx| this.activate(&id, cx)).ok();
                                    }
                                })
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .text_sm()
                                        .child(chevron)
                                        .when(!info, |el| el.child(icon))
                                        .child(
                                            div()
                                                .when(info, |el| el.italic().text_color(muted))
                                                .child(entry.item().label.clone()),
                                        )
                                        .when_some(suffix, |el, text| {
                                            el.child(div().text_xs().text_color(muted).child(text))
                                        })
                                        .when_some(source_meta.map(|m| m.status), |el, status| {
                                            match status {
                                                SourceStatus::Idle => el,
                                                SourceStatus::Pending(text) => el.child(
                                                    div().text_xs().text_color(muted).child(text),
                                                ),
                                                SourceStatus::Connected(version) => el
                                                    .child(
                                                        div()
                                                            .size(px(6.))
                                                            .rounded_full()
                                                            .bg(theme.success),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(muted)
                                                            .child(version),
                                                    ),
                                                SourceStatus::Failed => el.child(
                                                    div()
                                                        .size(px(6.))
                                                        .rounded_full()
                                                        .bg(theme.danger),
                                                ),
                                            }
                                        }),
                                )
                        })
                        .context_menu(move |_, entry, menu, _, cx| {
                            let row = entry.item().id.clone();
                            let Some(id) = connection_of(&row) else {
                                return menu;
                            };
                            let target = menu_target.clone();
                            let table = target.upgrade().and_then(|explorer| {
                                let ds = explorer.read(cx).data_sources.read(cx);
                                let config = ds.get(id)?;
                                let object = ObjectRef::parse(&row, config.engine)?;
                                let origin = Origin {
                                    name: config.display_name(),
                                    color: config.color.map(|c| c.rgb()),
                                    read_only: config.read_only,
                                };
                                Some((object, origin))
                            });
                            if let Some((object, origin)) = table {
                                let picked = object.clone();
                                return table_menu::build(
                                    menu,
                                    &object,
                                    &origin,
                                    move |action, window, cx| {
                                        let (row, object) = (row.clone(), picked.clone());
                                        target
                                            .update(cx, |this, cx| {
                                                this.table_action(row, object, action, window, cx)
                                            })
                                            .ok();
                                    },
                                );
                            }
                            let item = |label: &'static str,
                                        f: fn(
                                &mut Explorer,
                                ConnectionId,
                                &mut Window,
                                &mut Context<Explorer>,
                            )| {
                                let target = target.clone();
                                PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                    target.update(cx, |this, cx| f(this, id, window, cx)).ok();
                                })
                            };
                            let menu = match nodes.get(&entry.item().id).cloned() {
                                Some(node) => {
                                    let target = target.clone();
                                    menu.item(PopupMenuItem::new("Show diagram").on_click(
                                        move |_, _, cx| {
                                            let node = node.clone();
                                            target
                                                .update(cx, |this, cx| this.show_diagram(node, cx))
                                                .ok();
                                        },
                                    ))
                                    .separator()
                                }
                                None => menu,
                            };
                            menu.item(item("Connect", |this, id, _, cx| this.connect(id, cx)))
                                .item(item("Disconnect", |this, id, _, cx| {
                                    this.disconnect(id, cx)
                                }))
                                .item(item("Refresh", |this, id, _, cx| this.refresh(id, cx)))
                                .separator()
                                .item(item("Properties…", |this, id, window, cx| {
                                    this.edit(id, window, cx)
                                }))
                                .item(item("Delete…", |this, id, window, cx| {
                                    this.delete(id, window, cx)
                                }))
                        }),
                    ),
                )
            })
    }
}

#[cfg(test)]
impl Explorer {
    /// Selects the first row: the first data source.
    pub fn select_first_source(&mut self, cx: &mut Context<Self>) {
        self.tree
            .update(cx, |tree, cx| tree.set_selected_index(Some(0), cx));
    }

    /// Expands the row at `path`, as a click would: the first visible row
    /// labelled `path[0]`, then each next label among the previous row's
    /// children.
    pub fn expand(&mut self, path: &[&str], cx: &mut Context<Self>) {
        fn find(items: &[TreeItem], label: &str) -> Option<TreeItem> {
            items.iter().find_map(|item| {
                if item.label.as_ref() == label {
                    Some(item.clone())
                } else if item.is_expanded() {
                    find(&item.children, label)
                } else {
                    None
                }
            })
        }
        let mut item = find(&self.items, path[0]);
        for label in &path[1..] {
            item = item.and_then(|i| {
                i.children
                    .iter()
                    .find(|c| c.label.as_ref() == *label)
                    .cloned()
            });
        }
        let item = item.unwrap_or_else(|| panic!("no row {path:?}"));
        let id = item.id.clone();
        drop(item.expanded(true));
        self.rebuild(cx);
        self.load(&id, cx);
    }

    /// Labels of the rows a user would see (expanded paths only), depth-first.
    pub fn visible_labels(&self) -> Vec<String> {
        fn walk(items: &[TreeItem], out: &mut Vec<String>) {
            for item in items {
                out.push(item.label.to_string());
                if item.is_expanded() {
                    walk(&item.children, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.items, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` (via `super::*`) would shadow `#[test]`.
    use savoia_core::ConnectionId;

    use super::{connection_of, kind_of};

    #[test]
    fn ids_route_to_their_connection() {
        let id = ConnectionId::new();
        assert_eq!(connection_of(&format!("source:{id}")), Some(id));
        assert_eq!(
            connection_of(&format!("table:{id}/shop/public/orders")),
            Some(id)
        );
        assert_eq!(connection_of("group:not-a-uuid/x"), None);
        assert_eq!(kind_of(&format!("schema:{id}/db/public")), "schema");
    }
}
