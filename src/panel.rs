// Hallmark · pre-emit critique: P5 H5 E4 S5 R5 V4

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::base::{
    Align, Disableable as _, ElementExt as _, Placement, Positioner, StyledExt as _,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Root, Sizable as _, Theme};
use gpui_kit::prelude::*;
use gpui_kit::{
    Anchor, AnyElement, App, Bounds, ClickEvent, ClipboardItem, Context, DisplayId, Entity,
    FocusHandle, Hsla, Pixels, Render, Subscription, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind, WindowOptions, deferred, div, px, relative,
};
use uuid::Uuid;

use gitsync::domain::{
    ErrorKind, Freshness, OfflineReason, PullRequestSummary, RepositoryIssue, RepositorySnapshot,
    StaleReason, SynchronizationState, WorkingTreeState,
};
use gitsync::persistence::ThemePreference;

use crate::app::{
    AppModel, NoticeTone, RepositoryCategory, RepositoryGroup, RepositoryRow, issue_is_problem,
    repository_group_key,
};
use crate::macos;
use crate::modal;
use crate::settings::{self, SettingsTab};
use crate::ui::{self, Typography as _};

gpui_kit::actions!(
    gitsync_panel,
    [
        DismissPanel,
        SelectNext,
        SelectPrevious,
        OpenSelected,
        CopySelectedPath
    ]
);

const RAIL_WIDTH: f32 = 56.0;
const PANEL_WIDTH: f32 = 420.0;
const MIN_PANEL_HEIGHT: f32 = 300.0;
const MAX_PANEL_HEIGHT: f32 = 700.0;
const DISPLAY_MARGIN: f32 = 8.0;
const PANEL_CORNER_RADIUS: f64 = 12.0;
/// Search appears once a view lists more repositories than fit at a glance.
const SEARCH_THRESHOLD: usize = 6;

// Heights used to size the panel to its content. They match the paddings and
// text sizes used below closely enough that the content rarely scrolls until
// the panel reaches `MAX_PANEL_HEIGHT`.
const HEADER_HEIGHT: f32 = 70.0;
const SEARCH_HEIGHT: f32 = 44.0;
/// The search field is a little taller than a small control, to match the
/// header tile beside the title.
const SEARCH_FIELD_HEIGHT: f32 = 32.0;
const NOTICE_HEIGHT: f32 = 50.0;
const HEALTH_HEIGHT: f32 = 80.0;
const BANNER_HEIGHT: f32 = 38.0;
const SECTION_HEADER_HEIGHT: f32 = 54.0;
const ROW_HEIGHT: f32 = 54.0;
const DETAIL_ROW_HEIGHT: f32 = 86.0;
const SHOW_MORE_HEIGHT: f32 = 26.0;
/// Rows a section shows before "Show N more".
const SECTION_ROW_LIMIT: usize = 5;
const FOOTER_HEIGHT: f32 = 40.0;
const EMPTY_STATE_HEIGHT: f32 = 262.0;
const RAIL_ITEM_HEIGHT: f32 = 48.0;
/// Widest a chip hint gets before its text wraps, so it stays inside the
/// panel's main column.
const HINT_MAX_WIDTH: f32 = 280.0;
/// Hover time before a chip hint shows, the same as other tooltips.
const HINT_DELAY: Duration = Duration::from_millis(500);
/// Paints chip hints above the rows and the footer.
const HINT_PRIORITY: usize = 200;

// Spacing, in pixels so that it does not follow the rem size.
/// Space between the panel edge and its content.
const CONTENT_PADDING: f32 = 16.0;
/// How far hover backgrounds of rows reach past the text column.
const ROW_INSET: f32 = 8.0;
/// Space above and below each divided block.
const BLOCK_PADDING: f32 = 12.0;
/// Side of the square that holds the mark or computer icon in the header.
const HEADER_TILE: f32 = 36.0;
/// Height of a section's clickable title row.
const SECTION_HEADER_ROW: f32 = 28.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StatusItemAnchor {
    pub(crate) center_x: f64,
    pub(crate) bottom_y: f64,
}

/// What the panel's main column shows: every computer, or one of them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum PanelView {
    #[default]
    Overview,
    Computer(String),
}

#[derive(Default)]
pub(crate) struct PanelState {
    panel: Option<WindowHandle<Root>>,
    focused_repository: Option<Uuid>,
    /// The rail selection, kept while the panel is closed.
    view: PanelView,
    sections: SectionState,
    /// Until when the panel stays open after it opened another GitSync
    /// window, through the focus change that follows.
    keep_open_until: Option<Instant>,
}

impl gpui_kit::Global for PanelState {}

/// Which sections are folded and which list every row. Kept while the panel
/// is closed, so the panel reopens the way the user left it.
#[derive(Clone, Debug)]
pub(crate) struct SectionState {
    collapsed: HashSet<SectionKind>,
    show_all: HashSet<SectionKind>,
}

impl Default for SectionState {
    /// Sections that need the user start open; the rest start folded.
    fn default() -> Self {
        Self {
            collapsed: HashSet::from([
                SectionKind::Unverified,
                SectionKind::UpToDate,
                SectionKind::Paused,
            ]),
            show_all: HashSet::new(),
        }
    }
}

pub(crate) struct RepositoryPanel {
    model: Entity<AppModel>,
    search_input: Entity<InputState>,
    selected: Option<Uuid>,
    /// Row ids in display order, refreshed on every render for keyboard
    /// navigation.
    visible_rows: Vec<Uuid>,
    applied_focus: Option<Uuid>,
    height: f32,
    focus_handle: FocusHandle,
    /// The chip hint that is waiting to show or is visible.
    hint: Option<ChipHint>,
    /// Changes on every hover change, so a late show timer does nothing.
    hint_generation: u64,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone)]
struct ChipHint {
    bounds: Bounds<Pixels>,
    text: String,
    visible: bool,
}

impl RepositoryPanel {
    fn new(
        model: Entity<AppModel>,
        height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search repositories"));
        let subscriptions = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe(&search_input, |_, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.observe_window_activation(window, |_, window, cx| {
                if !window.is_window_active() {
                    close_when_app_inactive(cx);
                }
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.model.read(cx).theme_preference() == ThemePreference::System {
                    crate::app::follow_system_appearance(window.appearance(), window, cx);
                    cx.refresh_windows();
                }
            }),
        ];
        watch_panel(window, cx);
        Self {
            model,
            search_input,
            selected: None,
            visible_rows: Vec::new(),
            applied_focus: None,
            height,
            focus_handle,
            hint: None,
            hint_generation: 0,
            _subscriptions: subscriptions,
        }
    }

    /// Shows a chip hint after the usual tooltip delay, or at once when a
    /// hint is already visible, so moving between chips feels continuous.
    fn show_hint(&mut self, bounds: Bounds<Pixels>, text: String, cx: &mut Context<Self>) {
        self.hint_generation += 1;
        let generation = self.hint_generation;
        let visible = self.hint.as_ref().is_some_and(|hint| hint.visible);
        self.hint = Some(ChipHint {
            bounds,
            text,
            visible,
        });
        if visible {
            cx.notify();
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HINT_DELAY).await;
            this.update(cx, |this, cx| {
                if this.hint_generation == generation
                    && let Some(hint) = &mut this.hint
                {
                    hint.visible = true;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn hide_hint(&mut self, cx: &mut Context<Self>) {
        self.hint_generation += 1;
        if self.hint.take().is_some_and(|hint| hint.visible) {
            cx.notify();
        }
    }

    fn dismiss(&mut self, _: &DismissPanel, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn move_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        if self.visible_rows.is_empty() {
            return;
        }
        let current = self
            .selected
            .and_then(|id| self.visible_rows.iter().position(|row| *row == id));
        let next = match current {
            Some(index) => {
                (index as isize + step).clamp(0, self.visible_rows.len() as isize - 1) as usize
            }
            None if step > 0 => 0,
            None => self.visible_rows.len() - 1,
        };
        self.selected = Some(self.visible_rows[next]);
        cx.notify();
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.selected {
            open_repository(&self.model, id, cx);
        }
    }

    fn copy_selected(&mut self, _: &CopySelectedPath, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            return;
        };
        if let Some(row) = self
            .model
            .read(cx)
            .repositories
            .iter()
            .find(|row| row.record.id == id)
        {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_path_text(
                row.record.ssh_destination.as_deref(),
                &row.record.canonical_path,
            )));
            self.model.update(cx, |model, cx| {
                model.show_message("Path copied.".to_owned(), cx);
            });
        }
    }

    fn select_view(&mut self, view: PanelView, window: &mut Window, cx: &mut Context<Self>) {
        cx.global_mut::<PanelState>().view = view;
        self.selected = None;
        self.search_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    /// Applies a request (from a notification click) to show one repository:
    /// switch to its computer, clear the search, and select it.
    fn apply_focus_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(focus) = cx.global::<PanelState>().focused_repository else {
            self.applied_focus = None;
            return;
        };
        if self.applied_focus == Some(focus) {
            return;
        }
        self.applied_focus = Some(focus);
        let key = self
            .model
            .read(cx)
            .repositories
            .iter()
            .find(|row| row.record.id == focus)
            .map(|row| repository_group_key(row.record.ssh_destination.as_deref()));
        if let Some(key) = key {
            self.select_view(PanelView::Computer(key), window, cx);
        }
        self.selected = Some(focus);
    }
}

impl Render for RepositoryPanel {
    fn render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        self.apply_focus_request(window, cx);
        let search_query = self.search_input.read(cx).value().trim().to_lowercase();
        let view = cx.global::<PanelState>().view.clone();
        let section_state = cx.global::<PanelState>().sections.clone();
        let content = PanelContent::build(
            self.model.read(cx),
            &view,
            &search_query,
            self.applied_focus,
            self.selected,
            &section_state,
        );
        let model_ref = self.model.read(cx);
        let notice = model_ref.message.clone();
        let scanning = model_ref.adding_repository;
        let scan_progress = model_ref.scan_progress.as_ref().map(|progress| {
            (
                progress.found.load(AtomicOrdering::Relaxed),
                progress.checked.load(AtomicOrdering::Relaxed),
            )
        });
        let next_refresh = model_ref.next_refresh_in();
        let refreshes_remaining = model_ref.refreshes_remaining();
        let without_consent: Vec<Uuid> = model_ref
            .repositories_without_fetch_consent()
            .into_iter()
            .filter(|id| content.scope_ids.contains(id))
            .collect();
        let show_search = !search_query.is_empty() || content.stats.total > SEARCH_THRESHOLD;

        self.visible_rows = content
            .sections
            .iter()
            .flat_map(|section| section.rows.iter().map(|row| row.id))
            .collect();

        // Fit the panel to its content, growing downward from the menu bar.
        let height =
            content.estimated_height(notice.is_some(), !without_consent.is_empty(), show_search);
        if (height - self.height).abs() > 1.0 {
            self.height = height;
            // Resize through GPUI so its layout size always matches the
            // window. Resizing the native frame directly left GPUI drawing at
            // the old height: a shrunk panel could not scroll to its last
            // rows, and a grown panel showed an empty band below them. GPUI's
            // resize keeps the bottom edge, so the top is restored after it.
            cx.defer_in(window, move |_, window, _| {
                let top = macos::window_top(window);
                let width = window.bounds().size.width;
                window.resize(gpui_kit::size(width, px(height)));
                if let Some(top) = top {
                    macos::restore_window_top(window, top);
                }
            });
        }

        let model = self.model.clone();
        let panel = cx.entity().clone();
        let selected = self.selected;
        let theme = cx.theme().clone();

        div()
            .track_focus(&self.focus_handle)
            .key_context("GitSyncPanel")
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::copy_selected))
            .h_flex()
            .items_start()
            .size_full()
            .overflow_hidden()
            .rounded(px(PANEL_CORNER_RADIUS as f32))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(render_rail(&content, scanning, &model, &panel, cx))
            .when_some(
                self.hint.clone().filter(|hint| hint.visible),
                |this, hint| this.child(render_hint(hint, &theme)),
            )
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        div()
                            .id("panel-content")
                            .v_flex()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px(px(CONTENT_PADDING))
                            .pt(px(CONTENT_PADDING))
                            .pb_2()
                            .child(
                                div()
                                    .v_flex()
                                    .gap(px(12.0))
                                    .pb(px(14.0))
                                    .child(render_header(&content, &model, cx))
                                    .when(show_search, |this| {
                                        this.child(
                                            Input::new(&self.search_input)
                                                .h(px(SEARCH_FIELD_HEIGHT))
                                                .cleanable(true)
                                                .prefix(
                                                    Icon::new(IconName::Search)
                                                        .small()
                                                        .text_color(theme.muted_foreground),
                                                ),
                                        )
                                    })
                                    .when_some(notice, |this, notice| {
                                        this.child(render_notice(
                                            "panel-notice",
                                            &notice.text,
                                            notice.tone,
                                            &model,
                                            cx,
                                        ))
                                    }),
                            )
                            .when(!content.has_repositories, |this| {
                                this.child(render_empty_state(scanning, &model, cx))
                            })
                            .when(content.has_repositories, |this| {
                                this.child(block(
                                    render_health(&content.stats, &without_consent, &model, cx),
                                    cx,
                                ))
                            })
                            .children(content.sections.iter().map(|section| {
                                block(
                                    render_section(
                                        section,
                                        content.is_overview(),
                                        selected,
                                        &model,
                                        &panel,
                                        cx,
                                    ),
                                    cx,
                                )
                            }))
                            .when(
                                content.has_repositories
                                    && content.sections.is_empty()
                                    && !search_query.is_empty(),
                                |this| {
                                    this.child(
                                        div()
                                            .py(px(BLOCK_PADDING))
                                            .border_t_1()
                                            .border_color(theme.border)
                                            .text_secondary()
                                            .text_color(theme.muted_foreground)
                                            .child("No matching repositories"),
                                    )
                                },
                            ),
                    )
                    .child(render_footer(
                        refreshes_remaining,
                        next_refresh,
                        scanning,
                        scan_progress,
                        &model,
                        cx,
                    )),
            )
    }
}

/// One rail entry per computer that has monitored repositories.
#[derive(Clone)]
struct RailTab {
    key: String,
    label: String,
    ssh_destination: Option<String>,
    /// The most severe state inside, when it needs the user.
    attention: Option<RepositoryCategory>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Stats {
    total: usize,
    current: usize,
    action: usize,
    problems: usize,
    uncommitted: usize,
    unverified: usize,
    paused: usize,
    push: u32,
    pull: u32,
    /// Open pull requests by the user across the repositories.
    pull_requests: u32,
    last_checked: Option<SystemTime>,
}

impl Stats {
    fn add(&mut self, row: &RowView) {
        self.total += 1;
        match row.category {
            RepositoryCategory::Actionable => self.action += 1,
            RepositoryCategory::Problem => self.problems += 1,
            RepositoryCategory::Uncommitted => self.uncommitted += 1,
            RepositoryCategory::Unverified => self.unverified += 1,
            RepositoryCategory::Current => self.current += 1,
            RepositoryCategory::Paused => self.paused += 1,
        }
        if !row.paused {
            self.push = self.push.saturating_add(row.ahead);
            self.pull = self.pull.saturating_add(row.behind);
        }
        if let Some(summary) = row.pull_requests {
            self.pull_requests = self.pull_requests.saturating_add(summary.total());
        }
        self.last_checked = self.last_checked.max(row.last_checked);
    }

    /// Share of the active repositories that are up to date.
    fn health(&self) -> f32 {
        let active = self.total - self.paused;
        if active == 0 {
            0.0
        } else {
            self.current as f32 / active as f32
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum SectionKind {
    Problems,
    Action,
    Uncommitted,
    Unverified,
    UpToDate,
    Paused,
}

impl SectionKind {
    const ORDER: [Self; 6] = [
        Self::Problems,
        Self::Action,
        Self::Uncommitted,
        Self::Unverified,
        Self::UpToDate,
        Self::Paused,
    ];

    fn of(category: RepositoryCategory) -> Self {
        match category {
            RepositoryCategory::Problem => Self::Problems,
            RepositoryCategory::Actionable => Self::Action,
            RepositoryCategory::Uncommitted => Self::Uncommitted,
            RepositoryCategory::Unverified => Self::Unverified,
            RepositoryCategory::Current => Self::UpToDate,
            RepositoryCategory::Paused => Self::Paused,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Problems => "Problems",
            Self::Action => "Needs action",
            Self::Uncommitted => "Uncommitted",
            Self::Unverified => "Unverified",
            Self::UpToDate => "Up to date",
            Self::Paused => "Paused",
        }
    }

    fn color(self, theme: &Theme) -> Hsla {
        match self {
            Self::Problems => theme.danger,
            Self::Action => theme.warning,
            Self::Uncommitted => theme.info,
            Self::Unverified | Self::Paused => theme.muted_foreground,
            Self::UpToDate => theme.success,
        }
    }
}

struct Section {
    kind: SectionKind,
    /// The rows shown: none when folded, at most `SECTION_ROW_LIMIT` until
    /// the user asks for all.
    rows: Vec<RowView>,
    total: usize,
    collapsed: bool,
    /// Whether the section can list more (or fewer) rows than it does now.
    limited: bool,
    show_all: bool,
}

/// Everything the panel shows, computed from the model so the same numbers
/// also size the window before it opens.
struct PanelContent {
    tabs: Vec<RailTab>,
    view: PanelView,
    title: String,
    /// What the title covers, such as `8 repositories · 3 need attention`.
    subtitle: String,
    view_ssh_destination: Option<String>,
    view_paused: bool,
    stats: Stats,
    scope_ids: Vec<Uuid>,
    sections: Vec<Section>,
    selected: Option<Uuid>,
    has_repositories: bool,
    refreshing: bool,
}

impl PanelContent {
    fn build(
        model: &AppModel,
        requested: &PanelView,
        search_query: &str,
        focused: Option<Uuid>,
        selected: Option<Uuid>,
        section_state: &SectionState,
    ) -> Self {
        let groups = model.repository_groups();
        let rows: Vec<RowView> = model
            .repositories
            .iter()
            .map(|row| RowView::from_row(row, model.refresh_queued(row.record.id)))
            .collect();
        let tabs: Vec<RailTab> = groups
            .iter()
            .map(|group| RailTab {
                key: group.key.clone(),
                label: group.label.clone(),
                ssh_destination: group.ssh_destination.clone(),
                attention: rows
                    .iter()
                    .filter(|row| row.group_key == group.key)
                    .map(|row| row.category)
                    .filter(|category| {
                        matches!(
                            category,
                            RepositoryCategory::Problem
                                | RepositoryCategory::Actionable
                                | RepositoryCategory::Uncommitted
                        )
                    })
                    .min_by_key(|category| severity(*category)),
            })
            .collect();
        let selected_group: Option<&RepositoryGroup> = match requested {
            PanelView::Computer(key) => groups.iter().find(|group| &group.key == key),
            PanelView::Overview => None,
        };
        let view = selected_group.map_or(PanelView::Overview, |group| {
            PanelView::Computer(group.key.clone())
        });

        let mut scope: Vec<RowView> = rows
            .into_iter()
            .filter(|row| selected_group.is_none_or(|group| row.group_key == group.key))
            .collect();
        let mut stats = Stats::default();
        for row in &scope {
            stats.add(row);
        }
        let scope_ids = scope.iter().map(|row| row.id).collect();
        let refreshing = scope.iter().any(|row| row.refreshing);
        scope.retain(|row| repository_matches_search(row, search_query));
        scope.sort_by(|left, right| compare_rows(left, right, focused));

        // Every section is listed; folded sections show only their count. A
        // search opens every section and lists every match. A repository
        // opened from a notification is never hidden.
        let searching = !search_query.is_empty();
        let sections: Vec<Section> = SectionKind::ORDER
            .into_iter()
            .filter_map(|kind| {
                let members: Vec<RowView> = scope
                    .iter()
                    .filter(|row| SectionKind::of(row.category) == kind)
                    .cloned()
                    .collect();
                if members.is_empty() {
                    return None;
                }
                let holds_focus = members.iter().any(|row| Some(row.id) == focused);
                let collapsed =
                    !searching && !holds_focus && section_state.collapsed.contains(&kind);
                let show_all = searching || holds_focus || section_state.show_all.contains(&kind);
                let total = members.len();
                let rows = if collapsed {
                    Vec::new()
                } else if show_all {
                    members
                } else {
                    members.into_iter().take(SECTION_ROW_LIMIT).collect()
                };
                Some(Section {
                    kind,
                    rows,
                    total,
                    collapsed,
                    limited: !collapsed && !searching && total > SECTION_ROW_LIMIT,
                    show_all,
                })
            })
            .collect();

        let (title, view_ssh_destination, view_paused) = match selected_group {
            Some(group) => (
                group.label.clone(),
                group.ssh_destination.clone(),
                group.paused,
            ),
            None => ("GitSync".to_owned(), None, false),
        };
        // A renamed SSH computer names its destination instead of `SSH`.
        let scope = selected_group.map(|group| match &group.ssh_destination {
            Some(destination) if group.label != group.default_label => destination.clone(),
            Some(_) => "SSH".to_owned(),
            None => "Local".to_owned(),
        });
        let subtitle = subtitle(&stats, scope.as_deref(), view_paused);

        Self {
            tabs,
            view,
            title,
            subtitle,
            view_ssh_destination,
            view_paused,
            stats,
            scope_ids,
            sections,
            selected,
            has_repositories: !model.repositories.is_empty(),
            refreshing,
        }
    }

    fn is_overview(&self) -> bool {
        self.view == PanelView::Overview
    }

    fn estimated_height(&self, has_notice: bool, has_banner: bool, has_search: bool) -> f32 {
        // A little slack keeps the last row clear of the footer divider.
        let mut height = HEADER_HEIGHT + FOOTER_HEIGHT + 12.0;
        if has_notice {
            height += NOTICE_HEIGHT;
        }
        if has_search {
            height += SEARCH_HEIGHT;
        }
        if !self.has_repositories {
            height += EMPTY_STATE_HEIGHT;
        } else {
            height += HEALTH_HEIGHT;
            if has_banner {
                height += BANNER_HEIGHT;
            }
            for section in &self.sections {
                height += SECTION_HEADER_HEIGHT;
                height += section
                    .rows
                    .iter()
                    .map(|row| {
                        if Some(row.id) == self.selected && row.has_detail() {
                            DETAIL_ROW_HEIGHT
                        } else {
                            ROW_HEIGHT
                        }
                    })
                    .sum::<f32>();
                if section.limited {
                    height += SHOW_MORE_HEIGHT;
                }
            }
        }
        // The rail lists every computer plus its fixed items.
        let rail = (self.tabs.len() + 4) as f32 * RAIL_ITEM_HEIGHT + 24.0;
        height.max(rail).clamp(MIN_PANEL_HEIGHT, MAX_PANEL_HEIGHT)
    }
}

fn repositories_label(count: usize) -> String {
    match count {
        1 => "1 repository".to_owned(),
        count => format!("{count} repositories"),
    }
}

/// The line under the panel title. `scope` says where one computer is,
/// such as `Local` or `SSH`, and is `None` for the overview.
fn subtitle(stats: &Stats, scope: Option<&str>, paused: bool) -> String {
    if stats.total == 0 && scope.is_none() {
        return "Watches your Git repositories".to_owned();
    }
    let mut parts = Vec::new();
    if let Some(scope) = scope {
        parts.push(scope.to_owned());
    }
    parts.push(repositories_label(stats.total));
    let attention = stats.problems + stats.action + stats.uncommitted;
    if paused {
        parts.push("paused".to_owned());
    } else if attention > 0 {
        parts.push(format!("{attention} need attention"));
    } else if stats.total > 0 && stats.current + stats.paused == stats.total {
        parts.push("all up to date".to_owned());
    }
    parts.join(" · ")
}

/// The main action a row offers next to its overflow menu, if any.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrimaryAction {
    AllowFetch,
    Retry,
    Resume,
}

#[derive(Clone)]
struct RowView {
    id: Uuid,
    name: String,
    path: std::path::PathBuf,
    ssh_destination: Option<String>,
    group_key: String,
    branch_line: String,
    status: String,
    /// What the status pill means and what to do next, shown on hover.
    status_hint: String,
    detail: String,
    category: RepositoryCategory,
    pull_requests: Option<PullRequestSummary>,
    ahead: u32,
    behind: u32,
    last_checked: Option<SystemTime>,
    paused: bool,
    network_access_allowed: bool,
    refreshing: bool,
    queued: bool,
}

impl RowView {
    fn from_row(row: &RepositoryRow, queued: bool) -> Self {
        let snapshot = &row.snapshot;
        let category = row.category();
        let (ahead, behind) = match snapshot.synchronization {
            Some(SynchronizationState::Ahead { count }) => (count, 0),
            Some(SynchronizationState::Behind { count }) => (0, count),
            Some(SynchronizationState::Diverged { ahead, behind }) => (ahead, behind),
            Some(SynchronizationState::UpToDate) | None => (0, 0),
        };
        Self {
            id: row.record.id,
            name: row.record.display_name(),
            path: row.record.canonical_path.clone(),
            ssh_destination: row.record.ssh_destination.clone(),
            group_key: repository_group_key(row.record.ssh_destination.as_deref()),
            branch_line: branch_line(snapshot),
            status: pill_label(category, snapshot),
            status_hint: pill_hint(category, snapshot),
            detail: detail(category, snapshot, row.record.ssh_destination.as_deref()),
            category,
            pull_requests: snapshot.pull_requests,
            ahead,
            behind,
            last_checked: snapshot.local_scanned_at,
            paused: row.record.paused,
            network_access_allowed: row.record.network_access_allowed,
            refreshing: row.refreshing,
            queued,
        }
    }

    /// Whether the row has guidance to show when it is selected. Healthy
    /// rows have none.
    fn has_detail(&self) -> bool {
        !self.detail.is_empty()
            && matches!(
                self.category,
                RepositoryCategory::Problem
                    | RepositoryCategory::Uncommitted
                    | RepositoryCategory::Unverified
            )
    }

    fn primary_action(&self) -> Option<PrimaryAction> {
        if self.paused {
            Some(PrimaryAction::Resume)
        } else if !self.network_access_allowed {
            Some(PrimaryAction::AllowFetch)
        } else if self.category == RepositoryCategory::Problem {
            Some(PrimaryAction::Retry)
        } else {
            None
        }
    }
}

fn render_rail(
    content: &PanelContent,
    scanning: bool,
    model: &Entity<AppModel>,
    panel: &Entity<RepositoryPanel>,
    cx: &Context<RepositoryPanel>,
) -> AnyElement {
    let theme = cx.theme();
    let overview = rail_item(
        "rail-overview",
        IconName::LayoutDashboard,
        "Overview".to_owned(),
        content.is_overview(),
        None,
        panel,
        PanelView::Overview,
        cx,
    );
    div()
        .v_flex()
        .flex_none()
        .w(px(RAIL_WIDTH))
        .h_full()
        .items_center()
        .py_3()
        .gap_1()
        .bg(theme.title_bar)
        .border_r_1()
        .border_color(theme.title_bar_border)
        .child(overview)
        .children(content.tabs.iter().map(|tab| {
            let selected = content.view == PanelView::Computer(tab.key.clone());
            let dot = tab.attention.map(|category| status_color(category, theme));
            rail_item(
                format!("rail-{}", tab.key),
                if tab.ssh_destination.is_some() {
                    IconName::Globe
                } else {
                    IconName::HardDrive
                },
                tab.label.clone(),
                selected,
                dot,
                panel,
                PanelView::Computer(tab.key.clone()),
                cx,
            )
        }))
        .child(div().flex_1())
        .child(
            Button::new("rail-add")
                .ghost()
                .icon(IconName::Plus)
                .tooltip("Add repositories")
                .disabled(scanning)
                .dropdown_menu_with_anchor(Anchor::BottomLeft, {
                    let model = model.clone();
                    move |menu, _, _| {
                        menu.item(
                            PopupMenuItem::new("Add Local Folder…")
                                .icon(IconName::FolderOpen)
                                .on_click({
                                    let model = model.clone();
                                    move |_, window, cx| {
                                        choose_and_add_folder(model.clone(), window, cx)
                                    }
                                }),
                        )
                        .item(
                            PopupMenuItem::new("Add SSH Computer…")
                                .icon(IconName::Globe)
                                .on_click({
                                    let model = model.clone();
                                    move |_, _, cx| open_remote_window(model.clone(), cx)
                                }),
                        )
                    }
                }),
        )
        .child(
            Button::new("rail-help")
                .ghost()
                .icon(IconName::Info)
                .tooltip("Help")
                .on_click({
                    let model = model.clone();
                    move |_, _, cx| {
                        if let Err(error) = macos::open_source_repository() {
                            model.update(cx, |model, cx| model.show_error_message(error, cx));
                        }
                    }
                }),
        )
        .child(
            Button::new("rail-settings")
                .ghost()
                .icon(IconName::Settings)
                .tooltip("Settings")
                .dropdown_menu_with_anchor(Anchor::BottomLeft, {
                    let model = model.clone();
                    move |menu, _, _| {
                        menu.item(PopupMenuItem::new("Settings…").on_click({
                            let model = model.clone();
                            move |_, _, cx| {
                                open_settings(model.clone(), SettingsTab::General, None, cx)
                            }
                        }))
                        .item(PopupMenuItem::separator())
                        .item(PopupMenuItem::new("Quit GitSync").on_click(|_, _, cx| cx.quit()))
                    }
                }),
        )
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn rail_item(
    id: impl Into<gpui_kit::SharedString>,
    icon: IconName,
    label: String,
    selected: bool,
    dot: Option<Hsla>,
    panel: &Entity<RepositoryPanel>,
    view: PanelView,
    cx: &Context<RepositoryPanel>,
) -> AnyElement {
    let theme = cx.theme();
    let id: gpui_kit::SharedString = id.into();
    div()
        .id(id)
        .relative()
        .flex_none()
        .size(px(40.0))
        .my_0p5()
        .flex()
        .items_center()
        .justify_center()
        .rounded_lg()
        .cursor_pointer()
        .when(selected, |this| this.bg(theme.muted))
        .when(!selected, |this| this.hover(|style| style.bg(theme.muted)))
        .tooltip({
            let label = label.clone();
            move |window, cx| Tooltip::new(label.clone()).build(window, cx)
        })
        .on_click({
            let panel = panel.clone();
            move |_, window, cx| {
                panel.update(cx, |panel, cx| panel.select_view(view.clone(), window, cx));
            }
        })
        .child(Icon::new(icon).size(px(20.0)).text_color(if selected {
            theme.foreground
        } else {
            theme.muted_foreground
        }))
        .when(selected, |this| {
            this.child(
                div()
                    .absolute()
                    .left(px(-8.0))
                    .top(px(9.0))
                    .w(px(3.0))
                    .h(px(22.0))
                    .rounded_full()
                    .bg(theme.success),
            )
        })
        .when_some(dot, |this, color| {
            this.child(
                div()
                    .absolute()
                    .top(px(6.0))
                    .right(px(6.0))
                    .size(px(8.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.background)
                    .bg(color),
            )
        })
        .into_any_element()
}

fn render_header(
    content: &PanelContent,
    model: &Entity<AppModel>,
    cx: &Context<RepositoryPanel>,
) -> AnyElement {
    let theme = cx.theme();
    let refreshing = content.refreshing;
    let paused = content.view_paused;
    let computer_key = match &content.view {
        PanelView::Computer(key) => Some(key.clone()),
        PanelView::Overview => None,
    };
    let ssh_destination = content.view_ssh_destination.clone();
    // The overview carries the brand mark; a computer shows what it is.
    let tile: AnyElement = match (&computer_key, &ssh_destination) {
        (None, _) => ui::mark(px(20.0), theme.success).into_any_element(),
        (Some(_), destination) => Icon::new(if destination.is_some() {
            IconName::Globe
        } else {
            IconName::HardDrive
        })
        .size(px(18.0))
        .text_color(theme.foreground)
        .into_any_element(),
    };

    div()
        .h_flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .flex_none()
                .size(px(HEADER_TILE))
                .flex()
                .items_center()
                .justify_center()
                .rounded_lg()
                .bg(theme.muted)
                .border_1()
                .border_color(theme.border)
                .child(tile),
        )
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .child(div().text_title().truncate().child(content.title.clone()))
                .child(
                    div()
                        .text_secondary()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(content.subtitle.clone()),
                ),
        )
        .when(content.has_repositories, |this| {
            this.child(
                div()
                    .h_flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .when_some(ssh_destination.clone(), |this, destination| {
                        this.child(
                            Button::new("ssh-view")
                                .small()
                                .ghost()
                                .icon(IconName::SquareTerminal)
                                .tooltip("Open an SSH session in Terminal")
                                .on_click({
                                    let model = model.clone();
                                    move |_, _, cx| {
                                        if let Err(error) = macos::open_ssh_terminal(
                                            &destination,
                                            std::path::Path::new("."),
                                        ) {
                                            model.update(cx, |model, cx| {
                                                model.show_error_message(error, cx);
                                            });
                                        }
                                    }
                                }),
                        )
                    })
                    .when(computer_key.is_some(), |this| {
                        this.child(
                            Button::new("pause-view")
                                .small()
                                .ghost()
                                .icon(if paused {
                                    IconName::Play
                                } else {
                                    IconName::Pause
                                })
                                .tooltip(if paused {
                                    "Resume checking this computer"
                                } else {
                                    "Pause checking this computer"
                                })
                                .on_click({
                                    let model = model.clone();
                                    let ssh_destination = ssh_destination.clone();
                                    move |_, _, cx| {
                                        model.update(cx, |model, cx| {
                                            model.set_destination_paused(
                                                ssh_destination.as_deref(),
                                                !paused,
                                                cx,
                                            );
                                        });
                                    }
                                }),
                        )
                    })
                    .child(
                        Button::new("refresh-view")
                            .small()
                            .ghost()
                            .icon(IconName::RotateCw)
                            .loading(refreshing)
                            .disabled(refreshing || paused)
                            .tooltip("Check now (⌘R)")
                            .on_click({
                                let model = model.clone();
                                let ssh_destination = ssh_destination.clone();
                                let scoped = computer_key.is_some();
                                move |_, _, cx| {
                                    model.update(cx, |model, cx| {
                                        if scoped {
                                            model.refresh_destination(
                                                ssh_destination.as_deref(),
                                                cx,
                                            );
                                        } else {
                                            model.refresh_all(cx);
                                        }
                                    });
                                }
                            }),
                    ),
            )
        })
        .into_any_element()
}

/// The sync-health bar: the share of active repositories that are up to
/// date, with the push and pull totals under it.
fn render_health(
    stats: &Stats,
    without_consent: &[Uuid],
    model: &Entity<AppModel>,
    cx: &Context<RepositoryPanel>,
) -> AnyElement {
    let theme = cx.theme();
    let health = stats.health();
    let all_paused = stats.total > 0 && stats.paused == stats.total;
    let movement = match (stats.push, stats.pull) {
        (0, 0) => "Nothing to push or pull".to_owned(),
        (push, 0) => format!("↑{push} to push"),
        (0, pull) => format!("↓{pull} to pull"),
        (push, pull) => format!("↑{push} to push · ↓{pull} to pull"),
    };
    let movement = match stats.pull_requests {
        0 => movement,
        count => format!("{movement} · {}", pull_requests_label(count)),
    };
    let checked = stats.last_checked.map_or_else(
        || "Not checked yet".to_owned(),
        |time| format!("Checked {}", relative_time(time)),
    );
    let consent_count = without_consent.len();

    div()
        .v_flex()
        .gap_2()
        .child(
            div()
                .h_flex()
                .items_end()
                .justify_between()
                .gap_3()
                .child(eyebrow("Sync health", cx))
                .child(
                    div()
                        .h_flex()
                        .flex_none()
                        .items_baseline()
                        .gap_1()
                        .when(!all_paused, |this| {
                            this.child(
                                div()
                                    .text_heading()
                                    .child(format!("{}%", (health * 100.0).round() as u32)),
                            )
                        })
                        .child(
                            div()
                                .text_caption()
                                .text_color(theme.muted_foreground)
                                .child(if all_paused {
                                    "All paused"
                                } else {
                                    "up to date"
                                }),
                        ),
                ),
        )
        .child(
            div()
                .w_full()
                .h(px(6.0))
                .rounded_full()
                .bg(theme.border)
                .child(
                    div()
                        .h_full()
                        .w(relative(health))
                        .rounded_full()
                        .bg(theme.foreground.opacity(0.9)),
                ),
        )
        .child(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .gap_3()
                .text_caption()
                .text_color(theme.muted_foreground)
                .child(div().min_w_0().truncate().child(movement))
                .child(
                    div()
                        .id("last-checked")
                        .flex_none()
                        .child(checked)
                        .when_some(stats.last_checked, |this, time| {
                            this.tooltip(move |window, cx| {
                                Tooltip::new(absolute_time(time)).build(window, cx)
                            })
                        }),
                ),
        )
        .when(consent_count > 0, |this| {
            let ids = without_consent.to_vec();
            this.child(
                div()
                    .mt_1()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.muted)
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(
                                Icon::new(IconName::ArrowDown)
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(div().text_secondary().truncate().child(
                                if consent_count == 1 {
                                    "1 repository can’t fetch yet".to_owned()
                                } else {
                                    format!("{consent_count} repositories can’t fetch yet")
                                },
                            )),
                    )
                    .child(
                        Button::new("allow-fetch-all")
                            .xsmall()
                            .outline()
                            .label("Allow…")
                            .on_click({
                                let model = model.clone();
                                move |_, window, cx| {
                                    let ids = ids.clone();
                                    let model = model.clone();
                                    // The panel closes when the alert takes
                                    // focus, so it closes first.
                                    window.remove_window();
                                    modal::confirm(
                                        modal::AFTER_PANEL_CLOSES,
                                        {
                                            let count = ids.len();
                                            move || macos::confirm_fetch_for_all(count)
                                        },
                                        move |cx| {
                                            model.update(cx, |model, cx| {
                                                model.allow_network_access_for(&ids, cx);
                                            });
                                        },
                                        cx,
                                    );
                                }
                            }),
                    ),
            )
        })
        .into_any_element()
}

/// A short all-caps label above a block.
fn eyebrow(text: &'static str, cx: &Context<RepositoryPanel>) -> AnyElement {
    div()
        .text_eyebrow()
        .text_color(cx.theme().muted_foreground)
        .child(text.to_uppercase())
        .into_any_element()
}

/// One block of the panel, set off from the block above by a divider.
fn block(content: AnyElement, cx: &Context<RepositoryPanel>) -> AnyElement {
    div()
        .py(px(BLOCK_PADDING))
        .border_t_1()
        .border_color(cx.theme().border)
        .child(content)
        .into_any_element()
}

fn render_section(
    section: &Section,
    overview: bool,
    selected: Option<Uuid>,
    model: &Entity<AppModel>,
    panel: &Entity<RepositoryPanel>,
    cx: &Context<RepositoryPanel>,
) -> AnyElement {
    let theme = cx.theme();
    let kind = section.kind;
    let collapsed = section.collapsed;
    let hidden = section.total - section.rows.len();
    div()
        .v_flex()
        .gap_0p5()
        .child(
            div()
                .id(format!("section-{}", kind.title()))
                .h_flex()
                .items_center()
                .justify_between()
                .mx(px(-ROW_INSET))
                .px(px(ROW_INSET))
                .h(px(SECTION_HEADER_ROW))
                .rounded_md()
                .cursor_pointer()
                .hover(|style| style.bg(theme.muted))
                .on_click({
                    let panel = panel.clone();
                    move |_, _, cx| {
                        let state = &mut cx.global_mut::<PanelState>().sections;
                        if !state.collapsed.remove(&kind) {
                            state.collapsed.insert(kind);
                        }
                        panel.update(cx, |_, cx| cx.notify());
                    }
                })
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .child(div().size(px(7.0)).rounded_full().bg(kind.color(theme)))
                        .child(div().text_heading().child(kind.title())),
                )
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_1p5()
                        .child(
                            div()
                                .text_caption()
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_color(theme.muted_foreground)
                                .child(section.total.to_string()),
                        )
                        .child(
                            Icon::new(if collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall()
                            .text_color(theme.muted_foreground),
                        ),
                ),
        )
        .children(section.rows.iter().map(|row| {
            render_row(
                Some(row.id) == selected,
                overview,
                row.clone(),
                model,
                panel,
                cx,
            )
        }))
        .when(section.limited, |this| {
            this.child(
                div().pt_1().child(
                    Button::new(format!("more-{}", kind.title()))
                        .xsmall()
                        .ghost()
                        .icon(if section.show_all {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        })
                        .label(if section.show_all {
                            "Show less".to_owned()
                        } else {
                            format!("Show {hidden} more")
                        })
                        .on_click({
                            let panel = panel.clone();
                            move |_, _, cx| {
                                let state = &mut cx.global_mut::<PanelState>().sections;
                                if !state.show_all.remove(&kind) {
                                    state.show_all.insert(kind);
                                }
                                panel.update(cx, |_, cx| cx.notify());
                            }
                        }),
                ),
            )
        })
        .into_any_element()
}

pub(crate) fn render_notice<V: 'static>(
    id: &'static str,
    text: &str,
    tone: NoticeTone,
    model: &Entity<AppModel>,
    cx: &Context<V>,
) -> AnyElement {
    let (icon, color) = match tone {
        NoticeTone::Info => (IconName::Info, cx.theme().muted_foreground),
        NoticeTone::Error => (IconName::TriangleAlert, cx.theme().danger),
    };
    div()
        .h_flex()
        .items_start()
        .gap_2()
        .pl_3()
        .pr_1()
        .py_1p5()
        .rounded_lg()
        .border_1()
        .border_color(match tone {
            NoticeTone::Info => cx.theme().border,
            NoticeTone::Error => cx.theme().danger.opacity(0.5),
        })
        .bg(cx.theme().muted)
        .child(
            div()
                .pt(px(2.0))
                .child(Icon::new(icon).small().text_color(color)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .py(px(1.0))
                .whitespace_normal()
                .text_secondary()
                .child(text.to_owned()),
        )
        .child(
            Button::new(id)
                .xsmall()
                .ghost()
                .icon(IconName::Close)
                .tooltip("Close")
                .on_click({
                    let model = model.clone();
                    move |_, _, cx| model.update(cx, |model, cx| model.clear_message(cx))
                }),
        )
        .into_any_element()
}

fn render_empty_state(
    scanning: bool,
    model: &Entity<AppModel>,
    cx: &Context<RepositoryPanel>,
) -> AnyElement {
    let theme = cx.theme();
    div()
        .v_flex()
        .items_center()
        .gap_4()
        .pt_6()
        .pb_2()
        .child(
            div()
                .size(px(52.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_xl()
                .bg(theme.muted)
                .border_1()
                .border_color(theme.border)
                .child(ui::mark(px(30.0), theme.success)),
        )
        .child(
            div()
                .v_flex()
                .items_center()
                .gap_1()
                .child(div().text_heading().child("No repositories yet"))
                .child(
                    div()
                        .max_w(px(260.0))
                        .text_center()
                        .whitespace_normal()
                        .text_secondary()
                        .text_color(theme.muted_foreground)
                        .child("Choose a folder on this Mac or on an SSH computer. GitSync finds the Git working trees in it and shows what needs a push or pull."),
                ),
        )
        .child(
            div()
                .v_flex()
                .w(px(220.0))
                .gap_2()
                .child(
                    Button::new("empty-add-folder")
                        .small()
                        .primary()
                        .w_full()
                        .icon(IconName::FolderOpen)
                        .label("Add Local Folder…")
                        .disabled(scanning)
                        .on_click({
                            let model = model.clone();
                            move |_, window, cx| choose_and_add_folder(model.clone(), window, cx)
                        }),
                )
                .child(
                    Button::new("empty-add-remote")
                        .small()
                        .outline()
                        .w_full()
                        .icon(IconName::Globe)
                        .label("Add SSH Computer…")
                        .disabled(scanning)
                        .on_click({
                            let model = model.clone();
                            move |_, _, cx| open_remote_window(model.clone(), cx)
                        }),
                ),
        )
        .child(
            div()
                .text_caption()
                .text_color(theme.muted_foreground)
                .child("Read-only: GitSync never pushes, pulls, or changes files."),
        )
        .into_any_element()
}

fn render_footer(
    refreshes_remaining: usize,
    next_refresh: Option<Duration>,
    scanning: bool,
    scan_progress: Option<(usize, usize)>,
    model: &Entity<AppModel>,
    cx: &Context<RepositoryPanel>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let left: AnyElement = if scanning {
        let text = match scan_progress {
            Some((found, checked)) if found > 0 => format!("Checking {checked} of {found}…"),
            _ => "Scanning for repositories…".to_owned(),
        };
        div()
            .h_flex()
            .items_center()
            .gap_1p5()
            .min_w_0()
            .child(Icon::new(IconName::LoaderCircle).xsmall().text_color(muted))
            .child(div().truncate().child(text))
            .child(
                Button::new("cancel-scan")
                    .xsmall()
                    .ghost()
                    .label("Cancel")
                    .on_click({
                        let model = model.clone();
                        move |_, _, cx| model.update(cx, |model, cx| model.cancel_scan(cx))
                    }),
            )
            .into_any_element()
    } else {
        div()
            .child(format!("GitSync {}", env!("CARGO_PKG_VERSION")))
            .into_any_element()
    };
    // A pass over many SSH repositories takes a while, so the footer counts
    // down instead of only saying that checks run.
    let right = if refreshes_remaining > 0 {
        format!("Checking · {refreshes_remaining} left")
    } else {
        match next_refresh {
            None => "Manual checks".to_owned(),
            Some(remaining) => format!("Next check {}", time_until(remaining)),
        }
    };

    div()
        .flex_none()
        .mx(px(CONTENT_PADDING))
        .h(px(FOOTER_HEIGHT))
        .h_flex()
        .items_center()
        .justify_between()
        .gap_3()
        .border_t_1()
        .border_color(cx.theme().border)
        .text_caption()
        .text_color(muted)
        .child(div().flex_1().min_w_0().child(left))
        .child(div().flex_none().child(right))
        .into_any_element()
}

/// `in 4m`, `in 1h 5m`, or `in <1m`.
fn time_until(remaining: Duration) -> String {
    let minutes = remaining.as_secs().div_ceil(60);
    match minutes {
        0 => "now".to_owned(),
        1..=59 => format!("in {minutes}m"),
        _ => format!("in {}h {}m", minutes / 60, minutes % 60),
    }
}

fn render_row(
    selected: bool,
    overview: bool,
    row: RowView,
    model: &Entity<AppModel>,
    panel: &Entity<RepositoryPanel>,
    cx: &Context<RepositoryPanel>,
) -> AnyElement {
    let theme = cx.theme();
    let repository_id = row.id;
    let (status, status_hint, status_color) = if row.refreshing {
        (
            "Checking…".to_owned(),
            "GitSync is checking this repository now".to_owned(),
            theme.muted_foreground,
        )
    } else if row.queued {
        (
            "Queued".to_owned(),
            "This check starts when other checks on this computer finish".to_owned(),
            theme.muted_foreground,
        )
    } else {
        (
            row.status.clone(),
            row.status_hint.clone(),
            status_color(row.category, theme),
        )
    };
    let pull_requests = row.pull_requests.filter(|summary| summary.total() > 0);
    let secondary = match (&row.ssh_destination, overview) {
        (Some(destination), true) => format!("{destination} · {}", row.branch_line),
        _ => row.branch_line.clone(),
    };
    let primary = row.primary_action();

    div()
        .id(format!("row-{repository_id}"))
        .v_flex()
        .gap(px(2.0))
        .mx(px(-ROW_INSET))
        .px(px(ROW_INSET))
        .py(px(6.0))
        .rounded_md()
        .when(selected, |this| this.bg(theme.muted))
        .when(!selected, |this| {
            this.hover(|style| style.bg(theme.muted.opacity(0.6)))
        })
        .on_click({
            let panel = panel.clone();
            let model = model.clone();
            move |event: &ClickEvent, _, cx| {
                panel.update(cx, |panel, cx| {
                    panel.selected = Some(repository_id);
                    cx.notify();
                });
                if event.click_count() >= 2 {
                    open_repository(&model, repository_id, cx);
                }
            }
        })
        .child(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .min_w_0()
                        .text_row_title()
                        .truncate()
                        .child(row.name.clone()),
                )
                .child(
                    div()
                        .h_flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .when_some(pull_requests, |this, summary| {
                            this.child(pull_request_chip(
                                repository_id,
                                summary,
                                theme.muted_foreground,
                                panel,
                            ))
                        })
                        .child(status_pill(
                            format!("status-{repository_id}"),
                            status,
                            status_hint,
                            status_color,
                            panel,
                        )),
                ),
        )
        .child(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .gap_3()
                .min_h(px(20.0))
                .child(
                    div()
                        .min_w_0()
                        .text_secondary()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(secondary),
                )
                .child(
                    div()
                        .h_flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .when_some(primary, |this, action| {
                            this.child(primary_button(action, repository_id, model))
                        })
                        .child(row_menu(&row, model)),
                ),
        )
        // Guidance appears for the selected row only, so the list stays two
        // lines per repository.
        .when(selected && row.has_detail(), |this| {
            this.child(
                div()
                    .pt_0p5()
                    .text_secondary()
                    .whitespace_normal()
                    .text_color(detail_color(row.category, theme))
                    .child(row.detail.clone()),
            )
        })
        .into_any_element()
}

/// A repository's state as a small tinted capsule. Hover shows `hint`.
fn status_pill(
    id: String,
    text: String,
    hint: String,
    color: Hsla,
    panel: &Entity<RepositoryPanel>,
) -> impl IntoElement {
    with_hint(capsule(id, text, color), hint, panel)
}

/// The user's open pull requests in a repository, such as `2 PRs`. Hover
/// splits the count by review state.
fn pull_request_chip(
    repository_id: Uuid,
    summary: PullRequestSummary,
    color: Hsla,
    panel: &Entity<RepositoryPanel>,
) -> impl IntoElement {
    let label = match summary.total() {
        1 => "1 PR".to_owned(),
        count => format!("{count} PRs"),
    };
    with_hint(
        capsule(format!("pull-requests-{repository_id}"), label, color),
        pull_request_hint(summary),
        panel,
    )
}

/// Shows `text` in a hint under `chip` while the pointer is over it.
///
/// GPUI's own tooltip opens where the pointer is, so hints of different
/// lengths started at different places. This hint is anchored to the chip.
fn with_hint(
    chip: gpui_kit::Stateful<gpui_kit::Div>,
    text: String,
    panel: &Entity<RepositoryPanel>,
) -> impl IntoElement {
    let bounds = Rc::new(Cell::new(Bounds::default()));
    let writer = bounds.clone();
    let panel = panel.clone();
    chip.on_prepaint(move |chip_bounds, _, _| writer.set(chip_bounds))
        .on_hover(move |hovered, _, cx| {
            panel.update(cx, |panel, cx| {
                if *hovered {
                    panel.show_hint(bounds.get(), text.clone(), cx);
                } else {
                    panel.hide_hint(cx);
                }
            });
        })
}

/// The hint of the chip under the pointer: right-aligned with the chip and
/// below it, or above it when there is no room below.
fn render_hint(hint: ChipHint, theme: &Theme) -> impl IntoElement {
    deferred(
        Positioner::side(hint.bounds)
            .placement(Placement::Bottom)
            .align(Align::End)
            .offset(px(4.0))
            .margin(px(8.0))
            .child(
                div()
                    .max_w(px(HINT_MAX_WIDTH))
                    .whitespace_normal()
                    .px_2()
                    .py_1()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.tokens.popover)
                    .text_color(theme.popover_foreground)
                    .shadow_md()
                    .text_sm()
                    .child(hint.text),
            ),
    )
    .with_priority(HINT_PRIORITY)
}

fn capsule(id: String, text: String, color: Hsla) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(18.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .rounded_full()
        .bg(color.opacity(0.14))
        .text_caption()
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(color)
        .child(text)
}

fn primary_button(action: PrimaryAction, repository_id: Uuid, model: &Entity<AppModel>) -> Button {
    let (label, tooltip) = match action {
        PrimaryAction::AllowFetch => ("Allow", "Let GitSync fetch this repository’s upstream"),
        PrimaryAction::Retry => ("Retry", "Check this repository again"),
        PrimaryAction::Resume => ("Resume", "Start monitoring this repository again"),
    };
    let model = model.clone();
    Button::new(format!("primary-{repository_id}"))
        .xsmall()
        .outline()
        .label(label)
        .tooltip(tooltip)
        .on_click(move |_, _, cx| {
            model.update(cx, |model, cx| match action {
                PrimaryAction::AllowFetch => model.allow_network_access(repository_id, cx),
                PrimaryAction::Retry => model.refresh(repository_id, cx),
                PrimaryAction::Resume => model.toggle_paused(repository_id, cx),
            });
        })
}

fn row_menu(row: &RowView, model: &Entity<AppModel>) -> impl IntoElement {
    let row = row.clone();
    let model = model.clone();
    Button::new(format!("menu-{}", row.id))
        .xsmall()
        .ghost()
        .icon(IconName::Ellipsis)
        .tooltip("More actions")
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            let id = row.id;
            let busy = row.refreshing || row.queued || row.paused;
            let mut menu = menu.item(
                PopupMenuItem::new("Refresh")
                    .icon(IconName::RotateCw)
                    .disabled(busy)
                    .on_click({
                        let model = model.clone();
                        move |_, _, cx| model.update(cx, |model, cx| model.refresh(id, cx))
                    }),
            );
            menu = menu.item(
                PopupMenuItem::new(if row.ssh_destination.is_some() {
                    "Open SSH Session"
                } else {
                    "Open in Terminal"
                })
                .icon(IconName::SquareTerminal)
                .on_click({
                    let model = model.clone();
                    move |_, _, cx| open_repository(&model, id, cx)
                }),
            );
            if row.ssh_destination.is_none() {
                menu = menu.item(
                    PopupMenuItem::new("Reveal in Finder")
                        .icon(IconName::FolderOpen)
                        .on_click({
                            let model = model.clone();
                            let path = row.path.clone();
                            move |_, _, cx| {
                                if let Err(error) = macos::reveal_in_finder(&path) {
                                    model.update(cx, |model, cx| {
                                        model.show_error_message(error, cx)
                                    });
                                }
                            }
                        }),
                );
            }
            menu = menu.item(
                PopupMenuItem::new("Copy Path")
                    .icon(IconName::Copy)
                    .on_click({
                        let text = copy_path_text(row.ssh_destination.as_deref(), &row.path);
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                        }
                    }),
            );
            menu = menu.item(PopupMenuItem::separator());
            if !row.network_access_allowed {
                menu = menu.item(PopupMenuItem::new("Allow Fetch").on_click({
                    let model = model.clone();
                    move |_, _, cx| model.update(cx, |model, cx| model.allow_network_access(id, cx))
                }));
            }
            menu = menu
                .item(
                    PopupMenuItem::new(if row.paused { "Resume" } else { "Pause" })
                        .icon(if row.paused {
                            IconName::Play
                        } else {
                            IconName::Pause
                        })
                        .on_click({
                            let model = model.clone();
                            move |_, _, cx| {
                                model.update(cx, |model, cx| model.toggle_paused(id, cx))
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new(if row.ssh_destination.is_some() {
                        "Change Folder in Settings…"
                    } else {
                        "Change Folder…"
                    })
                    .on_click({
                        let model = model.clone();
                        let remote = row.ssh_destination.is_some();
                        move |_, window, cx| {
                            if remote {
                                open_settings(
                                    model.clone(),
                                    SettingsTab::Repositories,
                                    Some(id),
                                    cx,
                                );
                            } else {
                                // The panel closes when the picker takes focus.
                                window.remove_window();
                                modal::choose_folder_and_relocate(model.clone(), id, cx);
                            }
                        }
                    }),
                )
                .item(PopupMenuItem::new("Rename in Settings…").on_click({
                    let model = model.clone();
                    move |_, _, cx| {
                        open_settings(model.clone(), SettingsTab::Repositories, Some(id), cx)
                    }
                }))
                .item(
                    PopupMenuItem::new("Remove…")
                        .icon(IconName::Delete)
                        .on_click({
                            let model = model.clone();
                            let name = row.name.clone();
                            move |_, window, cx| {
                                // The panel closes when the alert takes
                                // focus, so it closes first.
                                window.remove_window();
                                let name = name.clone();
                                let model = model.clone();
                                modal::confirm(
                                    modal::AFTER_PANEL_CLOSES,
                                    move || macos::confirm_remove(&name),
                                    move |cx| model.update(cx, |model, cx| model.remove(id, cx)),
                                    cx,
                                );
                            }
                        }),
                );
            menu
        })
}

fn status_color(category: RepositoryCategory, theme: &Theme) -> Hsla {
    match category {
        RepositoryCategory::Actionable => theme.warning,
        RepositoryCategory::Problem => theme.danger,
        RepositoryCategory::Uncommitted => theme.info,
        RepositoryCategory::Current => theme.success,
        RepositoryCategory::Unverified | RepositoryCategory::Paused => theme.muted_foreground,
    }
}

fn detail_color(category: RepositoryCategory, theme: &Theme) -> Hsla {
    match category {
        RepositoryCategory::Problem => theme.danger,
        _ => theme.muted_foreground,
    }
}

/// Opens the repository where the user can act on it: Terminal for a local
/// repository, or an SSH session in that folder for a remote one.
fn open_repository(model: &Entity<AppModel>, repository_id: Uuid, cx: &mut App) {
    let Some((destination, path)) = model
        .read(cx)
        .repositories
        .iter()
        .find(|row| row.record.id == repository_id)
        .map(|row| {
            (
                row.record.ssh_destination.clone(),
                row.record.canonical_path.clone(),
            )
        })
    else {
        return;
    };
    let result = match destination {
        Some(destination) => macos::open_ssh_terminal(&destination, &path),
        None => macos::open_in_terminal(&path),
    };
    if let Err(error) = result {
        model.update(cx, |model, cx| model.show_error_message(error, cx));
    }
}

fn choose_and_add_folder(model: Entity<AppModel>, window: &mut Window, cx: &mut App) {
    window.remove_window();
    modal::choose_folder_and_scan(model, cx);
}

/// Opens the SSH window. The panel stays open beside it.
fn open_remote_window(model: Entity<AppModel>, cx: &mut App) {
    keep_open_briefly(cx);
    cx.defer(move |cx| settings::open_remote(model, cx));
}

/// Opens Settings. The panel stays open beside it, so the user can see a
/// change take effect.
fn open_settings(
    model: Entity<AppModel>,
    tab: SettingsTab,
    repository: Option<Uuid>,
    cx: &mut App,
) {
    keep_open_briefly(cx);
    cx.defer(move |cx| settings::open_at(model, tab, repository, cx));
}

/// Keeps the panel through the focus change when it opens another window.
/// macOS may report GitSync inactive for a moment while the new window
/// takes focus.
pub(crate) fn keep_open_briefly(cx: &mut App) {
    cx.global_mut::<PanelState>().keep_open_until = Some(Instant::now() + KEEP_OPEN_GRACE);
}

/// How long focus takes to settle on its next window after a GitSync window
/// loses it.
const FOCUS_SETTLE: Duration = Duration::from_millis(150);
/// How long the panel ignores losing focus after it opened another window.
const KEEP_OPEN_GRACE: Duration = Duration::from_secs(1);

/// How often an open panel checks that it still has a reason to stay.
const PANEL_WATCH_INTERVAL: Duration = Duration::from_millis(400);

/// Whether the panel stays open: it has focus, GitSync is the active app
/// (Settings or the SSH window has focus), or it just opened one of them.
/// The panel is a non-activating panel, so it can have focus while another
/// app stays active.
fn panel_should_stay(window: &Window, cx: &App) -> bool {
    debug_pinned()
        || window.is_window_active()
        || macos::application_is_active()
        || cx
            .global::<PanelState>()
            .keep_open_until
            .is_some_and(|until| Instant::now() < until)
}

/// Closes the panel soon after GitSync windows lose focus to another app.
/// Focus can move from the panel to Settings or the SSH window without
/// closing it. The panel's watchdog covers the cases where no GitSync window
/// had focus to lose.
pub(crate) fn close_when_app_inactive(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FOCUS_SETTLE).await;
        cx.update(|cx| {
            let Some(panel) = cx.global::<PanelState>().panel else {
                return;
            };
            let _ = panel.update(cx, |_, window, cx| {
                if !panel_should_stay(window, cx) {
                    window.remove_window();
                }
            });
        });
    })
    .detach();
}

/// Closes the panel when it no longer has a reason to stay, for the cases no
/// focus event reports: Settings was closed and then another app took over,
/// or a grace period ended while another app was active.
fn watch_panel(window: &Window, cx: &mut Context<RepositoryPanel>) {
    cx.spawn_in(window, async move |this, cx| {
        loop {
            cx.background_executor().timer(PANEL_WATCH_INTERVAL).await;
            let stay = this.update_in(cx, |_, window, cx| {
                let stay = panel_should_stay(window, cx);
                if !stay {
                    window.remove_window();
                }
                stay
            });
            if !matches!(stay, Ok(true)) {
                return;
            }
        }
    })
    .detach();
}

pub(crate) fn copy_path_text(ssh_destination: Option<&str>, path: &std::path::Path) -> String {
    match ssh_destination {
        Some(destination) => format!("{destination}:{}", path.display()),
        None => path.to_string_lossy().into_owned(),
    }
}

/// `main → origin/main · 3 changed, 1 untracked`
fn branch_line(snapshot: &RepositorySnapshot) -> String {
    let mut line = match (&snapshot.branch, &snapshot.upstream) {
        (Some(branch), Some(upstream)) => {
            let upstream = upstream
                .reference
                .strip_prefix("refs/remotes/")
                .or_else(|| upstream.reference.strip_prefix("refs/heads/"))
                .unwrap_or(&upstream.reference);
            format!("{branch} → {upstream}")
        }
        (Some(branch), None) => branch.clone(),
        (None, _) if snapshot.root.is_some() => "Detached HEAD".to_owned(),
        (None, _) => "Not checked yet".to_owned(),
    };
    if snapshot.working_tree.is_dirty() {
        line.push_str(" · ");
        line.push_str(&working_tree_detail(snapshot.working_tree));
    }
    line
}

fn pill_label(category: RepositoryCategory, snapshot: &RepositorySnapshot) -> String {
    match category {
        RepositoryCategory::Paused => return "Paused".to_owned(),
        RepositoryCategory::Problem => {
            return match &snapshot.issue {
                Some(issue) if authentication_required(issue) => "Auth needed",
                Some(RepositoryIssue::NoUpstream) => "No upstream",
                Some(RepositoryIssue::DetachedHead) => "Detached",
                Some(
                    RepositoryIssue::FetchFailed {
                        kind: ErrorKind::UpstreamGone,
                        ..
                    }
                    | RepositoryIssue::Unavailable {
                        kind: ErrorKind::UpstreamGone,
                        ..
                    },
                ) => "Upstream deleted",
                Some(RepositoryIssue::FetchFailed { .. }) => "Fetch failed",
                _ => "Check failed",
            }
            .to_owned();
        }
        RepositoryCategory::Uncommitted => {
            let tree = snapshot.working_tree;
            return if tree.conflicted > 0 {
                "Conflicts".to_owned()
            } else {
                let count = tree.staged + tree.modified + tree.untracked;
                format!("{count} uncommitted")
            };
        }
        _ => {}
    }
    match snapshot.synchronization {
        Some(SynchronizationState::Ahead { count }) => format!("Push {count}"),
        Some(SynchronizationState::Behind { count }) => format!("Pull {count}"),
        Some(SynchronizationState::Diverged { ahead, behind }) => {
            format!("↑{ahead} ↓{behind}")
        }
        Some(SynchronizationState::UpToDate) if category == RepositoryCategory::Current => {
            "Up to date".to_owned()
        }
        _ => "Unverified".to_owned(),
    }
}

/// The hover text of a status pill: what the state means and what to do.
fn pill_hint(category: RepositoryCategory, snapshot: &RepositorySnapshot) -> String {
    match category {
        RepositoryCategory::Paused => {
            return "GitSync does not check this repository. Resume it to monitor it again"
                .to_owned();
        }
        RepositoryCategory::Problem => {
            return match &snapshot.issue {
                Some(issue) if authentication_required(issue) => {
                    "The remote did not accept your credentials. Sign in to the Git host, then retry"
                        .to_owned()
                }
                Some(RepositoryIssue::NoUpstream) => {
                    let branch = snapshot.branch.as_deref().unwrap_or("<branch>");
                    format!(
                        "This branch does not track a remote branch, so GitSync cannot compare it. \
                         Run `git push -u origin {branch}` to set one"
                    )
                }
                Some(RepositoryIssue::DetachedHead) => {
                    "HEAD points to a commit, not to a branch. Check out a branch to track it"
                        .to_owned()
                }
                Some(
                    RepositoryIssue::FetchFailed {
                        kind: ErrorKind::UpstreamGone,
                        ..
                    }
                    | RepositoryIssue::Unavailable {
                        kind: ErrorKind::UpstreamGone,
                        ..
                    },
                ) => "The remote branch was deleted, usually after its pull request merged. \
                      Switch to another branch, or set a new upstream"
                    .to_owned(),
                Some(RepositoryIssue::FetchFailed { .. }) => {
                    "GitSync could not fetch from the remote. Make sure the remote is reachable, then retry"
                        .to_owned()
                }
                _ => "GitSync could not read this repository. Select the row to see the error"
                    .to_owned(),
            };
        }
        RepositoryCategory::Uncommitted => {
            return if snapshot.working_tree.conflicted > 0 {
                "Some files have merge conflicts. Resolve them, then commit".to_owned()
            } else {
                "Some files have changes that are not committed. Commit or stash them".to_owned()
            };
        }
        _ => {}
    }
    match snapshot.synchronization {
        Some(SynchronizationState::Ahead { count }) => format!(
            "{count} local {} not on the remote yet. Push to share your work",
            plural(count, "commit is", "commits are")
        ),
        Some(SynchronizationState::Behind { count }) => format!(
            "The remote has {count} {} that you do not have. Pull to update this branch",
            plural(count, "commit", "commits")
        ),
        Some(SynchronizationState::Diverged { ahead, behind }) => format!(
            "The local branch and the remote both changed. Pull ↓{behind} \
             (merge or rebase), then push ↑{ahead}"
        ),
        Some(SynchronizationState::UpToDate) if category == RepositoryCategory::Current => {
            "This branch is the same as its upstream. Nothing to do".to_owned()
        }
        _ => "GitSync cannot confirm the remote state. Allow fetch or retry the check".to_owned(),
    }
}

fn plural<'a>(count: u32, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

/// `1 open PR` or `3 open PRs`.
fn pull_requests_label(count: u32) -> String {
    match count {
        1 => "1 open PR".to_owned(),
        count => format!("{count} open PRs"),
    }
}

/// `Your open pull requests: 1 awaiting review, 1 approved`.
fn pull_request_hint(summary: PullRequestSummary) -> String {
    let parts: Vec<String> = [
        (summary.awaiting_review, "awaiting review"),
        (summary.changes_requested, "changes requested"),
        (summary.approved, "approved, ready to merge"),
        (summary.draft, "draft"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, state)| format!("{count} {state}"))
    .collect();
    format!("Your open pull requests on GitHub: {}", parts.join(", "))
}

fn detail(
    category: RepositoryCategory,
    snapshot: &RepositorySnapshot,
    ssh_destination: Option<&str>,
) -> String {
    if let Some(issue) = &snapshot.issue
        && issue_is_problem(issue)
    {
        if authentication_required(issue) {
            return authentication_guidance(ssh_destination);
        }
        return match issue {
            RepositoryIssue::NoUpstream => "Set an upstream branch in Git, then retry".to_owned(),
            RepositoryIssue::DetachedHead => "Check out a branch in Git, then retry".to_owned(),
            RepositoryIssue::FetchFailed {
                kind: ErrorKind::UpstreamGone,
                ..
            }
            | RepositoryIssue::Unavailable {
                kind: ErrorKind::UpstreamGone,
                ..
            } => "The upstream branch was deleted on the remote. Set a new upstream or delete the local branch".to_owned(),
            RepositoryIssue::Unavailable { diagnostic, .. }
            | RepositoryIssue::FetchFailed { diagnostic, .. } => concise_diagnostic(diagnostic),
        };
    }
    if category == RepositoryCategory::Paused {
        return String::new();
    }
    let last_fetch = snapshot
        .freshness
        .last_successful_fetch_at()
        .map(|time| format!("last fetched {}", relative_time(time)));
    let unverified_reason = match (&snapshot.freshness, &snapshot.issue) {
        (_, Some(RepositoryIssue::FetchFailed { kind, .. })) => Some(match kind {
            ErrorKind::Timeout => "Remote timed out",
            _ => "Remote unreachable",
        }),
        (
            Freshness::Stale {
                reason: StaleReason::Offline(OfflineReason::ConsentNotGranted),
                ..
            },
            _,
        ) => Some("Fetch not allowed yet"),
        (
            Freshness::Stale {
                reason: StaleReason::Offline(OfflineReason::DisabledByUser),
                ..
            },
            _,
        ) => Some("Fetching is off in Settings"),
        (
            Freshness::Stale {
                reason: StaleReason::CachedAtStartup,
                ..
            },
            _,
        ) => Some("Saved status from last session"),
        (
            Freshness::Stale {
                reason: StaleReason::FetchFailed(_),
                ..
            },
            _,
        ) => Some("Remote unreachable"),
        (Freshness::Unchecked, _) => Some("Not checked yet"),
        (Freshness::Fresh { .. } | Freshness::LocalUpstream { .. }, _) => None,
    };
    match (unverified_reason, &snapshot.freshness) {
        (Some(reason), _) => match last_fetch {
            Some(last_fetch) => format!("{reason} · {last_fetch}"),
            None => reason.to_owned(),
        },
        (None, Freshness::Fresh { fetched_at }) => {
            format!("Fetched {}", relative_time(*fetched_at))
        }
        (None, Freshness::LocalUpstream { scanned_at }) => {
            format!("Local upstream · checked {}", relative_time(*scanned_at))
        }
        (None, _) => String::new(),
    }
}

fn authentication_required(issue: &RepositoryIssue) -> bool {
    matches!(
        issue,
        RepositoryIssue::Unavailable {
            kind: ErrorKind::AuthenticationRequired,
            ..
        } | RepositoryIssue::FetchFailed {
            kind: ErrorKind::AuthenticationRequired,
            ..
        }
    )
}

fn authentication_guidance(ssh_destination: Option<&str>) -> String {
    ssh_destination.map_or_else(
        || "Authenticate with the configured Git remote, then retry".to_owned(),
        |destination| {
            format!(
                "Git credentials are missing on {destination}. Use SSH to authenticate there, then retry"
            )
        },
    )
}

fn concise_diagnostic(diagnostic: &str) -> String {
    let diagnostic = diagnostic.lines().next().unwrap_or_default().trim();
    if diagnostic.chars().count() <= 180 {
        return diagnostic.to_owned();
    }
    let mut concise: String = diagnostic.chars().take(179).collect();
    concise.push('…');
    concise
}

fn working_tree_detail(state: WorkingTreeState) -> String {
    let changed = state.staged + state.modified + state.conflicted;
    match (changed, state.untracked) {
        (0, untracked) => format!("{untracked} untracked"),
        (changed, 0) => format!("{changed} changed"),
        (changed, untracked) => format!("{changed} changed, {untracked} untracked"),
    }
}

/// Short relative time for lists: `just now`, `5m ago`, `3h ago`, `2d ago`,
/// then a calendar date such as `Oct 21`.
pub(crate) fn relative_time(time: SystemTime) -> String {
    let seconds = SystemTime::now()
        .duration_since(time)
        .unwrap_or_default()
        .as_secs();
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        86_400..=604_799 => format!("{}d ago", seconds / 86_400),
        _ => local_time(time).map_or_else(
            || "over a week ago".to_owned(),
            |local| format!("{} {}", MONTHS[local.tm_mon as usize % 12], local.tm_mday),
        ),
    }
}

/// Full local date and time for tooltips, such as `Oct 21, 2026 at 14:03`.
pub(crate) fn absolute_time(time: SystemTime) -> String {
    local_time(time).map_or_else(String::new, |local| {
        format!(
            "{} {}, {} at {:02}:{:02}",
            MONTHS[local.tm_mon as usize % 12],
            local.tm_mday,
            local.tm_year + 1900,
            local.tm_hour,
            local.tm_min
        )
    })
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn local_time(time: SystemTime) -> Option<libc::tm> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs() as libc::time_t;
    // SAFETY: `localtime_r` writes into the zeroed `tm` we own and reads only
    // the `time_t` we pass; it is thread-safe.
    unsafe {
        let mut local: libc::tm = std::mem::zeroed();
        (!libc::localtime_r(&seconds, &mut local).is_null()).then_some(local)
    }
}

fn severity(category: RepositoryCategory) -> u8 {
    match category {
        RepositoryCategory::Problem => 0,
        RepositoryCategory::Actionable => 1,
        RepositoryCategory::Uncommitted => 2,
        RepositoryCategory::Unverified => 3,
        RepositoryCategory::Current => 4,
        RepositoryCategory::Paused => 5,
    }
}

/// Sorts what needs the user first, then by name. A repository opened from a
/// notification comes first of all.
fn compare_rows(left: &RowView, right: &RowView, focused: Option<Uuid>) -> Ordering {
    let is_focused = |row: &RowView| Some(row.id) == focused;
    is_focused(right)
        .cmp(&is_focused(left))
        .then_with(|| severity(left.category).cmp(&severity(right.category)))
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.ssh_destination.cmp(&right.ssh_destination))
        .then_with(|| left.path.cmp(&right.path))
}

fn repository_matches_search(row: &RowView, query: &str) -> bool {
    query.is_empty()
        || row.name.to_lowercase().contains(query)
        || row.branch_line.to_lowercase().contains(query)
        || row.path.to_string_lossy().to_lowercase().contains(query)
        || row
            .ssh_destination
            .as_deref()
            .is_some_and(|destination| destination.to_lowercase().contains(query))
}

pub(crate) fn toggle(anchor: StatusItemAnchor, model: Entity<AppModel>, cx: &mut App) {
    let theme = model.read(cx).theme_preference();
    AppModel::apply_theme(theme, cx);
    cx.global_mut::<PanelState>().focused_repository = None;
    if let Some(panel) = cx.global_mut::<PanelState>().panel.take()
        && panel
            .update(cx, |_, window, _| window.remove_window())
            .is_ok()
    {
        return;
    }

    open_panel(anchor, model, cx);
}

/// Opens the panel from the global shortcut, or closes it when it is open.
/// Unlike a status item click, a shortcut arrives while another app is
/// active, so GitSync activates itself to keep the panel open, and hides
/// itself on close so focus returns to that app.
pub(crate) fn toggle_from_shortcut(
    anchor: StatusItemAnchor,
    model: Entity<AppModel>,
    cx: &mut App,
) {
    let open = cx
        .global::<PanelState>()
        .panel
        .is_some_and(|panel| panel.update(cx, |_, _, _| ()).is_ok());
    if open {
        toggle(anchor, model, cx);
        macos::hide_application();
    } else {
        macos::activate_application();
        toggle(anchor, model, cx);
    }
}

pub(crate) fn show(anchor: StatusItemAnchor, model: Entity<AppModel>, cx: &mut App) {
    let theme = model.read(cx).theme_preference();
    AppModel::apply_theme(theme, cx);
    if let Some(panel) = cx.global::<PanelState>().panel
        && panel
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    cx.global_mut::<PanelState>().panel = None;
    open_panel(anchor, model, cx);
}

/// Debug builds keep the panel open while another app is active when
/// `GITSYNC_DEBUG_PIN` is set, so a script can take a screenshot of it.
fn debug_pinned() -> bool {
    cfg!(debug_assertions) && std::env::var_os("GITSYNC_DEBUG_PIN").is_some()
}

/// Chooses what the panel shows the next time it opens.
#[cfg(debug_assertions)]
pub(crate) fn set_view(view: PanelView, cx: &mut App) {
    cx.global_mut::<PanelState>().view = view;
}

/// Switches the open panel to `view`, as a rail click would.
#[cfg(debug_assertions)]
pub(crate) fn switch_view(view: PanelView, cx: &mut App) {
    let Some(panel) = cx.global::<PanelState>().panel else {
        return;
    };
    let _ = panel.update(cx, |root, window, cx| {
        if let Ok(panel) = root.view().clone().downcast::<RepositoryPanel>() {
            panel.update(cx, |panel, cx| panel.select_view(view, window, cx));
        }
    });
}

pub(crate) fn show_repository(
    anchor: StatusItemAnchor,
    model: Entity<AppModel>,
    repository_id: Option<Uuid>,
    cx: &mut App,
) {
    cx.global_mut::<PanelState>().focused_repository = repository_id;
    cx.refresh_windows();
    show(anchor, model, cx);
}

fn open_panel(anchor: StatusItemAnchor, model: Entity<AppModel>, cx: &mut App) {
    let height = {
        let app = model.read(cx);
        let content = PanelContent::build(
            app,
            &cx.global::<PanelState>().view,
            "",
            cx.global::<PanelState>().focused_repository,
            None,
            &cx.global::<PanelState>().sections,
        );
        content.estimated_height(
            app.message.is_some(),
            !app.repositories_without_fetch_consent().is_empty(),
            content.stats.total > SEARCH_THRESHOLD,
        )
    };
    let (bounds, display_id) = panel_bounds(anchor, height, cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        display_id,
        // The panel paints an opaque, rounded background. The window itself
        // is transparent only so the area outside the rounded corners shows
        // the desktop.
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };

    match cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| RepositoryPanel::new(model, height, window, cx));
        // The panel paints its own rounded background, so the root stays
        // transparent and the corners show the desktop.
        cx.new(|cx| Root::new(view, window, cx).bg(gpui_kit::transparent_black()))
    }) {
        Ok(panel) => {
            let _ = panel.update(cx, |_, window, _| {
                macos::round_window_corners(window, PANEL_CORNER_RADIUS);
            });
            cx.global_mut::<PanelState>().panel = Some(panel);
        }
        Err(error) => eprintln!("failed to open GitSync panel: {error:#}"),
    }
}

fn panel_bounds(
    anchor: StatusItemAnchor,
    panel_height: f32,
    cx: &App,
) -> (Bounds<Pixels>, Option<DisplayId>) {
    let width = px(PANEL_WIDTH);
    let height = px(panel_height);
    let desired_x = px(anchor.center_x as f32 - PANEL_WIDTH / 2.0);
    let desired_y = px(anchor.bottom_y as f32);

    let display = cx.displays().into_iter().find(|display| {
        let bounds = display.bounds();
        desired_x >= bounds.left()
            && desired_x <= bounds.right()
            && desired_y >= bounds.top()
            && desired_y <= bounds.bottom()
    });

    let Some(display) = display else {
        return (
            Bounds::new(
                gpui_kit::point(desired_x, desired_y),
                gpui_kit::size(width, height),
            ),
            None,
        );
    };

    let display_bounds = display.bounds();
    let minimum_x = display_bounds.left() + px(DISPLAY_MARGIN);
    let maximum_x = display_bounds.right() - width - px(DISPLAY_MARGIN);
    let x = desired_x.max(minimum_x).min(maximum_x.max(minimum_x));
    let y = desired_y
        .max(display_bounds.top() + px(DISPLAY_MARGIN))
        .min(
            (display_bounds.bottom() - height - px(DISPLAY_MARGIN))
                .max(display_bounds.top() + px(DISPLAY_MARGIN)),
        );

    (
        Bounds::new(gpui_kit::point(x, y), gpui_kit::size(width, height)),
        Some(display.id()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_counts_only_active_repositories() {
        let mut stats = Stats::default();
        for category in [
            RepositoryCategory::Current,
            RepositoryCategory::Current,
            RepositoryCategory::Actionable,
            RepositoryCategory::Paused,
        ] {
            let mut row = test_row("", category);
            row.paused = category == RepositoryCategory::Paused;
            row.ahead = 2;
            stats.add(&row);
        }
        assert_eq!(stats.total, 4);
        assert!((stats.health() - 2.0 / 3.0).abs() < f32::EPSILON);
        // Paused repositories do not add to the push total.
        assert_eq!(stats.push, 6);
        assert_eq!(Stats::default().health(), 0.0);
    }

    #[test]
    fn sections_follow_severity_order() {
        assert_eq!(
            SectionKind::ORDER.map(SectionKind::title),
            [
                "Problems",
                "Needs action",
                "Uncommitted",
                "Unverified",
                "Up to date",
                "Paused"
            ]
        );
        assert_eq!(
            SectionKind::of(RepositoryCategory::Actionable),
            SectionKind::Action
        );
    }

    #[test]
    fn healthy_sections_start_folded() {
        let state = SectionState::default();
        assert!(!state.collapsed.contains(&SectionKind::Problems));
        assert!(!state.collapsed.contains(&SectionKind::Action));
        assert!(state.collapsed.contains(&SectionKind::Unverified));
        assert!(state.collapsed.contains(&SectionKind::UpToDate));
        assert!(state.collapsed.contains(&SectionKind::Paused));
    }

    #[test]
    fn folded_and_limited_sections_take_less_room() {
        let content = |section: Section| PanelContent {
            tabs: Vec::new(),
            view: PanelView::Overview,
            title: String::new(),
            subtitle: String::new(),
            view_ssh_destination: None,
            view_paused: false,
            stats: Stats::default(),
            scope_ids: Vec::new(),
            sections: vec![section],
            selected: None,
            has_repositories: true,
            refreshing: false,
        };
        let rows = |count: usize| {
            (0..count)
                .map(|_| test_row("", RepositoryCategory::Current))
                .collect::<Vec<_>>()
        };
        let folded = content(Section {
            kind: SectionKind::UpToDate,
            rows: Vec::new(),
            total: 3,
            collapsed: true,
            limited: false,
            show_all: false,
        });
        let open = content(Section {
            kind: SectionKind::UpToDate,
            rows: rows(3),
            total: 3,
            collapsed: false,
            limited: false,
            show_all: false,
        });
        assert!(
            folded.estimated_height(false, false, false)
                < open.estimated_height(false, false, false)
        );

        // A long section shows `SECTION_ROW_LIMIT` rows plus "Show N more",
        // so its height does not grow with the number of hidden rows.
        let limited = |total: usize| {
            content(Section {
                kind: SectionKind::UpToDate,
                rows: rows(SECTION_ROW_LIMIT),
                total,
                collapsed: false,
                limited: true,
                show_all: false,
            })
            .estimated_height(false, false, false)
        };
        assert_eq!(limited(6), limited(400));
    }

    #[test]
    fn subtitle_names_the_scope_and_what_needs_attention() {
        let stats = Stats {
            total: 4,
            current: 1,
            action: 2,
            problems: 1,
            ..Stats::default()
        };
        assert_eq!(
            subtitle(&stats, None, false),
            "4 repositories · 3 need attention"
        );
        assert_eq!(
            subtitle(&stats, Some("SSH"), false),
            "SSH · 4 repositories · 3 need attention"
        );
        assert_eq!(
            subtitle(&stats, Some("Local"), true),
            "Local · 4 repositories · paused"
        );
        let healthy = Stats {
            total: 1,
            current: 1,
            ..Stats::default()
        };
        assert_eq!(
            subtitle(&healthy, None, false),
            "1 repository · all up to date"
        );
        assert_eq!(
            subtitle(&Stats::default(), None, false),
            "Watches your Git repositories"
        );
    }

    #[test]
    fn next_check_reads_as_a_short_duration() {
        assert_eq!(time_until(Duration::ZERO), "now");
        assert_eq!(time_until(Duration::from_secs(30)), "in 1m");
        assert_eq!(time_until(Duration::from_secs(4 * 60)), "in 4m");
        assert_eq!(time_until(Duration::from_secs(65 * 60)), "in 1h 5m");
    }

    #[test]
    fn panel_height_follows_visible_content() {
        let content = |sections: Vec<Section>, has_repositories: bool| PanelContent {
            tabs: Vec::new(),
            view: PanelView::Overview,
            title: String::new(),
            subtitle: String::new(),
            view_ssh_destination: None,
            view_paused: false,
            stats: Stats::default(),
            scope_ids: Vec::new(),
            sections,
            selected: None,
            has_repositories,
            refreshing: false,
        };
        assert_eq!(
            content(Vec::new(), false).estimated_height(false, false, false),
            HEADER_HEIGHT + FOOTER_HEIGHT + 12.0 + EMPTY_STATE_HEIGHT
        );
        let section = |rows: usize| Section {
            kind: SectionKind::UpToDate,
            rows: (0..rows)
                .map(|_| test_row("", RepositoryCategory::Current))
                .collect(),
            total: rows,
            collapsed: false,
            limited: false,
            show_all: true,
        };
        let small = content(vec![section(1)], true).estimated_height(false, false, false);
        let larger = content(vec![section(3)], true).estimated_height(false, false, false);
        assert!(larger > small);
        assert_eq!(
            content(vec![section(30)], true).estimated_height(false, false, false),
            MAX_PANEL_HEIGHT
        );
    }

    #[test]
    fn remote_authentication_failure_has_actionable_guidance() {
        let issue = RepositoryIssue::FetchFailed {
            kind: ErrorKind::AuthenticationRequired,
            diagnostic: "fatal: could not read Username".to_owned(),
        };

        assert!(authentication_required(&issue));
        assert_eq!(
            authentication_guidance(Some("developer@workstation")),
            "Git credentials are missing on developer@workstation. Use SSH to authenticate there, then retry"
        );
    }

    #[test]
    fn uncommitted_pill_counts_files_and_flags_conflicts() {
        let mut snapshot = RepositorySnapshot::default();
        snapshot.working_tree.modified = 6;
        snapshot.working_tree.untracked = 6;
        assert_eq!(
            pill_label(RepositoryCategory::Uncommitted, &snapshot),
            "12 uncommitted"
        );
        snapshot.working_tree.conflicted = 1;
        assert_eq!(
            pill_label(RepositoryCategory::Uncommitted, &snapshot),
            "Conflicts"
        );
    }

    #[test]
    fn deleted_upstream_has_its_own_label() {
        let snapshot = RepositorySnapshot {
            issue: Some(RepositoryIssue::FetchFailed {
                kind: ErrorKind::UpstreamGone,
                diagnostic: "fatal: couldn't find remote ref refs/heads/feature".to_owned(),
            }),
            ..RepositorySnapshot::default()
        };
        assert_eq!(
            pill_label(RepositoryCategory::Problem, &snapshot),
            "Upstream deleted"
        );
        assert!(
            detail(RepositoryCategory::Problem, &snapshot, None)
                .starts_with("The upstream branch was deleted")
        );
    }

    #[test]
    fn rows_sort_by_severity_then_name_and_search_across_location() {
        let mut rows = [
            test_row("zeta", RepositoryCategory::Actionable),
            test_row("Alpha", RepositoryCategory::Current),
            test_row("beta", RepositoryCategory::Problem),
        ];
        rows[1].ssh_destination = Some("developer@workstation".to_owned());
        rows.sort_by(|left, right| compare_rows(left, right, None));

        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["beta", "zeta", "Alpha"]
        );
        assert!(repository_matches_search(&rows[2], "workstation"));
        assert!(!repository_matches_search(&rows[2], "missing"));
    }

    #[test]
    fn relative_time_switches_to_days_then_dates() {
        let now = SystemTime::now();
        assert_eq!(relative_time(now), "just now");
        assert_eq!(
            relative_time(now - Duration::from_secs(2 * 86_400)),
            "2d ago"
        );
        let old = relative_time(now - Duration::from_secs(30 * 86_400));
        assert!(MONTHS.iter().any(|month| old.starts_with(month)), "{old}");
        assert!(!absolute_time(now).is_empty());
    }

    #[test]
    fn status_hints_say_what_to_do() {
        let snapshot = RepositorySnapshot {
            branch: Some("feature".to_owned()),
            issue: Some(RepositoryIssue::NoUpstream),
            ..RepositorySnapshot::default()
        };
        assert!(
            pill_hint(RepositoryCategory::Problem, &snapshot)
                .contains("git push -u origin feature")
        );

        let ahead = RepositorySnapshot {
            synchronization: Some(SynchronizationState::Ahead { count: 1 }),
            ..RepositorySnapshot::default()
        };
        assert_eq!(
            pill_hint(RepositoryCategory::Actionable, &ahead),
            "1 local commit is not on the remote yet. Push to share your work"
        );
    }

    #[test]
    fn pull_requests_are_counted_and_split_by_review_state() {
        let summary = PullRequestSummary {
            awaiting_review: 2,
            approved: 1,
            ..PullRequestSummary::default()
        };
        assert_eq!(
            pull_request_hint(summary),
            "Your open pull requests on GitHub: 2 awaiting review, 1 approved, ready to merge"
        );
        let mut stats = Stats::default();
        let mut row = test_row("app", RepositoryCategory::Current);
        row.pull_requests = Some(summary);
        stats.add(&row);
        stats.add(&row);
        assert_eq!(stats.pull_requests, 6);
        assert_eq!(pull_requests_label(stats.pull_requests), "6 open PRs");
    }

    fn test_row(name: &str, category: RepositoryCategory) -> RowView {
        RowView {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            path: std::path::PathBuf::new(),
            ssh_destination: None,
            group_key: "local".to_owned(),
            branch_line: String::new(),
            status: String::new(),
            status_hint: String::new(),
            detail: String::new(),
            category,
            pull_requests: None,
            ahead: 0,
            behind: 0,
            last_checked: None,
            paused: false,
            network_access_allowed: true,
            refreshing: false,
            queued: false,
        }
    }
}
