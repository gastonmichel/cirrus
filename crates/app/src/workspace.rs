//! Root view: Google sign-in gate, title bar, navigator sidebar and the tabbed main area.

use gpui_kit::assets::IconName as I;
use gpui_kit::component::button::*;
use gpui_kit::component::*;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use cirrus_core::TableInfo;

use crate::CloseTab;
use crate::console::ConsoleView;
use crate::diagram::{DiagramEvent, DiagramView};
use crate::navigator::{Navigator, NavigatorEvent};
use crate::table_view::TableView;
use crate::tree::DbRef;

enum Auth {
    Checking,
    /// `notice` replaces the default prompt text; `error` is shown as a red details box.
    SignedOut { notice: Option<String>, error: Option<String> },
    /// `gcloud auth login` is running; `url` is the sign-in page once gcloud prints it.
    SigningIn { url: Option<String> },
    SignedIn(String),
}

impl Auth {
    fn signed_out() -> Self {
        Auth::SignedOut { notice: None, error: None }
    }

    fn notice(text: &str) -> Self {
        Auth::SignedOut { notice: Some(text.into()), error: None }
    }

    fn error(e: anyhow::Error) -> Self {
        Auth::SignedOut { notice: None, error: Some(format!("{e:#}")) }
    }
}

/// What a tab shows. Add a variant (and its arms below) to support a new kind of tab.
enum TabContent {
    Table(Entity<TableView>),
    Console(Entity<ConsoleView>),
    Diagram(Entity<DiagramView>),
}

struct OpenTab {
    /// Identity used to re-activate an existing tab instead of opening a duplicate.
    key: String,
    content: TabContent,
}

impl OpenTab {
    fn title(&self, cx: &App) -> String {
        match &self.content {
            TabContent::Table(v) => v.read(cx).title(),
            TabContent::Console(v) => v.read(cx).title(),
            TabContent::Diagram(v) => v.read(cx).title(),
        }
    }

    fn icon(&self) -> I {
        match &self.content {
            TabContent::Table(_) => I::Table,
            TabContent::Console(_) => I::SquareTerminal,
            TabContent::Diagram(_) => I::Workflow,
        }
    }

    fn view(&self) -> AnyView {
        match &self.content {
            TabContent::Table(v) => v.clone().into(),
            TabContent::Console(v) => v.clone().into(),
            TabContent::Diagram(v) => v.clone().into(),
        }
    }
}

pub struct Workspace {
    auth: Auth,
    navigator: Entity<Navigator>,
    tabs: Vec<OpenTab>,
    active: usize,
    console_count: usize,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let navigator = cx.new(|cx| Navigator::new(window, cx));
        let subscriptions = vec![cx.subscribe_in(&navigator, window, |this, _, ev: &NavigatorEvent, window, cx| {
            match ev {
                NavigatorEvent::OpenTable(db, table) => this.open_table(db.clone(), table.clone(), window, cx),
                NavigatorEvent::OpenConsole(db) => this.open_console(db.clone(), window, cx),
                NavigatorEvent::OpenDiagram(db, schema) => this.open_diagram(db.clone(), schema.clone(), window, cx),
            }
        })];
        let mut this = Self {
            auth: Auth::Checking,
            navigator,
            tabs: vec![],
            active: 0,
            console_count: 0,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.check_auth(cx);
        this
    }

    fn check_auth(&mut self, cx: &mut Context<Self>) {
        self.auth = Auth::Checking;
        cx.spawn(async move |this, cx| {
            let result = cirrus_core::runtime::spawn(async {
                let account = cirrus_core::gcloud::active_account().await?;
                if account.is_some() {
                    cirrus_core::gcloud::access_token().await?;
                }
                Ok(account)
            })
            .await;
            this.update(cx, |this, cx| {
                this.auth = match result {
                    Ok(Some(account)) => {
                        this.navigator.update(cx, |n, cx| n.reload_projects(cx));
                        Auth::SignedIn(account)
                    }
                    Ok(None) => Auth::signed_out(),
                    Err(e) if cirrus_core::gcloud::needs_login(&e) => {
                        Auth::notice("Your Google session has expired. Sign in again to continue.")
                    }
                    Err(e) => Auth::error(e),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn sign_in(&mut self, cx: &mut Context<Self>) {
        self.auth = Auth::SigningIn { url: None };
        let (url_tx, mut url_rx) = tokio::sync::mpsc::unbounded_channel();
        cx.spawn(async move |this, cx| {
            while let Some(found) = url_rx.recv().await {
                this.update(cx, |this, cx| {
                    if let Auth::SigningIn { url } = &mut this.auth {
                        *url = Some(found);
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let result = cirrus_core::runtime::spawn(cirrus_core::gcloud::login(url_tx)).await;
            this.update(cx, |this, cx| match result {
                Ok(()) => this.check_auth(cx),
                Err(e) => {
                    this.auth = if format!("{e}").contains("cancelled") { Auth::signed_out() } else { Auth::error(e) };
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Activates the tab with `key`; returns false when no such tab is open.
    fn activate(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.tabs.iter().position(|t| t.key == key) else { return false };
        self.active = ix;
        cx.notify();
        true
    }

    fn add_tab(&mut self, key: String, content: TabContent, cx: &mut Context<Self>) {
        self.tabs.push(OpenTab { key, content });
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    fn open_table(&mut self, db: DbRef, table: TableInfo, window: &mut Window, cx: &mut Context<Self>) {
        let key = format!("table:{}/{}/{}/{}.{}", db.project, db.instance, db.database, table.schema, table.name);
        if !self.activate(&key, cx) {
            let view = cx.new(|cx| TableView::new(db, table, window, cx));
            self.add_tab(key, TabContent::Table(view), cx);
        }
    }

    fn open_console(&mut self, db: DbRef, window: &mut Window, cx: &mut Context<Self>) {
        self.console_count += 1;
        let key = format!("console:{}", self.console_count);
        let view = cx.new(|cx| ConsoleView::new(db, None, window, cx));
        self.add_tab(key, TabContent::Console(view), cx);
    }

    fn open_diagram(&mut self, db: DbRef, schema: String, window: &mut Window, cx: &mut Context<Self>) {
        let key = format!("diagram:{}/{}/{}/{}", db.project, db.instance, db.database, schema);
        if self.activate(&key, cx) {
            return;
        }
        let view = cx.new(|cx| DiagramView::new(db, schema, cx));
        self._subscriptions.push(cx.subscribe_in(&view, window, |this, _, ev: &DiagramEvent, window, cx| match ev {
            DiagramEvent::OpenTable(db, table) => this.open_table(db.clone(), table.clone(), window, cx),
        }));
        self.add_tab(key, TabContent::Diagram(view), cx);
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            self.tabs.remove(ix);
            if self.active >= ix && self.active > 0 {
                self.active -= 1;
            }
            cx.notify();
        }
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let is_dark = theme.mode.is_dark();
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .gap_2()
                .child(Icon::new(I::Database).small().text_color(theme.primary))
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Cirrus"))
                .child(Tag::secondary().xsmall().child("read-only"))
                .child(div().flex_1())
                .children(match &self.auth {
                    Auth::SignedIn(account) => Some(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(I::CircleUser).xsmall())
                            .child(account.clone()),
                    ),
                    _ => None,
                })
                .child(
                    Button::new("theme")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(if is_dark { I::Sun } else { I::Moon }))
                        .tooltip("Toggle light/dark")
                        .on_click(move |_, window, cx| {
                            let mode = if is_dark { ThemeMode::Light } else { ThemeMode::Dark };
                            Theme::change(mode, Some(window), cx);
                        }),
                ),
        )
    }

    fn sign_in_in_terminal(&mut self, cx: &mut Context<Self>) {
        cirrus_core::gcloud::cancel_login();
        self.auth = match cirrus_core::gcloud::login_in_terminal() {
            Ok(()) => Auth::notice("Finish signing in in the Terminal window, then click \"Check again\"."),
            Err(e) => Auth::error(e),
        };
        cx.notify();
    }

    fn render_sign_in(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let signing_in = matches!(self.auth, Auth::SigningIn { .. });
        let login_url = match &self.auth {
            Auth::SigningIn { url } => url.clone(),
            _ => None,
        };
        let (busy, message, error): (bool, String, Option<String>) = match &self.auth {
            Auth::Checking => (true, "Checking gcloud credentials…".into(), None),
            Auth::SigningIn { .. } => (true, "Complete the Google sign-in in your browser…".into(), None),
            Auth::SignedOut { notice, error } => (
                false,
                notice.clone().unwrap_or_else(|| "Sign in with your Google account to browse your Cloud SQL instances.".into()),
                error.clone(),
            ),
            Auth::SignedIn(_) => (false, String::new(), None),
        };
        h_flex().size_full().justify_center().bg(theme.background).child(
            v_flex()
                .w(px(440.))
                .p_6()
                .gap_4()
                .items_center()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_md()
                .child(Icon::new(I::DatabaseZap).size(px(40.)).text_color(theme.primary))
                .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Cirrus"))
                .child(div().text_sm().text_center().text_color(theme.muted_foreground).child(message))
                .children(error.map(|e| {
                    div()
                        .id("auth-error")
                        .w_full()
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(theme.radius)
                        .bg(theme.danger.opacity(0.08))
                        .text_xs()
                        .text_color(theme.danger)
                        .child(e)
                }))
                .child(
                    Button::new("sign-in")
                        .primary()
                        .w_full()
                        .icon(Icon::new(I::LogIn))
                        .label("Sign in with Google")
                        .loading(busy)
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| this.sign_in(cx))),
                )
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .justify_center()
                        .when(signing_in, |el| {
                            el.child(
                                Button::new("cancel-sign-in")
                                    .small()
                                    .label("Cancel")
                                    .on_click(|_, _, _| cirrus_core::gcloud::cancel_login()),
                            )
                            .children(login_url.map(|url| {
                                Button::new("reopen-sign-in")
                                    .small()
                                    .ghost()
                                    .icon(Icon::new(I::ExternalLink))
                                    .label("Reopen sign-in page")
                                    .on_click(move |_, _, cx| cx.open_url(&url))
                            }))
                        })
                        .when(!busy, |el| {
                            el.child(
                                Button::new("check-again")
                                    .small()
                                    .icon(Icon::new(I::RefreshCw))
                                    .label("Check again")
                                    .tooltip("Re-check gcloud credentials (e.g. after `gcloud auth login` in a terminal)")
                                    .on_click(cx.listener(|this, _, _, cx| this.check_auth(cx))),
                            )
                        })
                        .when(!matches!(self.auth, Auth::Checking), |el| {
                            el.child(
                                Button::new("terminal-sign-in")
                                    .small()
                                    .ghost()
                                    .icon(Icon::new(I::SquareTerminal))
                                    .label("Sign in via Terminal")
                                    .tooltip("Runs `gcloud auth login` in Terminal, which can also handle Google re-authentication prompts")
                                    .on_click(cx.listener(|this, _, _, cx| this.sign_in_in_terminal(cx))),
                            )
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_center()
                        .text_color(theme.muted_foreground)
                        .child("Uses your gcloud CLI credentials (`gcloud auth login`). Databases are accessed with IAM database authentication."),
                ),
        )
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        if self.tabs.is_empty() {
            return h_flex()
                .size_full()
                .justify_center()
                .child(
                    v_flex()
                        .items_center()
                        .gap_2()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(I::Table2).size(px(36.)))
                        .child(div().text_sm().child("Open a table from the sidebar"))
                        .child(div().text_xs().child("Use the schema's diagram button for an ER diagram, or a database's console button for SQL.")),
                )
                .into_any_element();
        }
        self.active = self.active.min(self.tabs.len() - 1);
        let tab_bar = TabBar::new("main-tabs")
            .small()
            .selected_index(self.active)
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.active = *ix;
                cx.notify();
            }))
            .children(self.tabs.iter().enumerate().map(|(ix, tab)| {
                Tab::new()
                    .label(tab.title(cx))
                    .prefix(div().pl_2().child(Icon::new(tab.icon()).xsmall()))
                    .suffix(
                        Button::new(("close-tab", ix))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(I::X))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, cx);
                            })),
                    )
            }));
        v_flex()
            .size_full()
            .child(tab_bar)
            .child(div().flex_1().min_h_0().child(self.tabs[self.active].view()))
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.auth {
            Auth::SignedIn(_) => h_resizable("workspace-split")
                .child(
                    resizable_panel()
                        .size(px(300.))
                        .size_range(px(200.)..px(600.))
                        .child(self.navigator.clone()),
                )
                .child(resizable_panel().child(self.render_main(cx)))
                .into_any_element(),
            _ => self.render_sign_in(cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                let active = this.active;
                this.close_tab(active, cx);
            }))
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(div().flex_1().min_h_0().child(content))
    }
}
