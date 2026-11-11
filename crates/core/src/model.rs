use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub project_id: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub name: String,
    pub project: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub database_version: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub connection_name: Option<String>,
    #[serde(default)]
    pub instance_type: Option<String>,
}

impl Instance {
    pub fn is_postgres(&self) -> bool {
        self.database_version
            .as_deref()
            .is_some_and(|v| v.starts_with("POSTGRES"))
    }

    pub fn is_runnable(&self) -> bool {
        self.state.as_deref() == Some("RUNNABLE")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

impl RelKind {
    pub fn from_pg(kind: &str) -> Option<Self> {
        Some(match kind {
            "r" => Self::Table,
            "p" => Self::PartitionedTable,
            "v" => Self::View,
            "m" => Self::MaterializedView,
            "f" => Self::ForeignTable,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::PartitionedTable => "partitioned table",
            Self::View => "view",
            Self::MaterializedView => "materialized view",
            Self::ForeignTable => "foreign table",
        }
    }

    pub fn is_view(self) -> bool {
        matches!(self, Self::View | Self::MaterializedView)
    }
}

#[derive(Debug, Clone)]
pub struct SchemaInfo {
    pub name: String,
    pub is_system: bool,
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: RelKind,
    /// Planner estimate (`pg_class.reltuples`); negative when never analysed.
    pub estimated_rows: f64,
    pub comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
    pub is_primary_key: bool,
    pub is_foreign_key: bool,
}

#[derive(Debug, Clone)]
pub struct ConstraintInfo {
    pub name: String,
    /// `p` primary key, `f` foreign key, `u` unique, `c` check, `x` exclusion.
    pub kind: char,
    pub definition: String,
    pub columns: Vec<String>,
}

impl ConstraintInfo {
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            'p' => "PRIMARY KEY",
            'f' => "FOREIGN KEY",
            'u' => "UNIQUE",
            'c' => "CHECK",
            'x' => "EXCLUDE",
            't' => "TRIGGER",
            'n' => "NOT NULL",
            _ => "OTHER",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ForeignKey {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IndexInfo {
    pub name: String,
    pub definition: String,
    pub is_unique: bool,
    pub is_primary: bool,
}

#[derive(Debug, Clone)]
pub struct TableDetails {
    pub table: TableInfo,
    pub columns: Vec<ColumnInfo>,
    pub constraints: Vec<ConstraintInfo>,
    pub indexes: Vec<IndexInfo>,
    pub total_size_bytes: Option<i64>,
    pub ddl: String,
}

/// Everything the ER diagram needs for one schema.
#[derive(Debug, Clone)]
pub struct SchemaModel {
    pub schema: String,
    pub tables: Vec<(TableInfo, Vec<ColumnInfo>)>,
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Debug, Clone)]
pub struct ResultColumn {
    pub name: String,
    pub type_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    /// Values in PostgreSQL text format; `None` is SQL NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// More rows were available than were fetched.
    pub has_more: bool,
    pub elapsed: Duration,
    /// Command tag for statements that don't return rows (e.g. `SET`).
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Default)]
pub struct PageRequest {
    /// Raw SQL boolean expression placed after `WHERE`.
    pub filter: Option<String>,
    pub order_by: Option<(String, SortDirection)>,
    pub limit: usize,
    pub offset: usize,
}
