//! Lazily-loaded navigator tree: Project → Instance → Database → Schema → Table.
//! Pure data; the navigator view renders the flattened visible rows.

use std::sync::Arc;

use cirrus_core::{Instance, SchemaInfo, TableInfo};

/// Identifies one database on one Cloud SQL instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DbRef {
    pub project: String,
    pub instance: String,
    pub database: String,
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    Project { project: String },
    Instance { instance: Arc<Instance> },
    Database { db: DbRef },
    Schema { db: DbRef, schema: SchemaInfo },
    Table { db: DbRef, table: TableInfo },
}

impl NodeKind {
    pub fn is_leaf(&self) -> bool {
        matches!(self, NodeKind::Table { .. })
    }

    /// Identity that survives reloads and restarts (used to remember expanded nodes).
    pub fn key(&self) -> String {
        match self {
            NodeKind::Project { project } => format!("project:{project}"),
            NodeKind::Instance { instance } => format!("instance:{}/{}", instance.project, instance.name),
            NodeKind::Database { db } => format!("db:{}/{}/{}", db.project, db.instance, db.database),
            NodeKind::Schema { db, schema } => {
                format!("schema:{}/{}/{}/{}", db.project, db.instance, db.database, schema.name)
            }
            NodeKind::Table { db, table } => {
                format!("table:{}/{}/{}/{}.{}", db.project, db.instance, db.database, table.schema, table.name)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Children {
    NotLoaded,
    Loading,
    Loaded(Vec<Node>),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub label: String,
    /// Secondary text shown dimmed after the label (e.g. engine version, row estimate).
    pub detail: Option<String>,
    pub disabled: bool,
    pub expanded: bool,
    pub children: Children,
}

impl Node {
    pub fn new(kind: NodeKind, label: impl Into<String>) -> Self {
        Self { kind, label: label.into(), detail: None, disabled: false, expanded: false, children: Children::NotLoaded }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// Path of child indices from the root list to a node.
pub type NodePath = Vec<usize>;

/// One visible row of the flattened tree.
#[derive(Debug, Clone)]
pub enum Row {
    Node { path: NodePath, depth: usize },
    Status { depth: usize, text: String, is_error: bool },
}

pub fn node_mut<'a>(roots: &'a mut [Node], path: &[usize]) -> Option<&'a mut Node> {
    let (first, rest) = path.split_first()?;
    let mut node = roots.get_mut(*first)?;
    for &i in rest {
        match &mut node.children {
            Children::Loaded(children) => node = children.get_mut(i)?,
            _ => return None,
        }
    }
    Some(node)
}

pub fn node<'a>(roots: &'a [Node], path: &[usize]) -> Option<&'a Node> {
    let (first, rest) = path.split_first()?;
    let mut node = roots.get(*first)?;
    for &i in rest {
        match &node.children {
            Children::Loaded(children) => node = children.get(i)?,
            _ => return None,
        }
    }
    Some(node)
}

/// Flattens the expanded part of the tree. Top-level nodes are filtered by `filter`
/// (case-insensitive substring); a project also matches when one of its loaded
/// descendants matches, so searching for a table name keeps its ancestors visible.
pub fn flatten(roots: &[Node], filter: &str) -> Vec<Row> {
    let filter = filter.trim().to_lowercase();
    let mut rows = Vec::new();
    for (i, node) in roots.iter().enumerate() {
        if filter.is_empty() || subtree_matches(node, &filter) {
            push_rows(node, vec![i], 0, &filter, &mut rows);
        }
    }
    rows
}

fn subtree_matches(node: &Node, filter: &str) -> bool {
    node.label.to_lowercase().contains(filter)
        || matches!(&node.children, Children::Loaded(c) if c.iter().any(|n| subtree_matches(n, filter)))
}

fn push_rows(node: &Node, path: NodePath, depth: usize, filter: &str, rows: &mut Vec<Row>) {
    rows.push(Row::Node { path: path.clone(), depth });
    if !node.expanded {
        return;
    }
    // Once a node itself matches, show all of its children.
    let child_filter = if !filter.is_empty() && node.label.to_lowercase().contains(filter) { "" } else { filter };
    match &node.children {
        Children::NotLoaded => {}
        Children::Loading => rows.push(Row::Status { depth: depth + 1, text: "Loading…".into(), is_error: false }),
        Children::Failed(e) => rows.push(Row::Status { depth: depth + 1, text: e.clone(), is_error: true }),
        Children::Loaded(children) if children.is_empty() => {
            rows.push(Row::Status { depth: depth + 1, text: "Empty".into(), is_error: false })
        }
        Children::Loaded(children) => {
            for (i, child) in children.iter().enumerate() {
                if child_filter.is_empty() || subtree_matches(child, child_filter) {
                    let mut p = path.clone();
                    p.push(i);
                    push_rows(child, p, depth + 1, child_filter, rows);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str, children: Vec<Node>) -> Node {
        let mut n = Node::new(NodeKind::Project { project: name.into() }, name);
        n.expanded = true;
        n.children = Children::Loaded(children);
        n
    }

    #[test]
    fn filter_keeps_ancestors_of_matches() {
        let db = DbRef { project: "p".into(), instance: "i".into(), database: "d".into() };
        let child = Node::new(NodeKind::Database { db }, "orders_db");
        let roots = vec![project("alpha", vec![child]), project("beta", vec![])];
        let rows = flatten(&roots, "orders");
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[1], Row::Node { path, depth: 1 } if path == &vec![0, 0]));
    }

    #[test]
    fn collapsed_nodes_hide_children() {
        let mut p = project("alpha", vec![]);
        p.expanded = false;
        assert_eq!(flatten(&[p], "").len(), 1);
    }
}
