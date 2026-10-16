//! Domain types shared by the drivers, storage, transfer engines and the UI.
//! This crate has no UI or database-driver dependencies.

mod catalog;
pub mod complete;
mod connection;
pub mod data_query;
pub mod ddl;
mod driver;
pub mod edit;
mod engine;
mod error;
pub mod export;
pub mod format;
mod query;
pub mod split;
pub mod sql_text;

pub use catalog::{
    Catalog, ColumnInfo, DatabaseNode, ForeignKey, IndexInfo, ObjectCounts, SchemaNode,
    SchemaObjects, ServerInfo, TableInfo, TableKind, attach,
};
pub use connection::{
    ConnectionColor, ConnectionConfig, ConnectionId, Endpoint, Secrets, SshAuth, SshConfig,
    SslMode, parse_url,
};
pub use driver::{Connection, Driver};
pub use engine::Engine;
pub use error::{AppError, AppResult};
pub use query::{
    Cancel, CancelHandle, Cell, Closed, ColumnMeta, PAGE_LATENCY, PAGE_ROWS, QueryEvent,
    QueryHandle, QuerySender, Row, ValueKind,
};
