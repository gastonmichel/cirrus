//! A table/view tab: paged data with server-side sort and filter, plus structure and DDL.

use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName as I;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::table::ColumnSort;
use gpui_kit::component::*;
use gpui_kit::component::tab::{Tab as TabItem, TabBar};
use gpui_kit::*;
use cirrus_core::pg::Session;
use cirrus_core::{PageRequest, QueryResult, ResultColumn, SortDirection, TableDetails, TableInfo};

use crate::grid::ResultView;
use crate::tree::DbRef;

const PAGE_SIZE: usize = 200;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Data,
    Columns,
    Constraints,
    Indexes,
    Ddl,
}

const TABS: [(Tab, &str); 5] = [
    (Tab::Data, "Data"),
    (Tab::Columns, "Columns"),
    (Tab::Constraints, "Constraints"),
    (Tab::Indexes, "Indexes"),
    (Tab::Ddl, "DDL"),
];

pub struct TableView {
    db: DbRef,
    table: TableInfo,
    tab: Tab,
    data: Entity<ResultView>,
    filter: Entity<InputState>,
    applied_filter: Option<String>,
    order: Option<(String, SortDirection)>,
    /// Incremented per data request so stale responses are ignored.
    generation: u64,
    details: Option<Result<TableDetails, String>>,
    columns: Entity<ResultView>,
    constraints: Entity<ResultView>,
    indexes: Entity<ResultView>,
    ddl: Entity<EditorState>,
    _subscriptions: Vec<Subscription>,
}

impl TableView {
    pub fn new(db: DbRef, table: TableInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = format!("{}.{}", table.schema, table.name);
        let data = cx.new(|cx| ResultView::new(name.clone(), window, cx));
        let columns = cx.new(|cx| ResultView::new(format!("{name}_columns"), window, cx));
        let constraints = cx.new(|cx| ResultView::new(format!("{name}_constraints"), window, cx));
        let indexes = cx.new(|cx| ResultView::new(format!("{name}_indexes"), window, cx));
        let ddl = cx.new(|cx| EditorState::new(window, cx).language("sql").default_value("-- loading…"));
        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder("WHERE …   e.g.  status = 'active' AND created_at > now() - interval '7 days'")
        });

        let this = cx.entity().downgrade();
        let on_sort = {
            let this = this.clone();
            Rc::new(move |col: String, sort: ColumnSort, _: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| {
                    this.order = match sort {
                        ColumnSort::Ascending => Some((col, SortDirection::Asc)),
                        ColumnSort::Descending => Some((col, SortDirection::Desc)),
                        _ => None,
                    };
                    this.fetch(None, cx);
                })
                .ok();
            })
        };
        let on_load_more = Rc::new(move |offset: usize, _: &mut Window, cx: &mut App| {
            this.update(cx, |this, cx| this.fetch(Some(offset), cx)).ok();
        });
        data.update(cx, |d, cx| {
            d.on_sort(on_sort, cx);
            d.on_load_more(on_load_more, cx);
        });

        let subscriptions = vec![cx.subscribe_in(&filter, window, |this, _, ev: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.apply_filter(cx);
            }
        })];

        let mut this = Self {
            db,
            table,
            tab: Tab::Data,
            data,
            filter,
            applied_filter: None,
            order: None,
            generation: 0,
            details: None,
            columns,
            constraints,
            indexes,
            ddl,
            _subscriptions: subscriptions,
        };
        this.data.update(cx, |d, cx| d.set_loading(true, cx));
        this.fetch(None, cx);
        this.load_details(window, cx);
        this
    }

    pub fn title(&self) -> String {
        self.table.name.clone()
    }

    async fn session(db: &DbRef) -> anyhow::Result<Arc<Session>> {
        let (p, i, d) = (db.project.clone(), db.instance.clone(), db.database.clone());
        cirrus_core::runtime::spawn(async move { cirrus_core::pg::session(&p, &i, &d).await }).await
    }

    /// Fetches the first page (replacing rows), or appends the page at `append_at`.
    /// Never touches the grid's table synchronously: it may be called from inside the table's callbacks.
    fn fetch(&mut self, append_at: Option<usize>, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let append = append_at.is_some();
        let offset = append_at.unwrap_or(0);
        let req = PageRequest {
            filter: self.applied_filter.clone(),
            order_by: self.order.clone(),
            limit: PAGE_SIZE,
            offset,
        };
        let db = self.db.clone();
        let (schema, table) = (self.table.schema.clone(), self.table.name.clone());
        cx.spawn(async move |this, cx| {
            let result = async {
                let session = Self::session(&db).await?;
                cirrus_core::runtime::spawn(async move { session.table_page(&schema, &table, &req).await }).await
            }
            .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.data.update(cx, |d, cx| match result {
                    Ok(page) if append => d.append_rows(page, cx),
                    // First load (or the shape changed): rebuild columns. Otherwise keep widths and sort state.
                    Ok(page) if page.columns.len() != d.result(cx).columns.len() => d.set_result(page, cx),
                    Ok(page) => d.replace_rows(page, cx),
                    Err(e) => d.set_error(format!("{e:#}"), cx),
                });
            })
            .ok();
        })
        .detach();
    }

    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter.read(cx).value().trim().to_string();
        self.applied_filter = (!filter.is_empty()).then_some(filter);
        self.data.update(cx, |d, cx| {
            d.status = None;
            d.set_loading(true, cx);
        });
        self.fetch(None, cx);
    }

    fn count_rows(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        let (schema, table, filter) = (self.table.schema.clone(), self.table.name.clone(), self.applied_filter.clone());
        self.data.update(cx, |d, cx| {
            d.status = Some("counting…".into());
            cx.notify();
        });
        cx.spawn(async move |this, cx| {
            let count = async {
                let session = Self::session(&db).await?;
                cirrus_core::runtime::spawn(async move { session.table_count(&schema, &table, filter.as_deref()).await }).await
            }
            .await;
            this.update(cx, |this, cx| {
                this.data.update(cx, |d, cx| {
                    d.status = Some(match count {
                        Ok(n) => format!("{} rows total", format_count(n)),
                        Err(e) => format!("count failed: {e}"),
                    });
                    cx.notify();
                });
            })
            .ok();
        })
        .detach();
    }

    fn load_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let db = self.db.clone();
        let table = self.table.clone();
        cx.spawn_in(window, async move |this, cx| {
            let details = async {
                let session = Self::session(&db).await?;
                cirrus_core::runtime::spawn(async move { session.table_details(&table).await }).await
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                match &details {
                    Ok(d) => {
                        this.columns.update(cx, |v, cx| v.set_result(columns_result(d), cx));
                        this.constraints.update(cx, |v, cx| v.set_result(constraints_result(d), cx));
                        this.indexes.update(cx, |v, cx| v.set_result(indexes_result(d), cx));
                        let ddl = d.ddl.clone();
                        this.ddl.update(cx, |e, cx| e.set_value(ddl, window, cx));
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        for v in [&this.columns, &this.constraints, &this.indexes] {
                            v.update(cx, |v, cx| v.set_error(msg.clone(), cx));
                        }
                        this.ddl.update(cx, |ed, cx| ed.set_value(format!("-- {msg}"), window, cx));
                    }
                }
                this.details = Some(details.map_err(|e| format!("{e:#}")));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.data.update(cx, |d, cx| {
            d.status = None;
            d.set_loading(true, cx);
        });
        self.fetch(None, cx);
        self.load_details(window, cx);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut meta = vec![self.table.kind.label().to_string()];
        if self.table.estimated_rows >= 0. {
            meta.push(format!("~{} rows", format_count(self.table.estimated_rows as i64)));
        }
        if let Some(Ok(d)) = &self.details
            && let Some(bytes) = d.total_size_bytes
        {
            meta.push(format_bytes(bytes));
        }
        meta.push(format!("{} · {}", self.db.instance, self.db.database));

        h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .child(Icon::new(if self.table.kind.is_view() { I::Eye } else { I::Table }).text_color(theme.muted_foreground))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{}.{}", self.table.schema, self.table.name)),
            )
            .child(div().text_xs().text_color(theme.muted_foreground).child(meta.join("  ·  ")))
            .child(div().flex_1())
            .child(
                Button::new("refresh")
                    .ghost()
                    .small()
                    .icon(Icon::new(I::RefreshCw))
                    .tooltip("Reload data and structure")
                    .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
            )
    }
}

impl Render for TableView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let selected = TABS.iter().position(|(t, _)| *t == self.tab).unwrap_or(0);
        let tabs = TabBar::new("table-tabs")
            .underline()
            .small()
            .selected_index(selected)
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.tab = TABS[*ix].0;
                cx.notify();
            }))
            .children(TABS.iter().map(|(_, label)| TabItem::new().label(*label)));

        let body = match self.tab {
            Tab::Data => v_flex()
                .size_full()
                .child(
                    h_flex()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div().flex_1().child(
                                Input::new(&self.filter)
                                    .small()
                                    .cleanable(true)
                                    .prefix(div().text_xs().text_color(theme.muted_foreground).child("WHERE")),
                            ),
                        )
                        .child(
                            Button::new("apply-filter")
                                .small()
                                .icon(Icon::new(I::Funnel))
                                .label("Apply")
                                .on_click(cx.listener(|this, _, _, cx| this.apply_filter(cx))),
                        )
                        .child(
                            Button::new("count")
                                .small()
                                .ghost()
                                .icon(Icon::new(I::Hash))
                                .label("Count")
                                .tooltip("Run SELECT count(*) with the current filter")
                                .on_click(cx.listener(|this, _, _, cx| this.count_rows(cx))),
                        ),
                )
                .child(div().flex_1().min_h_0().child(self.data.clone()))
                .into_any_element(),
            Tab::Columns => self.columns.clone().into_any_element(),
            Tab::Constraints => self.constraints.clone().into_any_element(),
            Tab::Indexes => self.indexes.clone().into_any_element(),
            Tab::Ddl => div().size_full().p_2().child(Editor::new(&self.ddl).h_full().readonly(true)).into_any_element(),
        };

        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .child(div().px_2().border_b_1().border_color(cx.theme().border).child(tabs))
            .child(div().flex_1().min_h_0().child(body))
    }
}

fn text_result(columns: &[&str], rows: Vec<Vec<Option<String>>>) -> QueryResult {
    QueryResult {
        columns: columns.iter().map(|c| ResultColumn { name: c.to_string(), type_name: String::new() }).collect(),
        rows,
        ..Default::default()
    }
}

fn columns_result(d: &TableDetails) -> QueryResult {
    let rows = d
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let key = match (c.is_primary_key, c.is_foreign_key) {
                (true, true) => "PK, FK",
                (true, false) => "PK",
                (false, true) => "FK",
                _ => "",
            };
            vec![
                Some((i + 1).to_string()),
                Some(c.name.clone()),
                Some(c.data_type.clone()),
                Some(if c.nullable { "YES" } else { "NO" }.into()),
                Some(key.into()),
                c.default.clone(),
                c.comment.clone(),
            ]
        })
        .collect();
    text_result(&["#", "column", "type", "nullable", "key", "default", "comment"], rows)
}

fn constraints_result(d: &TableDetails) -> QueryResult {
    let rows = d
        .constraints
        .iter()
        .map(|c| vec![Some(c.name.clone()), Some(c.kind_label().into()), Some(c.columns.join(", ")), Some(c.definition.clone())])
        .collect();
    text_result(&["name", "type", "columns", "definition"], rows)
}

fn indexes_result(d: &TableDetails) -> QueryResult {
    let rows = d
        .indexes
        .iter()
        .map(|i| {
            vec![
                Some(i.name.clone()),
                Some(if i.is_primary { "primary" } else if i.is_unique { "unique" } else { "" }.into()),
                Some(i.definition.clone()),
            ]
        })
        .collect();
    text_result(&["name", "kind", "definition"], rows)
}

pub fn format_count(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn format_bytes(bytes: i64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024. && u < units.len() - 1 {
        v /= 1024.;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", units[u]) }
}
