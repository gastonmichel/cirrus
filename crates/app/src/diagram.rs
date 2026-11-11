//! ER diagram of one schema: draggable table nodes, foreign-key edges, pan & zoom,
//! and a detail panel for the selected table.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::assets::IconName as I;
use gpui_kit::component::button::*;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::*;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use cirrus_core::{ColumnInfo, SchemaModel, TableInfo};

use crate::er_layout::{self, HEADER_HEIGHT, MAX_VISIBLE_COLUMNS, NODE_WIDTH, ROW_HEIGHT};
use crate::tree::DbRef;

const MIN_ZOOM: f32 = 0.15;
const MAX_ZOOM: f32 = 2.0;
/// Below this zoom level nodes show only their name.
const COMPACT_ZOOM: f32 = 0.45;
/// "Fit" never zooms out further than this (keeps column names legible).
const FIT_MIN_ZOOM: f32 = 0.5;

pub enum DiagramEvent {
    OpenTable(DbRef, TableInfo),
}

impl EventEmitter<DiagramEvent> for DiagramView {}

struct Edge {
    from: usize,
    from_col: Option<usize>,
    to: usize,
    to_col: Option<usize>,
}

enum Drag {
    Node { ix: usize, start_mouse: Point<Pixels>, start_pos: Point<f32> },
    Pan { start_mouse: Point<Pixels>, start_pan: Point<Pixels>, moved: bool },
}

enum State {
    Loading,
    Failed(String),
    Ready(SchemaModel),
}

pub struct DiagramView {
    db: DbRef,
    schema: String,
    state: State,
    /// Node positions in diagram space (unscaled).
    positions: Vec<Point<f32>>,
    heights: Vec<f32>,
    edges: Vec<Edge>,
    pan: Point<Pixels>,
    zoom: f32,
    drag: Option<Drag>,
    selected: Option<usize>,
    /// Canvas bounds from the last paint (window coordinates).
    viewport: Rc<Cell<Bounds<Pixels>>>,
    needs_fit: bool,
}

impl DiagramView {
    pub fn new(db: DbRef, schema: String, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            db,
            schema,
            state: State::Loading,
            positions: vec![],
            heights: vec![],
            edges: vec![],
            pan: point(px(24.), px(24.)),
            zoom: 1.,
            drag: None,
            selected: None,
            viewport: Rc::new(Cell::new(Bounds::default())),
            needs_fit: false,
        };
        this.load(cx);
        this
    }

    pub fn title(&self) -> String {
        format!("ER · {}", self.schema)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.state = State::Loading;
        self.selected = None;
        let (db, schema) = (self.db.clone(), self.schema.clone());
        cx.spawn(async move |this, cx| {
            let result = cirrus_core::runtime::spawn(async move {
                cirrus_core::pg::session(&db.project, &db.instance, &db.database)
                    .await?
                    .schema_model(&schema)
                    .await
            })
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(model) => this.set_model(model),
                    Err(e) => this.state = State::Failed(format!("{e:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn set_model(&mut self, model: SchemaModel) {
        let index: HashMap<&str, usize> = model.tables.iter().enumerate().map(|(i, (t, _))| (t.name.as_str(), i)).collect();
        let col_ix = |table: usize, col: Option<&String>| {
            col.and_then(|c| model.tables[table].1.iter().position(|ci| &ci.name == c))
        };
        self.edges = model
            .foreign_keys
            .iter()
            .filter(|fk| fk.ref_schema == model.schema)
            .filter_map(|fk| {
                let from = *index.get(fk.table.as_str())?;
                let to = *index.get(fk.ref_table.as_str())?;
                Some(Edge { from, from_col: col_ix(from, fk.columns.first()), to, to_col: col_ix(to, fk.ref_columns.first()) })
            })
            .collect();
        self.heights = model.tables.iter().map(|(_, cols)| er_layout::node_height(cols.len())).collect();
        self.state = State::Ready(model);
        self.relayout();
    }

    fn relayout(&mut self) {
        let edges: Vec<(usize, usize)> = self.edges.iter().map(|e| (e.from, e.to)).collect();
        let viewport = self.viewport.get().size;
        let aspect = if viewport.height > px(0.) { f32::from(viewport.width) / f32::from(viewport.height) } else { 1.6 };
        self.positions =
            er_layout::layout(&self.heights, &edges, aspect).into_iter().map(|(x, y)| point(x, y)).collect();
        self.needs_fit = true;
    }

    fn content_bounds(&self) -> Option<(Point<f32>, Point<f32>)> {
        if self.positions.is_empty() {
            return None;
        }
        let mut min = point(f32::MAX, f32::MAX);
        let mut max = point(f32::MIN, f32::MIN);
        for (p, h) in self.positions.iter().zip(&self.heights) {
            min = point(min.x.min(p.x), min.y.min(p.y));
            max = point(max.x.max(p.x + NODE_WIDTH), max.y.max(p.y + h));
        }
        Some((min, max))
    }

    fn fit(&mut self) {
        let viewport = self.viewport.get();
        let Some((min, max)) = self.content_bounds() else { return };
        let (vw, vh) = (f32::from(viewport.size.width), f32::from(viewport.size.height));
        if vw <= 0. || vh <= 0. {
            return;
        }
        let pad = 40.;
        // Never zoom out so far that column names become unreadable; pan instead.
        let zoom = ((vw - pad * 2.) / (max.x - min.x)).min((vh - pad * 2.) / (max.y - min.y)).clamp(FIT_MIN_ZOOM, 1.);
        self.zoom = zoom;
        // Centre each axis when it fits, otherwise start at the top-left corner.
        let offset = |view: f32, lo: f32, hi: f32| ((view - (hi - lo) * zoom) / 2.).max(pad) - lo * zoom;
        self.pan = point(px(offset(vw, min.x, max.x)), px(offset(vh, min.y, max.y)));
        self.needs_fit = false;
    }

    /// Zooms keeping the diagram point under `anchor` (window coordinates) fixed.
    fn zoom_at(&mut self, factor: f32, anchor: Point<Pixels>) {
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let local = anchor - self.viewport.get().origin;
        let dx = (f32::from(local.x) - f32::from(self.pan.x)) / self.zoom;
        let dy = (f32::from(local.y) - f32::from(self.pan.y)) / self.zoom;
        self.pan = point(px(f32::from(local.x) - dx * new_zoom), px(f32::from(local.y) - dy * new_zoom));
        self.zoom = new_zoom;
    }

    fn zoom_center(&mut self, factor: f32, cx: &mut Context<Self>) {
        let vp = self.viewport.get();
        self.zoom_at(factor, vp.center());
        cx.notify();
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = ev.delta.pixel_delta(px(16.));
        if ev.modifiers.platform || ev.modifiers.control {
            let factor = (1. + f32::from(delta.y) * 0.01).clamp(0.5, 2.);
            self.zoom_at(factor, ev.position);
        } else {
            self.pan += delta;
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        match &mut self.drag {
            Some(Drag::Node { ix, start_mouse, start_pos }) => {
                let d = ev.position - *start_mouse;
                self.positions[*ix] =
                    point(start_pos.x + f32::from(d.x) / self.zoom, start_pos.y + f32::from(d.y) / self.zoom);
                cx.notify();
            }
            Some(Drag::Pan { start_mouse, start_pan, moved }) => {
                let d = ev.position - *start_mouse;
                *moved |= f32::from(d.x).abs() + f32::from(d.y).abs() > 3.;
                self.pan = *start_pan + d;
                cx.notify();
            }
            None => {}
        }
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(Drag::Pan { moved: false, .. }) = self.drag {
            self.selected = None;
        }
        self.drag = None;
        cx.notify();
    }

    fn render_node(&self, ix: usize, table: &TableInfo, columns: &[ColumnInfo], cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let z = self.zoom;
        let pos = self.positions[ix];
        let selected = self.selected == Some(ix);
        let related = self.selected.is_some_and(|s| {
            self.edges.iter().any(|e| (e.from == s && e.to == ix) || (e.to == s && e.from == ix))
        });
        let border = if selected {
            theme.primary
        } else if related {
            theme.primary.opacity(0.5)
        } else {
            theme.border
        };
        let compact = z < COMPACT_ZOOM;

        // Everything scales with the zoom, including line heights, so text stays inside its box.
        // Compact nodes are just a name filling the whole box.
        let title_size = if compact { (12.5 * z * 1.8).clamp(6., 14.) } else { 12.5 * z };
        let header = h_flex()
            .when(compact, |el| el.h_full().justify_center())
            .when(!compact, |el| el.h(px(HEADER_HEIGHT * z)).border_b_1().border_color(theme.border))
            .flex_shrink_0()
            .px(px(8. * z))
            .gap(px(6. * z))
            .bg(if selected { theme.primary.opacity(0.15) } else { theme.secondary })
            .when(!compact, |el| {
                el.child(
                    Icon::new(if table.kind.is_view() { I::Eye } else { I::Table })
                        .size(px(13. * z))
                        .text_color(theme.muted_foreground),
                )
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(title_size))
                    .line_height(px(title_size * 1.3))
                    .child(table.name.clone()),
            );

        let rows = columns.iter().take(MAX_VISIBLE_COLUMNS).map(|c| {
            h_flex()
                .h(px(ROW_HEIGHT * z))
                .flex_shrink_0()
                .px(px(8. * z))
                .gap(px(6. * z))
                .text_size(px(11.5 * z))
                .line_height(px(ROW_HEIGHT * z))
                .child(
                    div().w(px(12. * z)).flex_shrink_0().children(if c.is_primary_key {
                        Some(Icon::new(I::KeyRound).size(px(11. * z)).text_color(theme.yellow))
                    } else if c.is_foreign_key {
                        Some(Icon::new(I::Link).size(px(11. * z)).text_color(theme.blue))
                    } else {
                        None
                    }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .when(c.is_primary_key, |el| el.font_weight(FontWeight::SEMIBOLD))
                        .child(c.name.clone()),
                )
                .child(
                    div()
                        .max_w(px(100. * z))
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(c.data_type.clone()),
                )
        });
        let more = columns.len().saturating_sub(MAX_VISIBLE_COLUMNS);

        v_flex()
            .id(("er-node", ix))
            .absolute()
            .left(self.pan.x + px(pos.x * z))
            .top(self.pan.y + px(pos.y * z))
            .w(px(NODE_WIDTH * z))
            .h(px(self.heights[ix] * z))
            .overflow_hidden()
            .bg(theme.popover)
            .border_1()
            .border_color(border)
            .rounded(px(6. * z))
            .shadow_sm()
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.selected = Some(ix);
                    this.drag = Some(Drag::Node { ix, start_mouse: ev.position, start_pos: this.positions[ix] });
                    cx.notify();
                }),
            )
            .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                if ev.click_count() >= 2
                    && let State::Ready(model) = &this.state
                {
                    cx.emit(DiagramEvent::OpenTable(this.db.clone(), model.tables[ix].0.clone()));
                }
            }))
            .child(header)
            .when(!compact, |el| {
                el.py(px(4. * z)).children(rows).when(more > 0, |el| {
                    el.child(
                        div()
                            .h(px(ROW_HEIGHT * z))
                            .px(px(26. * z))
                            .text_size(px(11. * z))
                            .text_color(theme.muted_foreground)
                            .child(format!("+ {more} more…")),
                    )
                })
            })
            .into_any_element()
    }

    /// Paints FK edges; captures everything it needs by value since canvas closures are `'static`.
    fn edges_canvas(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let normal = theme.muted_foreground.opacity(0.55);
        let highlight = theme.primary;
        let z = self.zoom;
        let pan = self.pan;
        let selected = self.selected;
        let viewport = self.viewport.clone();

        // Anchor y-offset (unscaled) of a column row inside a node.
        let row_y = |col: Option<usize>| match col {
            Some(c) => HEADER_HEIGHT + 4. + (c.min(MAX_VISIBLE_COLUMNS) as f32 + 0.5) * ROW_HEIGHT,
            None => HEADER_HEIGHT / 2.,
        };
        let edges: Vec<_> = self
            .edges
            .iter()
            .map(|e| {
                let a = self.positions[e.from];
                let b = self.positions[e.to];
                let hot = selected.is_some_and(|s| s == e.from || s == e.to);
                (a, row_y(e.from_col), b, row_y(e.to_col), e.from == e.to, hot)
            })
            .collect();

        canvas(
            move |bounds, _, _| viewport.set(bounds),
            move |bounds, _, window, _| {
                let origin = bounds.origin + pan;
                let to_screen = |x: f32, y: f32| origin + point(px(x * z), px(y * z));
                // Draw normal edges first so highlighted ones end up on top.
                for pass_hot in [false, true] {
                    for &(a, ay, b, by, self_ref, hot) in &edges {
                        if hot != pass_hot {
                            continue;
                        }
                        let color = if hot { highlight } else { normal };
                        let (start, end, c1, c2);
                        if self_ref {
                            start = to_screen(a.x + NODE_WIDTH, a.y + ay);
                            end = to_screen(a.x + NODE_WIDTH, a.y + by);
                            c1 = to_screen(a.x + NODE_WIDTH + 50., a.y + ay);
                            c2 = to_screen(a.x + NODE_WIDTH + 50., a.y + by);
                        } else if b.x + NODE_WIDTH < a.x {
                            // Target is to the left.
                            start = to_screen(a.x, a.y + ay);
                            end = to_screen(b.x + NODE_WIDTH, b.y + by);
                            let dx = ((a.x - b.x - NODE_WIDTH) / 2.).max(30.);
                            c1 = to_screen(a.x - dx, a.y + ay);
                            c2 = to_screen(b.x + NODE_WIDTH + dx, b.y + by);
                        } else if a.x + NODE_WIDTH < b.x {
                            // Target is to the right.
                            start = to_screen(a.x + NODE_WIDTH, a.y + ay);
                            end = to_screen(b.x, b.y + by);
                            let dx = ((b.x - a.x - NODE_WIDTH) / 2.).max(30.);
                            c1 = to_screen(a.x + NODE_WIDTH + dx, a.y + ay);
                            c2 = to_screen(b.x - dx, b.y + by);
                        } else {
                            // Vertically stacked: loop around the left side.
                            start = to_screen(a.x, a.y + ay);
                            end = to_screen(b.x, b.y + by);
                            let left = a.x.min(b.x) - 60.;
                            c1 = to_screen(left, a.y + ay);
                            c2 = to_screen(left, b.y + by);
                        }
                        let mut path = PathBuilder::stroke(px(if hot { 2. } else { 1.25 }));
                        path.move_to(start);
                        path.cubic_bezier_to(end, c1, c2);
                        if let Ok(path) = path.build() {
                            window.paint_path(path, color);
                        }
                        // Many-side dot at the referencing column, bar at the referenced key.
                        let r = px(3. * z.max(0.5));
                        window.paint_quad(
                            fill(Bounds::new(start - point(r, r), size(r * 2., r * 2.)), color).corner_radii(r),
                        );
                        let bar = Bounds::new(end - point(px(1.5), r * 1.6), size(px(3.), r * 3.2));
                        window.paint_quad(fill(bar, color));
                    }
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    fn render_details(&self, model: &SchemaModel, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (table, columns) = &model.tables[ix];
        let outgoing: Vec<_> = model.foreign_keys.iter().filter(|fk| fk.table == table.name).collect();
        let incoming: Vec<_> = model
            .foreign_keys
            .iter()
            .filter(|fk| fk.ref_table == table.name && fk.ref_schema == model.schema)
            .collect();
        let section = |title: &str| {
            div()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(title.to_uppercase())
        };
        let open = {
            let (db, table) = (self.db.clone(), table.clone());
            cx.listener(move |_, _, _, cx| cx.emit(DiagramEvent::OpenTable(db.clone(), table.clone())))
        };

        v_flex()
            .w(px(320.))
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .p_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(if table.kind.is_view() { I::Eye } else { I::Table }))
                    .child(div().flex_1().truncate().font_weight(FontWeight::SEMIBOLD).child(table.name.clone()))
                    .child(
                        Button::new("close-details")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(I::X))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.selected = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div().id("er-details").flex_1().min_h_0().px_3().pb_3().overflow_y_scrollbar().child(
                    v_flex()
                        .text_sm()
                        .child(
                            div().pt_2().text_xs().text_color(theme.muted_foreground).child(format!(
                                "{}{}",
                                table.kind.label(),
                                if table.estimated_rows >= 0. {
                                    format!(" · ~{} rows", crate::table_view::format_count(table.estimated_rows as i64))
                                } else {
                                    String::new()
                                }
                            )),
                        )
                        .children(table.comment.clone().map(|c| div().pt_1().text_xs().child(c)))
                        .child(section(&format!("Columns ({})", columns.len())))
                        .children(columns.iter().map(|c| {
                            h_flex()
                                .py_0p5()
                                .gap_2()
                                .child(div().flex_1().min_w_0().truncate().child(c.name.clone()))
                                .when(c.is_primary_key, |el| el.child(Tag::warning().xsmall().child("PK")))
                                .when(c.is_foreign_key, |el| el.child(Tag::info().xsmall().child("FK")))
                                .child(
                                    div()
                                        .max_w(px(130.))
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("{}{}", c.data_type, if c.nullable { "" } else { " !" })),
                                )
                        }))
                        .when(!outgoing.is_empty(), |el| {
                            el.child(section("References")).children(outgoing.iter().map(|fk| {
                                div().py_0p5().text_xs().child(format!(
                                    "({}) → {}{}({})",
                                    fk.columns.join(", "),
                                    if fk.ref_schema != model.schema { format!("{}.", fk.ref_schema) } else { String::new() },
                                    fk.ref_table,
                                    fk.ref_columns.join(", ")
                                ))
                            }))
                        })
                        .when(!incoming.is_empty(), |el| {
                            el.child(section("Referenced by")).children(incoming.iter().map(|fk| {
                                div()
                                    .py_0p5()
                                    .text_xs()
                                    .child(format!("{}({}) ", fk.table, fk.columns.join(", ")))
                            }))
                        }),
                ),
            )
            .child(
                div().p_3().border_t_1().border_color(theme.border).child(
                    Button::new("open-table")
                        .primary()
                        .small()
                        .w_full()
                        .icon(Icon::new(I::Table))
                        .label("Open table")
                        .on_click(open),
                ),
            )
            .into_any_element()
    }
}

impl Render for DiagramView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.needs_fit {
            if self.viewport.get().size.width > px(0.) {
                self.fit();
            } else {
                window.request_animation_frame();
            }
        }
        let theme = cx.theme().clone();

        let toolbar = h_flex()
            .px_3()
            .py_1p5()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .child(Icon::new(I::Workflow).text_color(theme.muted_foreground))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{} · {} / {}", self.schema, self.db.instance, self.db.database)),
            )
            .children(match &self.state {
                State::Ready(model) => Some(
                    div()
                        .pl_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{} tables · {} relationships", model.tables.len(), self.edges.len())),
                ),
                _ => None,
            })
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .pr_1()
                    .child("Drag to pan · ⌘-scroll to zoom · double-click to open"),
            )
            .child(
                Button::new("zoom-out")
                    .ghost()
                    .small()
                    .icon(Icon::new(I::ZoomOut))
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_center(1. / 1.2, cx))),
            )
            .child(div().w(px(44.)).text_center().text_xs().child(format!("{:.0}%", self.zoom * 100.)))
            .child(
                Button::new("zoom-in")
                    .ghost()
                    .small()
                    .icon(Icon::new(I::ZoomIn))
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_center(1.2, cx))),
            )
            .child(
                Button::new("fit")
                    .ghost()
                    .small()
                    .icon(Icon::new(I::Maximize))
                    .tooltip("Fit to window")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.fit();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("relayout")
                    .ghost()
                    .small()
                    .icon(Icon::new(I::LayoutGrid))
                    .tooltip("Auto-arrange")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.relayout();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("reload")
                    .ghost()
                    .small()
                    .icon(Icon::new(I::RefreshCw))
                    .tooltip("Reload schema")
                    .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
            );

        let body = match &self.state {
            State::Loading => h_flex()
                .flex_1()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(Spinner::new().small())
                .child("Reading schema…")
                .into_any_element(),
            State::Failed(e) => div().flex_1().p_4().text_sm().text_color(theme.danger).child(e.clone()).into_any_element(),
            State::Ready(model) if model.tables.is_empty() => h_flex()
                .flex_1()
                .justify_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("This schema has no tables or views.")
                .into_any_element(),
            State::Ready(model) => {
                let nodes: Vec<_> = model
                    .tables
                    .iter()
                    .enumerate()
                    .map(|(ix, (t, cols))| self.render_node(ix, t, cols, cx))
                    .collect();
                let details = self.selected.map(|ix| self.render_details(model, ix, cx));
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("er-canvas")
                            .relative()
                            .flex_1()
                            .h_full()
                            .overflow_hidden()
                            .bg(theme.background)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                                    this.drag =
                                        Some(Drag::Pan { start_mouse: ev.position, start_pan: this.pan, moved: false });
                                    cx.notify();
                                }),
                            )
                            .on_mouse_move(cx.listener(Self::on_mouse_move))
                            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)))
                            .on_mouse_up_out(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                    this.drag = None;
                                    cx.notify();
                                }),
                            )
                            .on_scroll(cx.listener(Self::on_scroll))
                            .on_pinch(cx.listener(|this, ev: &PinchEvent, _, cx| {
                                this.zoom_at(1. + ev.delta, ev.position);
                                cx.notify();
                            }))
                            .child(self.edges_canvas(cx))
                            .children(nodes),
                    )
                    .children(details)
                    .into_any_element()
            }
        };

        v_flex().size_full().child(toolbar).child(body)
    }
}
