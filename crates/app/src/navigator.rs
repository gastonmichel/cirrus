//! Sidebar navigator: Projects → Instances → Databases → Schemas → Tables, loaded lazily.

use std::collections::HashSet;
use std::sync::Arc;

use gpui_kit::assets::IconName as I;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use cirrus_core::TableInfo;

use crate::table_view::format_count;
use crate::tree::{self, Children, DbRef, Node, NodeKind, NodePath, Row};

const ROW_HEIGHT: f32 = 26.;
const INDENT: f32 = 14.;

pub enum NavigatorEvent {
    OpenTable(DbRef, TableInfo),
    OpenConsole(DbRef),
    OpenDiagram(DbRef, String),
}

impl EventEmitter<NavigatorEvent> for Navigator {}

pub struct Navigator {
    roots: Children,
    rows: Vec<Row>,
    selected: Option<NodePath>,
    filter: Entity<InputState>,
    scroll: UniformListScrollHandle,
    /// Keys of expanded nodes, persisted across restarts and re-applied as children load.
    expanded: HashSet<String>,
    _subscriptions: Vec<Subscription>,
}

impl Navigator {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter projects, databases, tables…"));
        let subscriptions = vec![cx.subscribe(&filter, |this, _, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                this.rebuild_rows(cx);
            }
        })];
        Self {
            roots: Children::NotLoaded,
            rows: vec![],
            selected: None,
            filter,
            scroll: UniformListScrollHandle::new(),
            expanded: cirrus_core::settings::load().expanded_nodes.into_iter().collect(),
            _subscriptions: subscriptions,
        }
    }

    pub fn reload_projects(&mut self, cx: &mut Context<Self>) {
        self.roots = Children::Loading;
        self.selected = None;
        self.rebuild_rows(cx);
        cx.spawn(async move |this, cx| {
            let result = cirrus_core::runtime::spawn(cirrus_core::admin::list_projects()).await;
            this.update(cx, |this, cx| {
                this.roots = match result {
                    Ok(projects) => Children::Loaded(
                        projects
                            .into_iter()
                            .map(|p| {
                                let label = p.project_id.clone();
                                let node = Node::new(NodeKind::Project { project: p.project_id }, label);
                                match p.name {
                                    Some(name) if name != node.label => node.with_detail(name),
                                    _ => node,
                                }
                            })
                            .collect(),
                    ),
                    Err(e) => Children::Failed(format!("{e:#}")),
                };
                this.restore_expanded(&[], cx);
                this.rebuild_rows(cx);
            })
            .ok();
        })
        .detach();
    }

    fn roots(&self) -> &[Node] {
        match &self.roots {
            Children::Loaded(nodes) => nodes,
            _ => &[],
        }
    }

    fn roots_mut(&mut self) -> &mut [Node] {
        match &mut self.roots {
            Children::Loaded(nodes) => nodes,
            _ => &mut [],
        }
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter.read(cx).value().to_string();
        self.rows = tree::flatten(self.roots(), &filter);
        cx.notify();
    }

    fn click(&mut self, path: NodePath, cx: &mut Context<Self>) {
        self.selected = Some(path.clone());
        let Some(node) = tree::node_mut(self.roots_mut(), &path) else { return };
        if node.disabled {
            return cx.notify();
        }
        if let NodeKind::Table { db, table } = &node.kind {
            let event = NavigatorEvent::OpenTable(db.clone(), table.clone());
            cx.emit(event);
            return cx.notify();
        }
        node.expanded = !node.expanded;
        let (expanded, key) = (node.expanded, node.kind.key());
        let needs_load = node.expanded && matches!(node.children, Children::NotLoaded | Children::Failed(_));
        if needs_load {
            node.children = Children::Loading;
            let kind = node.kind.clone();
            self.load_children(path, kind, cx);
        }
        self.remember_expanded(key, expanded);
        self.rebuild_rows(cx);
    }

    fn refresh_node(&mut self, path: NodePath, cx: &mut Context<Self>) {
        let Some(node) = tree::node_mut(self.roots_mut(), &path) else { return };
        node.expanded = true;
        node.children = Children::Loading;
        let kind = node.kind.clone();
        self.remember_expanded(kind.key(), true);
        self.load_children(path, kind, cx);
        self.rebuild_rows(cx);
    }

    /// Records an expand/collapse and persists it. Collapsing a node keeps its
    /// descendants' state, so re-expanding it reopens them as they were.
    fn remember_expanded(&mut self, key: String, expanded: bool) {
        let changed = if expanded { self.expanded.insert(key) } else { self.expanded.remove(&key) };
        if changed {
            let mut keys: Vec<String> = self.expanded.iter().cloned().collect();
            keys.sort();
            cirrus_core::settings::update(|state| state.expanded_nodes = keys);
        }
    }

    /// Re-expands (and loads) the children of `parent` (or the roots, for `[]`)
    /// that were expanded before; recursion happens as each level finishes loading.
    fn restore_expanded(&mut self, parent: &[usize], cx: &mut Context<Self>) {
        let expanded = &self.expanded;
        let Children::Loaded(roots) = &mut self.roots else { return };
        let children = if parent.is_empty() {
            roots
        } else {
            match tree::node_mut(roots, parent).map(|n| &mut n.children) {
                Some(Children::Loaded(nodes)) => nodes,
                _ => return,
            }
        };
        let mut to_load = Vec::new();
        for (i, child) in children.iter_mut().enumerate() {
            if child.kind.is_leaf() || child.disabled || child.expanded || !expanded.contains(&child.kind.key()) {
                continue;
            }
            child.expanded = true;
            if matches!(child.children, Children::NotLoaded) {
                child.children = Children::Loading;
                let mut path = parent.to_vec();
                path.push(i);
                to_load.push((path, child.kind.clone()));
            }
        }
        for (path, kind) in to_load {
            self.load_children(path, kind, cx);
        }
    }

    fn load_children(&mut self, path: NodePath, kind: NodeKind, cx: &mut Context<Self>) {
        let expected_label = tree::node(self.roots(), &path).map(|n| n.label.clone());
        cx.spawn(async move |this, cx| {
            let result = cirrus_core::runtime::spawn(fetch_children(kind)).await;
            this.update(cx, |this, cx| {
                // The tree may have been reloaded meanwhile; only apply to the same node.
                let Some(node) = tree::node_mut(this.roots_mut(), &path) else { return };
                if Some(&node.label) != expected_label.as_ref() {
                    return;
                }
                node.children = match result {
                    Ok(children) => Children::Loaded(children),
                    Err(e) => Children::Failed(first_line(&format!("{e:#}"))),
                };
                this.restore_expanded(&path, cx);
                this.rebuild_rows(cx);
            })
            .ok();
        })
        .detach();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let base = h_flex().id(("nav-row", ix)).h(px(ROW_HEIGHT)).w_full().overflow_hidden().pr_1().gap_1p5().text_sm();
        match &self.rows[ix] {
            Row::Status { depth, text, is_error } => base
                .pl(px(8. + INDENT * (*depth as f32) + 18.))
                .text_xs()
                .text_color(if *is_error { theme.danger } else { theme.muted_foreground })
                .child(div().truncate().child(text.clone()))
                .when(*is_error, |el| {
                    let text = text.clone();
                    el.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
                })
                .into_any_element(),
            Row::Node { path, depth } => {
                let Some(node) = tree::node(self.roots(), path) else { return div().into_any_element() };
                let selected = self.selected.as_ref() == Some(path);
                // Fixed-width, non-shrinking slots keep every row aligned however narrow the panel is.
                let chevron = h_flex().w(px(14.)).flex_shrink_0().justify_center().children((!node.kind.is_leaf()).then(|| {
                    Icon::new(if node.expanded { I::ChevronDown } else { I::ChevronRight })
                        .xsmall()
                        .text_color(theme.muted_foreground)
                }));
                let (icon, icon_color) = node_icon(&node.kind, theme);
                let click_path = path.clone();
                let mut row = base
                    .pl(px(8. + INDENT * (*depth as f32)))
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(selected, |el| el.bg(theme.list_active))
                    .when(!selected, |el| el.hover(|el| el.bg(theme.list_hover)))
                    .when(node.disabled, |el| el.opacity(0.5))
                    .on_click(cx.listener(move |this, _, _, cx| this.click(click_path.clone(), cx)))
                    .child(chevron)
                    .child(
                        h_flex()
                            .w(px(16.))
                            .flex_shrink_0()
                            .justify_center()
                            .child(Icon::new(icon).small().text_color(icon_color)),
                    )
                    // The label truncates before anything else; the detail only uses leftover space.
                    .child(div().min_w_0().truncate().child(node.label.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .children(node.detail.clone()),
                    );
                row = row.child(h_flex().flex_shrink_0().children(self.row_actions(ix, path, node, cx)));
                row.into_any_element()
            }
        }
    }

    /// Small trailing buttons: refresh for containers, console for databases, diagram for schemas.
    fn row_actions(&self, ix: usize, path: &NodePath, node: &Node, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut actions = Vec::new();
        match &node.kind {
            NodeKind::Database { db } => {
                let db = db.clone();
                actions.push(
                    Button::new(("console", ix))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(I::SquareTerminal))
                        .tooltip("Open SQL console")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.stop_propagation();
                            cx.emit(NavigatorEvent::OpenConsole(db.clone()));
                        }))
                        .into_any_element(),
                );
            }
            NodeKind::Schema { db, schema } => {
                let (db, schema) = (db.clone(), schema.name.clone());
                actions.push(
                    Button::new(("diagram", ix))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(I::Workflow))
                        .tooltip("Open ER diagram")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.stop_propagation();
                            cx.emit(NavigatorEvent::OpenDiagram(db.clone(), schema.clone()));
                        }))
                        .into_any_element(),
                );
            }
            _ => {}
        }
        if !node.kind.is_leaf() && node.expanded && !node.disabled {
            let path = path.clone();
            actions.push(
                Button::new(("refresh", ix))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(I::RefreshCw))
                    .tooltip("Refresh")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.refresh_node(path.clone(), cx);
                    }))
                    .into_any_element(),
            );
        }
        actions
    }
}

fn node_icon(kind: &NodeKind, theme: &Theme) -> (I, Hsla) {
    match kind {
        NodeKind::Project { .. } => (I::Cloud, theme.blue),
        NodeKind::Instance { .. } => (I::Server, theme.cyan),
        NodeKind::Database { .. } => (I::Database, theme.yellow),
        NodeKind::Schema { schema, .. } if schema.is_system => (I::FolderCog, theme.muted_foreground),
        NodeKind::Schema { .. } => (I::Folder, theme.magenta),
        NodeKind::Table { table, .. } if table.kind.is_view() => (I::Eye, theme.green),
        NodeKind::Table { .. } => (I::Table, theme.green),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).to_string()
}

/// Loads the children of a node (runs on the tokio runtime).
async fn fetch_children(kind: NodeKind) -> anyhow::Result<Vec<Node>> {
    Ok(match kind {
        NodeKind::Project { project } => cirrus_core::admin::list_instances(&project)
            .await?
            .into_iter()
            .map(|inst| {
                let mut detail = inst.database_version.clone().unwrap_or_default().replace('_', " ").to_lowercase();
                if let Some(region) = &inst.region {
                    detail.push_str(&format!(" · {region}"));
                }
                let supported = inst.is_postgres();
                if !supported {
                    detail.push_str(" · unsupported");
                } else if !inst.is_runnable() {
                    detail.push_str(&format!(" · {}", inst.state.clone().unwrap_or_default().to_lowercase()));
                }
                let label = inst.name.clone();
                let mut node = Node::new(NodeKind::Instance { instance: Arc::new(inst) }, label).with_detail(detail);
                node.disabled = !supported;
                node
            })
            .collect(),
        NodeKind::Instance { instance } => cirrus_core::admin::list_databases(&instance.project, &instance.name)
            .await?
            .into_iter()
            .map(|database| {
                let db = DbRef { project: instance.project.clone(), instance: instance.name.clone(), database: database.clone() };
                Node::new(NodeKind::Database { db }, database)
            })
            .collect(),
        NodeKind::Database { db } => cirrus_core::pg::session(&db.project, &db.instance, &db.database)
            .await?
            .schemas()
            .await?
            .into_iter()
            .map(|schema| {
                let label = schema.name.clone();
                let system = schema.is_system;
                let node = Node::new(NodeKind::Schema { db: db.clone(), schema }, label);
                if system { node.with_detail("system") } else { node }
            })
            .collect(),
        NodeKind::Schema { db, schema } => cirrus_core::pg::session(&db.project, &db.instance, &db.database)
            .await?
            .tables(&schema.name)
            .await?
            .into_iter()
            .map(|table| {
                let label = table.name.clone();
                let detail = if table.kind.is_view() {
                    table.kind.label().to_string()
                } else if table.estimated_rows >= 0. {
                    format!("~{}", format_count(table.estimated_rows as i64))
                } else {
                    String::new()
                };
                Node::new(NodeKind::Table { db: db.clone(), table }, label).with_detail(detail)
            })
            .collect(),
        NodeKind::Table { .. } => vec![],
    })
}

impl Render for Navigator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let body = match &self.roots {
            Children::Loading | Children::NotLoaded => h_flex()
                .p_3()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(Spinner::new().small())
                .child("Loading projects…")
                .into_any_element(),
            Children::Failed(e) => v_flex()
                .p_3()
                .gap_2()
                .text_sm()
                .child(div().text_color(theme.danger).child(e.clone()))
                .child(
                    Button::new("retry")
                        .small()
                        .label("Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.reload_projects(cx))),
                )
                .into_any_element(),
            Children::Loaded(_) => uniform_list(
                "navigator-rows",
                self.rows.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                    range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll)
            .size_full()
            .px_1()
            .into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .child(div().flex_1().child(Input::new(&self.filter).small().cleanable(true).prefix(Icon::new(I::Search).small())))
                    .child(
                        Button::new("reload-projects")
                            .ghost()
                            .small()
                            .icon(Icon::new(I::RefreshCw))
                            .tooltip("Reload projects")
                            .on_click(cx.listener(|this, _, _, cx| this.reload_projects(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}
