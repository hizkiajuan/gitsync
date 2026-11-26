// Hallmark · pre-emit critique: P5 H5 E4 S5 R5 V4

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Duration;

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Root, Selectable as _, Sizable as _};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, Entity, Focusable, Render, Subscription,
    TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowOptions,
    div, px, size,
};
use uuid::Uuid;

use gitsync::persistence::ThemePreference;

use crate::app::{
    AppModel, RepositoryGroup, RepositoryGroupState, ScanEvent, repository_group_description,
    repository_group_key,
};
use crate::macos;
use crate::modal;
use crate::panel::{copy_path_text, render_notice};
use crate::ui::{self, Typography as _};

/// The Git executable that ships with macOS.
const DEFAULT_GIT: &str = "/usr/bin/git";

// Spacing, in pixels so that it does not follow the rem size.
/// Space between the window edge and the page.
const PAGE_PADDING: f32 = 20.0;
/// Space between groups of settings.
const GROUP_GAP: f32 = 24.0;
/// Space between a group's heading and its card.
const HEADING_GAP: f32 = 8.0;
/// Inner padding of a card row.
const ROW_PADDING_X: f32 = 14.0;
const ROW_PADDING_Y: f32 = 10.0;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SettingsTab {
    #[default]
    General,
    Repositories,
    Advanced,
}

impl SettingsTab {
    const ALL: [Self; 3] = [Self::General, Self::Repositories, Self::Advanced];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Repositories => "Repositories",
            Self::Advanced => "Advanced",
        }
    }
}

#[derive(Default)]
pub(crate) struct SettingsWindowState {
    window: Option<WindowHandle<Root>>,
    view: Option<Entity<SettingsView>>,
    remote_window: Option<WindowHandle<Root>>,
}

impl gpui_kit::Global for SettingsWindowState {}

/// The name field of one computer group.
struct ComputerInput {
    key: String,
    input: Entity<InputState>,
    _subscription: Subscription,
}

struct RepositoryInput {
    id: Uuid,
    input: Entity<InputState>,
    /// The folder, editable: Enter points the repository at another folder.
    path_input: Entity<InputState>,
    _subscriptions: [Subscription; 2],
}

pub(crate) struct SettingsView {
    model: Entity<AppModel>,
    tab: SettingsTab,
    repository_inputs: Vec<RepositoryInput>,
    computer_inputs: Vec<ComputerInput>,
    /// A repository to highlight and focus, for "Rename in Settings…".
    highlighted_repository: Option<Uuid>,
    focus_pending: bool,
    copied_repository: Option<Uuid>,
    copied_diagnostics: bool,
    launch_status: Result<macos::LoginItemStatus, String>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    fn new(
        model: Entity<AppModel>,
        tab: SettingsTab,
        highlighted_repository: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, {
                let model = model.clone();
                move |_, window, cx| {
                    if model.read(cx).theme_preference() == ThemePreference::System {
                        crate::app::follow_system_appearance(window.appearance(), window, cx);
                        cx.refresh_windows();
                    }
                }
            }),
            // The login item can change in System Settings while this window
            // is in the background, so read it again on activation instead of
            // on every frame.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.launch_status = macos::launch_at_login_status();
                    cx.notify();
                } else {
                    crate::panel::close_when_app_inactive(cx);
                }
            }),
        ];
        Self {
            model,
            tab,
            repository_inputs: Vec::new(),
            computer_inputs: Vec::new(),
            highlighted_repository,
            focus_pending: highlighted_repository.is_some(),
            copied_repository: None,
            copied_diagnostics: false,
            launch_status: macos::launch_at_login_status(),
            _subscriptions: subscriptions,
        }
    }

    fn show(&mut self, tab: SettingsTab, repository: Option<Uuid>, cx: &mut Context<Self>) {
        self.tab = tab;
        if repository.is_some() {
            self.highlighted_repository = repository;
            self.focus_pending = true;
        }
        cx.notify();
    }

    /// Keeps one name field per computer group, like `sync_repository_inputs`.
    fn sync_computer_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let groups = self.model.read(cx).repository_groups();
        self.computer_inputs
            .retain(|entry| groups.iter().any(|group| group.key == entry.key));
        for group in groups {
            if self
                .computer_inputs
                .iter()
                .any(|entry| entry.key == group.key)
            {
                continue;
            }
            let default_label = group.default_label.clone();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(group.label.clone())
                    .placeholder(default_label.clone())
            });
            let subscription = cx.subscribe_in(&input, window, {
                let model = self.model.clone();
                let key = group.key.clone();
                move |_, input, event, window, cx| {
                    if !matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                        return;
                    }
                    let value = input.read(cx).value().to_string();
                    model.update(cx, |model, cx| model.rename_computer(&key, &value, cx));
                    // A blank name restores the default, so show it.
                    if value.trim().is_empty() {
                        input.update(cx, |input, cx| {
                            input.set_value(default_label.clone(), window, cx)
                        });
                    }
                }
            });
            self.computer_inputs.push(ComputerInput {
                key: group.key,
                input,
                _subscription: subscription,
            });
        }
    }

    /// Keeps one rename field per repository. Repositories can be added or
    /// removed while the window is open, so this runs on every render.
    fn sync_repository_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let records: Vec<(Uuid, String, String)> = self
            .model
            .read(cx)
            .repositories
            .iter()
            .map(|row| {
                (
                    row.record.id,
                    row.record.display_name(),
                    editable_path(
                        row.record.ssh_destination.is_some(),
                        &row.record.selected_path,
                        &row.record.canonical_path,
                    ),
                )
            })
            .collect();
        self.repository_inputs
            .retain(|entry| records.iter().any(|(id, _, _)| *id == entry.id));
        for (id, name, path) in records {
            if self.repository_inputs.iter().any(|entry| entry.id == id) {
                continue;
            }
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(name)
                    .placeholder("Name")
            });
            let subscription = cx.subscribe(&input, {
                let model = self.model.clone();
                move |_, input, event, cx| {
                    if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                        let value = input.read(cx).value();
                        model.update(cx, |model, cx| {
                            model.rename_repository(id, value.as_ref(), cx);
                        });
                    }
                }
            });
            let path_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(path)
                    .placeholder("Folder")
            });
            let path_subscription = cx.subscribe(&path_input, {
                let model = self.model.clone();
                move |_, input, event, cx| {
                    // Only Enter moves the repository, so leaving a field
                    // that was touched by accident changes nothing.
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        let value = input.read(cx).value();
                        model.update(cx, |model, cx| {
                            model.relocate_repository(id, value.as_ref(), cx);
                        });
                    }
                }
            });
            self.repository_inputs.push(RepositoryInput {
                id,
                input,
                path_input,
                _subscriptions: [subscription, path_subscription],
            });
        }
        if self.focus_pending
            && self.tab == SettingsTab::Repositories
            && let Some(id) = self.highlighted_repository
            && let Some(entry) = self.repository_inputs.iter().find(|entry| entry.id == id)
        {
            self.focus_pending = false;
            let key = self
                .model
                .read(cx)
                .repositories
                .iter()
                .find(|row| row.record.id == id)
                .map(|row| repository_group_key(row.record.ssh_destination.as_deref()));
            if let Some(key) = key {
                cx.global_mut::<RepositoryGroupState>()
                    .set_collapsed(&key, false);
            }
            entry.input.read(cx).focus_handle(cx).focus(window, cx);
        }
    }
}

impl Render for SettingsView {
    fn render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        self.sync_repository_inputs(window, cx);
        self.sync_computer_inputs(window, cx);
        let notice = self.model.read(cx).message.clone();
        let tab = self.tab;
        let view = cx.entity().clone();
        let body = match tab {
            SettingsTab::General => self.render_general(cx),
            SettingsTab::Repositories => self.render_repositories(cx),
            SettingsTab::Advanced => self.render_advanced(cx),
        };
        let model = self.model.clone();

        div()
            .v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_body()
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .justify_center()
                    .px(px(PAGE_PADDING))
                    .py(px(10.0))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        TabBar::new("settings-tabs")
                            .segmented()
                            .small()
                            .selected_index(
                                SettingsTab::ALL
                                    .iter()
                                    .position(|candidate| *candidate == tab)
                                    .unwrap_or_default(),
                            )
                            .children(SettingsTab::ALL.map(|tab| Tab::new().label(tab.label())))
                            .on_click(move |index, _, cx| {
                                view.update(cx, |view, cx| {
                                    view.tab = SettingsTab::ALL[*index];
                                    cx.notify();
                                });
                            }),
                    ),
            )
            .when_some(notice, |this, notice| {
                this.child(div().flex_none().px(px(PAGE_PADDING)).pt(px(12.0)).child(
                    render_notice("settings-notice", &notice.text, notice.tone, &model, cx),
                ))
            })
            .child(
                div()
                    .id("settings-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .v_flex()
                            .px(px(PAGE_PADDING))
                            .pt(px(18.0))
                            .pb(px(PAGE_PADDING))
                            .child(body),
                    ),
            )
    }
}

impl SettingsView {
    fn render_general(&self, cx: &Context<Self>) -> AnyElement {
        let model_ref = self.model.read(cx);
        let fetch_enabled = model_ref.fetch_enabled();
        let refresh_interval = model_ref.refresh_interval_seconds();
        let refresh_on_wake = model_ref.refresh_on_wake();
        let notifications_enabled = model_ref.notifications_enabled();
        let notify_on_push = model_ref.notify_on_push();
        let theme = model_ref.theme_preference();
        let launch_registered = self
            .launch_status
            .as_ref()
            .is_ok_and(|status| status.is_registered());
        let launch_description = self
            .launch_status
            .as_ref()
            .map(|status| status.description())
            .unwrap_or("Unavailable");
        let launch_needs_approval = self
            .launch_status
            .as_ref()
            .is_ok_and(|status| *status == macos::LoginItemStatus::RequiresApproval);
        let model = self.model.clone();
        let view = cx.entity().clone();

        div()
            .v_flex()
            .gap(px(GROUP_GAP))
            .child(group(
                "Checking",
                "When GitSync looks at your repositories and their remotes.",
                [
                    setting_row(
                        "Fetch from remotes",
                        "Turns every fetch on or off. Uses your existing Git, SSH, and credential helpers.",
                        Switch::new("fetch-enabled")
                            .small()
                            .checked(fetch_enabled)
                            .accessibility_label("Fetch from repository remotes")
                            .on_click({
                                let model = model.clone();
                                move |enabled, _, cx| {
                                    model.update(cx, |model, cx| {
                                        model.set_fetch_enabled(*enabled, cx)
                                    });
                                }
                            })
                            .into_any_element(),
                        cx,
                    ),
                    setting_row(
                        "Check every",
                        "Manual checks only when you click Refresh.",
                        segmented(
                            "interval",
                            &[
                                (0, "Manual"),
                                (60, "1 min"),
                                (300, "5 min"),
                                (900, "15 min"),
                                (1_800, "30 min"),
                            ],
                            refresh_interval,
                            {
                                let model = model.clone();
                                move |seconds, cx| {
                                    model.update(cx, |model, cx| {
                                        model.set_refresh_interval_seconds(seconds, cx);
                                    });
                                }
                            },
                        ),
                        cx,
                    ),
                    setting_row(
                        "Check after wake",
                        "Waits briefly for the network, then checks active repositories.",
                        Switch::new("refresh-on-wake")
                            .small()
                            .checked(refresh_on_wake)
                            .accessibility_label("Refresh repositories after wake")
                            .on_click({
                                let model = model.clone();
                                move |enabled, _, cx| {
                                    model.update(cx, |model, cx| {
                                        model.set_refresh_on_wake(*enabled, cx)
                                    });
                                }
                            })
                            .into_any_element(),
                        cx,
                    ),
                ],
                cx,
            ))
            .child(group(
                "Notifications",
                "Banners for changes you did not make yourself.",
                [
                    setting_row(
                        "Notify about remote changes",
                        "When a remote has new commits to pull, or your branch diverged.",
                        Switch::new("notifications-enabled")
                            .small()
                            .checked(notifications_enabled)
                            .accessibility_label("Notify when repositories need attention")
                            .on_click({
                                let model = model.clone();
                                move |enabled, _, cx| {
                                    model.update(cx, |model, cx| {
                                        model.set_notifications_enabled(*enabled, cx);
                                    });
                                }
                            })
                            .into_any_element(),
                        cx,
                    ),
                    setting_row(
                        "Also notify about commits to push",
                        "Off by default, because those commits are your own.",
                        Switch::new("notify-on-push")
                            .small()
                            .checked(notify_on_push)
                            .disabled(!notifications_enabled)
                            .accessibility_label("Notify when commits are waiting to be pushed")
                            .on_click({
                                let model = model.clone();
                                move |enabled, _, cx| {
                                    model.update(cx, |model, cx| {
                                        model.set_notify_on_push(*enabled, cx)
                                    });
                                }
                            })
                            .into_any_element(),
                        cx,
                    ),
                ],
                cx,
            ))
            .child(group(
                "App",
                "How GitSync starts and looks.",
                [
                    setting_row(
                        "Launch at login",
                        launch_description,
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .when(launch_needs_approval, |this| {
                                this.child(
                                    Button::new("open-login-items")
                                        .small()
                                        .outline()
                                        .label("Open System Settings")
                                        .on_click({
                                            let model = model.clone();
                                            move |_, _, cx| {
                                                if let Err(error) =
                                                    macos::open_login_items_settings()
                                                {
                                                    model.update(cx, |model, cx| {
                                                        model.show_error_message(error, cx);
                                                    });
                                                }
                                            }
                                        }),
                                )
                            })
                            .child(
                                Switch::new("launch-at-login")
                                    .small()
                                    .checked(launch_registered)
                                    .accessibility_label("Launch GitSync at login")
                                    .on_click({
                                        let model = model.clone();
                                        let view = view.clone();
                                        move |enabled, _, cx| {
                                            let result = macos::set_launch_at_login(*enabled);
                                            view.update(cx, |view, cx| {
                                                view.launch_status =
                                                    macos::launch_at_login_status();
                                                cx.notify();
                                            });
                                            model.update(cx, |model, cx| match result {
                                                Ok(status) => model
                                                    .set_launch_at_login_preference(
                                                        status.is_registered(),
                                                        cx,
                                                    ),
                                                Err(error) => model.show_error_message(error, cx),
                                            });
                                        }
                                    }),
                            )
                            .into_any_element(),
                        cx,
                    ),
                    setting_row(
                        "Appearance",
                        "System follows the macOS setting.",
                        segmented(
                            "theme",
                            &[
                                (ThemePreference::System, "System"),
                                (ThemePreference::Light, "Light"),
                                (ThemePreference::Dark, "Dark"),
                            ],
                            theme,
                            {
                                let model = model.clone();
                                move |preference, cx| {
                                    model.update(cx, |model, cx| {
                                        model.set_theme_preference(preference, cx);
                                    });
                                }
                            },
                        ),
                        cx,
                    ),
                ],
                cx,
            ))
            .into_any_element()
    }

    fn render_repositories(&self, cx: &Context<Self>) -> AnyElement {
        let model_ref = self.model.read(cx);
        let scanning = model_ref.adding_repository;
        let mut repositories: Vec<SettingsRepository> = model_ref
            .repositories
            .iter()
            .filter_map(|row| {
                let entry = self
                    .repository_inputs
                    .iter()
                    .find(|entry| entry.id == row.record.id)?;
                Some(SettingsRepository {
                    id: row.record.id,
                    input: entry.input.clone(),
                    path_input: entry.path_input.clone(),
                    name: row.record.display_name(),
                    path: row.record.canonical_path.clone(),
                    ssh_destination: row.record.ssh_destination.clone(),
                    paused: row.record.paused,
                })
            })
            .collect();
        repositories.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.name.cmp(&right.name))
        });
        let groups: Vec<(RepositoryGroup, bool)> = model_ref
            .repository_groups()
            .into_iter()
            .map(|group| {
                let collapsed = cx.global::<RepositoryGroupState>().is_collapsed(&group);
                (group, collapsed)
            })
            .collect();
        let model = self.model.clone();
        let view = cx.entity().clone();
        let copied_repository = self.copied_repository;
        let highlighted = self.highlighted_repository;

        div()
            .v_flex()
            .gap(px(GROUP_GAP))
            .child(
                div()
                    .h_flex()
                    .items_end()
                    .justify_between()
                    .gap_4()
                    .child(div().flex_1().min_w_0().child(group_heading(
                        "Monitored repositories",
                        "Grouped by computer. Click a name to rename it. A switch pauses checking.",
                        cx,
                    )))
                    .child(
                        div()
                            .h_flex()
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("settings-add-remote")
                                    .small()
                                    .outline()
                                    .icon(IconName::Globe)
                                    .label("SSH Computer…")
                                    .disabled(scanning)
                                    .on_click({
                                        let model = model.clone();
                                        move |_, _, cx| open_remote(model.clone(), cx)
                                    }),
                            )
                            .child(
                                Button::new("settings-add-repository")
                                    .small()
                                    .primary()
                                    .icon(IconName::FolderOpen)
                                    .label("Add Folder…")
                                    .disabled(scanning)
                                    .on_click({
                                        let model = model.clone();
                                        move |_, _, cx| {
                                            modal::choose_folder_and_scan(model.clone(), cx)
                                        }
                                    }),
                            ),
                    ),
            )
            .when(repositories.is_empty(), |this| {
                this.child(
                    card(cx).child(
                        div()
                            .v_flex()
                            .items_center()
                            .gap_2()
                            .py(px(28.0))
                            .px(px(ROW_PADDING_X))
                            .child(ui::mark(px(28.0), cx.theme().muted_foreground))
                            .child(div().text_row_title().child("No repositories yet"))
                            .child(
                                div()
                                    .text_secondary()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Add a folder on this Mac or on an SSH computer."),
                            ),
                    ),
                )
            })
            .children(groups.into_iter().map(|(group, collapsed)| {
                let members: Vec<SettingsRepository> = repositories
                    .iter()
                    .filter(|repository| {
                        repository_group_key(repository.ssh_destination.as_deref()) == group.key
                    })
                    .cloned()
                    .collect();
                let name_input = self
                    .computer_inputs
                    .iter()
                    .find(|entry| entry.key == group.key)
                    .map(|entry| entry.input.clone());
                render_repository_group(
                    group,
                    name_input,
                    collapsed,
                    members,
                    &model,
                    &view,
                    copied_repository,
                    highlighted,
                    cx,
                )
            }))
            .into_any_element()
    }

    fn render_advanced(&self, cx: &Context<Self>) -> AnyElement {
        let model_ref = self.model.read(cx);
        let git_executable = model_ref.git_executable().to_string_lossy().into_owned();
        let default_git = git_executable == DEFAULT_GIT;
        let fetch_timeout = model_ref.fetch_timeout_seconds();
        let testing_git = model_ref.testing_git;
        let storage_root = model_ref.storage_root().to_path_buf();
        let diagnostics_path = model_ref.diagnostics_path();
        let model = self.model.clone();
        let view = cx.entity().clone();
        let copied_diagnostics = self.copied_diagnostics;
        let theme = cx.theme();

        div()
            .v_flex()
            .gap(px(GROUP_GAP))
            .child(group(
                "Git",
                "The Git executable and time limits used for every check.",
                [
                    row(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.0))
                            .child(div().text_row_title().child("Git executable"))
                            .child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_secondary()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(git_executable),
                            )
                            .into_any_element(),
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .when(!default_git, |this| {
                                this.child(
                                    Button::new("default-git")
                                        .small()
                                        .ghost()
                                        .label("Use Default")
                                        .tooltip("Use /usr/bin/git")
                                        .on_click({
                                            let model = model.clone();
                                            move |_, _, cx| {
                                                model.update(cx, |model, cx| {
                                                    model.set_git_executable(
                                                        PathBuf::from(DEFAULT_GIT),
                                                        cx,
                                                    );
                                                });
                                            }
                                        }),
                                )
                            })
                            .child(
                                Button::new("choose-git")
                                    .small()
                                    .outline()
                                    .label("Choose…")
                                    .on_click({
                                        let model = model.clone();
                                        move |_, _, cx| {
                                            modal::choose_git_executable(model.clone(), cx)
                                        }
                                    }),
                            )
                            .child(
                                Button::new("test-git")
                                    .small()
                                    .outline()
                                    .loading(testing_git)
                                    .disabled(testing_git)
                                    .label("Test")
                                    .tooltip("Run the Git executable once and show its version")
                                    .on_click({
                                        let model = model.clone();
                                        move |_, _, cx| {
                                            model.update(cx, |model, cx| model.test_git(cx));
                                        }
                                    }),
                            )
                            .into_any_element(),
                        cx,
                    ),
                    setting_row(
                        "Command timeout",
                        "For each Git or SSH command. Folder scans get at least 2 minutes.",
                        segmented(
                            "timeout",
                            &[(10, "10 sec"), (30, "30 sec"), (60, "60 sec")],
                            fetch_timeout,
                            {
                                let model = model.clone();
                                move |seconds, cx| {
                                    model.update(cx, |model, cx| {
                                        model.set_fetch_timeout_seconds(seconds, cx);
                                    });
                                }
                            },
                        ),
                        cx,
                    ),
                ],
                cx,
            ))
            .child(group(
                "Diagnostics",
                "Local information for troubleshooting. Repository paths are left out. No telemetry.",
                [
                    setting_row(
                        "Diagnostic summary",
                        "Versions, settings, and recent errors, ready to paste into a report.",
                        Button::new("copy-diagnostics")
                            .small()
                            .outline()
                            .icon(if copied_diagnostics {
                                IconName::Check
                            } else {
                                IconName::Copy
                            })
                            .label(if copied_diagnostics {
                                "Copied"
                            } else {
                                "Copy Summary"
                            })
                            .on_click({
                                let model = model.clone();
                                let view = view.clone();
                                move |_, _, cx| {
                                    let summary = model.read(cx).diagnostic_summary();
                                    cx.write_to_clipboard(ClipboardItem::new_string(summary));
                                    flash_copied(&view, None, cx);
                                }
                            })
                            .into_any_element(),
                        cx,
                    ),
                    setting_row(
                        "Data and logs",
                        "GitSync keeps its settings, cache, and logs on this Mac only.",
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("open-data-folder")
                                    .small()
                                    .ghost()
                                    .icon(IconName::Folder)
                                    .label("Data Folder")
                                    .on_click({
                                        let model = model.clone();
                                        let storage_root = storage_root.clone();
                                        move |_, _, cx| {
                                            if let Err(error) =
                                                macos::open_data_folder(&storage_root)
                                            {
                                                model.update(cx, |model, cx| {
                                                    model.show_error_message(error, cx);
                                                });
                                            }
                                        }
                                    }),
                            )
                            .child(
                                Button::new("reveal-diagnostics")
                                    .small()
                                    .ghost()
                                    .icon(IconName::FileText)
                                    .label("Logs")
                                    .on_click({
                                        let model = model.clone();
                                        let storage_root = storage_root.clone();
                                        move |_, _, cx| {
                                            if let Err(error) = macos::reveal_diagnostics(
                                                &diagnostics_path,
                                                &storage_root,
                                            ) {
                                                model.update(cx, |model, cx| {
                                                    model.show_error_message(error, cx);
                                                });
                                            }
                                        }
                                    }),
                            )
                            .child(
                                Button::new("delete-diagnostics")
                                    .small()
                                    .ghost()
                                    .icon(IconName::Delete)
                                    .tooltip("Delete Logs")
                                    .on_click({
                                        let model = model.clone();
                                        move |_, _, cx| {
                                            model.update(cx, |model, cx| {
                                                model.delete_diagnostics(cx)
                                            });
                                        }
                                    }),
                            )
                            .into_any_element(),
                        cx,
                    ),
                ],
                cx,
            ))
            .child(
                div()
                    .v_flex()
                    .gap(px(HEADING_GAP))
                    .child(group_heading("About", "", cx))
                    .child(
                        card(cx).child(row(
                            div()
                                .h_flex()
                                .flex_1()
                                .min_w_0()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .flex_none()
                                        .size(px(36.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_lg()
                                        .bg(theme.background)
                                        .border_1()
                                        .border_color(theme.border)
                                        .child(ui::mark(px(20.0), theme.success)),
                                )
                                .child(
                                    div()
                                        .v_flex()
                                        .min_w_0()
                                        .gap(px(2.0))
                                        .child(div().text_row_title().child(format!(
                                            "GitSync {}",
                                            env!("CARGO_PKG_VERSION")
                                        )))
                                        .child(
                                            div()
                                                .text_secondary()
                                                .text_color(theme.muted_foreground)
                                                .child("A read-only Git monitor for the menu bar. MIT License."),
                                        ),
                                )
                                .into_any_element(),
                            div()
                                .h_flex()
                                .gap_1()
                                .child(
                                    Button::new("source-repository")
                                        .small()
                                        .ghost()
                                        .icon(IconName::Github)
                                        .label("Source")
                                        .on_click({
                                            let model = model.clone();
                                            move |_, _, cx| {
                                                if let Err(error) = macos::open_source_repository()
                                                {
                                                    model.update(cx, |model, cx| {
                                                        model.show_error_message(error, cx);
                                                    });
                                                }
                                            }
                                        }),
                                )
                                .child(
                                    Button::new("third-party-licenses")
                                        .small()
                                        .ghost()
                                        .icon(IconName::ExternalLink)
                                        .label("Licenses")
                                        .on_click({
                                            let model = model.clone();
                                            move |_, _, cx| {
                                                if let Err(error) =
                                                    macos::open_third_party_licenses()
                                                {
                                                    model.update(cx, |model, cx| {
                                                        model.show_error_message(error, cx);
                                                    });
                                                }
                                            }
                                        }),
                                )
                                .child(
                                    Button::new("releases")
                                        .small()
                                        .ghost()
                                        .icon(IconName::ExternalLink)
                                        .label("Releases")
                                        .on_click({
                                            let model = model.clone();
                                            move |_, _, cx| {
                                                if let Err(error) = macos::open_releases() {
                                                    model.update(cx, |model, cx| {
                                                        model.show_error_message(error, cx);
                                                    });
                                                }
                                            }
                                        }),
                                )
                                .into_any_element(),
                            cx,
                        )),
                    ),
            )
            .child(
                div()
                    .v_flex()
                    .gap(px(HEADING_GAP))
                    .child(
                        div()
                            .text_heading()
                            .text_color(theme.danger)
                            .child("Danger zone"),
                    )
                    .child(
                        card(cx)
                            .border_color(theme.danger.opacity(0.6))
                            .child(setting_row(
                                "Reset GitSync",
                                "Forget every repository, setting, and cached status, and turn off launch at login. Repository files are not changed.",
                                Button::new("reset-gitsync")
                                    .small()
                                    .danger()
                                    .label("Reset…")
                                    .on_click({
                                        let model = model.clone();
                                        move |_, window, cx| {
                                            let model = model.clone();
                                            modal::confirm_in_window(
                                                window,
                                                "Reset GitSync?",
                                                macos::RESET_DETAIL,
                                                "Reset",
                                                move |cx| model.update(cx, |model, cx| model.reset(cx)),
                                                cx,
                                            );
                                        }
                                    })
                                    .into_any_element(),
                                cx,
                            )),
                    ),
            )
            .into_any_element()
    }
}

/// Shows "Copied" on a button for a moment, then restores its label.
fn flash_copied(view: &Entity<SettingsView>, repository: Option<Uuid>, cx: &mut App) {
    view.update(cx, |view, cx| {
        match repository {
            Some(id) => view.copied_repository = Some(id),
            None => view.copied_diagnostics = true,
        }
        cx.notify();
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            view.update(cx, |view, cx| {
                match repository {
                    Some(id) if view.copied_repository == Some(id) => view.copied_repository = None,
                    Some(_) => {}
                    None => view.copied_diagnostics = false,
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    });
}

/// A row of mutually exclusive choices, such as an interval or a theme.
fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str)],
    current: T,
    on_select: impl Fn(T, &mut App) + 'static,
) -> AnyElement {
    let values: Vec<T> = options.iter().map(|(value, _)| *value).collect();
    ButtonGroup::new(id)
        .small()
        .compact()
        .children(options.iter().enumerate().map(|(index, (value, label))| {
            let selected = *value == current;
            Button::new(format!("{id}-{index}"))
                .label(*label)
                .selected(selected)
                .when(selected, |button| button.primary())
        }))
        .on_click(move |clicked, _, cx| {
            if let Some(value) = clicked.first().and_then(|index| values.get(*index)) {
                on_select(*value, cx);
            }
        })
        .into_any_element()
}

/// A titled group of settings: a heading and description above a card that
/// holds the rows, divided by hairlines.
fn group<V: 'static>(
    title: &'static str,
    description: &'static str,
    rows: impl IntoIterator<Item = AnyElement>,
    cx: &Context<V>,
) -> AnyElement {
    let border = cx.theme().border;
    div()
        .v_flex()
        .gap(px(HEADING_GAP))
        .child(group_heading(title, description, cx))
        .child(
            card(cx).children(rows.into_iter().enumerate().map(|(index, row)| {
                div()
                    .when(index > 0, |this| this.border_t_1().border_color(border))
                    .child(row)
            })),
        )
        .into_any_element()
}

/// A group's title, with an optional description under it.
fn group_heading<V: 'static>(
    title: &'static str,
    description: &'static str,
    cx: &Context<V>,
) -> AnyElement {
    div()
        .v_flex()
        .gap(px(2.0))
        .px(px(2.0))
        .child(div().text_heading().child(title))
        .when(!description.is_empty(), |this| {
            this.child(
                div()
                    .text_secondary()
                    .text_color(cx.theme().muted_foreground)
                    .child(description),
            )
        })
        .into_any_element()
}

/// The rounded, filled container that sets a group apart from the page.
fn card<V: 'static>(cx: &Context<V>) -> gpui_kit::Div {
    div()
        .v_flex()
        .overflow_hidden()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().muted.opacity(0.7))
}

/// One line of a card: a label column and its controls on the right.
fn row<V: 'static>(label: AnyElement, control: AnyElement, _: &Context<V>) -> AnyElement {
    div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_4()
        .px(px(ROW_PADDING_X))
        .py(px(ROW_PADDING_Y))
        .min_h(px(48.0))
        .child(label)
        .child(div().flex_none().child(control))
        .into_any_element()
}

fn setting_row<V: 'static>(
    title: &'static str,
    description: &'static str,
    control: AnyElement,
    cx: &Context<V>,
) -> AnyElement {
    row(
        div()
            .v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(2.0))
            .child(div().text_row_title().child(title))
            .child(
                div()
                    .text_secondary()
                    .text_color(cx.theme().muted_foreground)
                    .whitespace_normal()
                    .child(description),
            )
            .into_any_element(),
        control,
        cx,
    )
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ConnectionTest {
    #[default]
    Idle,
    Running,
}

struct RemoteFolderView {
    model: Entity<AppModel>,
    destination: Entity<InputState>,
    root: Entity<InputState>,
    test: ConnectionTest,
    /// The last connection test or scan result, shown under the fields.
    status: Option<Result<String, String>>,
    _subscriptions: Vec<Subscription>,
}

impl RemoteFolderView {
    fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let destination = cx.new(|cx| {
            InputState::new(window, cx).placeholder("user@host or an alias from ~/.ssh/config")
        });
        let root = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("~/Git")
                .placeholder("~/Git or /absolute/path")
        });
        let subscriptions = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, {
                let model = model.clone();
                move |_, window, cx| {
                    if model.read(cx).theme_preference() == ThemePreference::System {
                        crate::app::follow_system_appearance(window.appearance(), window, cx);
                        cx.refresh_windows();
                    }
                }
            }),
            cx.observe_window_activation(window, |_, window, cx| {
                if !window.is_window_active() {
                    crate::panel::close_when_app_inactive(cx);
                }
            }),
            cx.subscribe_in(&model, window, |this, _, event, window, cx| match event {
                ScanEvent::RemoteFinished { result: Ok(()), .. } => window.remove_window(),
                ScanEvent::RemoteFinished {
                    result: Err(error), ..
                } => {
                    this.status = Some(Err(error.clone()));
                    cx.notify();
                }
            }),
            cx.subscribe(&destination, |this, _, event, cx| match event {
                InputEvent::PressEnter { .. } => this.scan(cx),
                InputEvent::Change => {
                    this.status = None;
                    cx.notify();
                }
                _ => {}
            }),
            cx.subscribe(&root, |this, _, event, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.scan(cx);
                }
            }),
        ];
        destination.read(cx).focus_handle(cx).focus(window, cx);

        Self {
            model,
            destination,
            root,
            test: ConnectionTest::Idle,
            status: None,
            _subscriptions: subscriptions,
        }
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        if self.model.read(cx).adding_repository {
            return;
        }
        self.status = None;
        let destination = self.destination.read(cx).value().to_string();
        let root = self.root.read(cx).value().to_string();
        self.model.update(cx, |model, cx| {
            model.add_remote_folder(&destination, &root, cx);
        });
    }

    fn test_connection(&mut self, cx: &mut Context<Self>) {
        if self.test == ConnectionTest::Running {
            return;
        }
        self.test = ConnectionTest::Running;
        self.status = None;
        let destination = self.destination.read(cx).value().to_string();
        let root = self.root.read(cx).value().to_string();
        let task = self
            .model
            .update(cx, |model, cx| model.test_remote(&destination, &root, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.test = ConnectionTest::Idle;
                this.status = Some(result);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

impl Render for RemoteFolderView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui_kit::IntoElement {
        let app = self.model.read(cx);
        let scanning = app.adding_repository;
        let progress = app.scan_progress.as_ref().map(|progress| {
            (
                progress.found.load(AtomicOrdering::Relaxed),
                progress.checked.load(AtomicOrdering::Relaxed),
            )
        });
        let model = self.model.clone();
        let view = cx.entity().clone();
        let testing = self.test == ConnectionTest::Running;
        let status = self.status.clone();
        let theme = cx.theme();

        div()
            .v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_body()
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(16.0))
                    .px(px(PAGE_PADDING))
                    .pt(px(PAGE_PADDING))
                    .child(
                        div()
                            .h_flex()
                            .items_start()
                            .gap_3()
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(36.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_lg()
                                    .bg(theme.muted)
                                    .border_1()
                                    .border_color(theme.border)
                                    .child(Icon::new(IconName::Globe).size(px(18.0))),
                            )
                            .child(
                                div()
                                    .v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(2.0))
                                    .child(div().text_title().child("Add an SSH computer"))
                                    .child(
                                        div()
                                            .text_secondary()
                                            .text_color(theme.muted_foreground)
                                            .whitespace_normal()
                                            .child("Uses your OpenSSH config, keys, and agent. GitSync never stores a password or private key. Connect once in Terminal first, so the host key is known."),
                                    ),
                            ),
                    )
                    .child(field(
                        "SSH destination",
                        Input::new(&self.destination).small().into_any_element(),
                        None,
                        cx,
                    ))
                    .child(field(
                        "Folder to scan",
                        Input::new(&self.root).small().into_any_element(),
                        Some("~/Git is a folder in the remote home directory. You can also enter an absolute path."),
                        cx,
                    ))
                    .when(scanning, |this| {
                        let text = match progress {
                            Some((found, checked)) if found > 0 => {
                                format!("Found {found} repositories · checking {checked} of {found}…")
                            }
                            _ => "Connecting and looking for repositories…".to_owned(),
                        };
                        this.child(status_line(
                            Icon::new(IconName::LoaderCircle),
                            text,
                            theme.muted_foreground,
                        ))
                    })
                    .when_some(status.filter(|_| !scanning), |this, status| {
                        this.child(match status {
                            Ok(message) => status_line(
                                Icon::new(IconName::CircleCheck),
                                message,
                                theme.success,
                            ),
                            Err(error) => status_line(
                                Icon::new(IconName::TriangleAlert),
                                error,
                                theme.danger,
                            ),
                        })
                    }),
            )
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .justify_between()
                    .gap_2()
                    .px(px(PAGE_PADDING))
                    .py(px(12.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("test-remote")
                            .small()
                            .outline()
                            .label("Test Connection")
                            .tooltip("Check the SSH login and Git, and count the repositories a scan would find")
                            .loading(testing)
                            .disabled(testing || scanning)
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| view.update(cx, |view, cx| view.test_connection(cx))
                            }),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(if scanning {
                                Button::new("cancel-remote")
                                    .small()
                                    .ghost()
                                    .label("Cancel Scan")
                                    .on_click({
                                        let model = model.clone();
                                        move |_, _, cx| {
                                            model.update(cx, |model, cx| model.cancel_scan(cx));
                                        }
                                    })
                            } else {
                                Button::new("close-remote")
                                    .small()
                                    .ghost()
                                    .label("Close")
                                    .on_click(|_, window, _| window.remove_window())
                            })
                            .child(
                                Button::new("scan-remote")
                                    .small()
                                    .primary()
                                    .label("Scan")
                                    .loading(scanning)
                                    .disabled(scanning)
                                    .on_click(move |_, _, cx| view.update(cx, |view, cx| view.scan(cx))),
                            ),
                    ),
            )
    }
}

/// Shows a local path under the home folder as `~/…`, the way Finder and
/// Terminal users read it. Remote paths are left as they are.
/// The folder as the user edits it: `~/Git/app` for a path in the home
/// folder, here or on an SSH computer, otherwise the absolute path.
fn editable_path(remote: bool, selected_path: &Path, canonical_path: &Path) -> String {
    if remote {
        if selected_path.is_absolute() {
            return selected_path.display().to_string();
        }
        if selected_path == Path::new(".") {
            return "~".to_owned();
        }
        return format!("~/{}", selected_path.display());
    }
    abbreviate_home(&canonical_path.to_string_lossy(), false)
}

fn abbreviate_home(path: &str, remote: bool) -> String {
    if !remote
        && let Some(home) = std::env::var_os("HOME")
        && let Some(home) = home.to_str()
        && !home.is_empty()
        && let Some(rest) = path.strip_prefix(home)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return format!("~{rest}");
    }
    path.to_owned()
}

/// A labeled form field with an optional hint under it.
fn field<V: 'static>(
    label: &'static str,
    input: AnyElement,
    hint: Option<&'static str>,
    cx: &Context<V>,
) -> AnyElement {
    div()
        .v_flex()
        .gap(px(6.0))
        .child(div().text_row_title().child(label))
        .child(input)
        .when_some(hint, |this, hint| {
            this.child(
                div()
                    .text_secondary()
                    .text_color(cx.theme().muted_foreground)
                    .whitespace_normal()
                    .child(hint),
            )
        })
        .into_any_element()
}

fn status_line(icon: Icon, text: String, color: gpui_kit::Hsla) -> AnyElement {
    div()
        .h_flex()
        .items_start()
        .gap_2()
        .child(div().pt(px(2.0)).child(icon.small().text_color(color)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .text_secondary()
                .child(text),
        )
        .into_any_element()
}

#[derive(Clone)]
struct SettingsRepository {
    id: Uuid,
    input: Entity<InputState>,
    path_input: Entity<InputState>,
    name: String,
    path: PathBuf,
    ssh_destination: Option<String>,
    paused: bool,
}

fn render_repository(
    repository: SettingsRepository,
    model: &Entity<AppModel>,
    view: &Entity<SettingsView>,
    copied_repository: Option<Uuid>,
    highlighted: bool,
    cx: &Context<SettingsView>,
) -> AnyElement {
    let theme = cx.theme();
    let id = repository.id;
    let path_text = copy_path_text(repository.ssh_destination.as_deref(), &repository.path);
    let ssh_destination = repository.ssh_destination.clone();
    let row_group = format!("repository-{id}");

    div()
        .group(row_group.clone())
        .h_flex()
        .items_center()
        .gap_3()
        .pl(px(ROW_PADDING_X - 6.0))
        .pr(px(ROW_PADDING_X))
        .py(px(8.0))
        .border_t_1()
        .border_color(theme.border)
        // The dark palette's accent is close to the card color, so the
        // highlight uses a tint of the brand color instead.
        .when(highlighted, |this| this.bg(theme.success.opacity(0.10)))
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(1.0))
                // The name field looks like text until it is hovered, so the
                // list reads as names, not as a form.
                .child(
                    div()
                        .rounded_md()
                        .border_1()
                        .border_color(gpui_kit::transparent_black())
                        .hover(|style| style.border_color(theme.border).bg(theme.background))
                        .child(
                            Input::new(&repository.input)
                                .small()
                                .appearance(false)
                                .font_weight(gpui_kit::FontWeight::MEDIUM),
                        ),
                )
                // The folder is editable the same way: Enter points the
                // repository at another folder, for one that moved.
                .child(
                    div()
                        .rounded_md()
                        .border_1()
                        .border_color(gpui_kit::transparent_black())
                        .text_caption()
                        .text_color(theme.muted_foreground)
                        .hover(|style| style.border_color(theme.border).bg(theme.background))
                        .child(Input::new(&repository.path_input).small().appearance(false)),
                ),
        )
        .child(
            div()
                .h_flex()
                .flex_none()
                .items_center()
                .gap(px(2.0))
                // Actions stay quiet until the pointer is on the row.
                .opacity(0.45)
                .group_hover(row_group, |style| style.opacity(1.0))
                .child(
                    Button::new(format!("open-{id}"))
                        .small()
                        .ghost()
                        .icon(IconName::SquareTerminal)
                        .tooltip(if ssh_destination.is_some() {
                            "Open SSH session"
                        } else {
                            "Open in Terminal"
                        })
                        .on_click({
                            let model = model.clone();
                            let path = repository.path.clone();
                            let destination = ssh_destination.clone();
                            move |_, _, cx| {
                                let result = match &destination {
                                    Some(destination) => {
                                        macos::open_ssh_terminal(destination, &path)
                                    }
                                    None => macos::open_in_terminal(&path),
                                };
                                if let Err(error) = result {
                                    model.update(cx, |model, cx| {
                                        model.show_error_message(error, cx)
                                    });
                                }
                            }
                        }),
                )
                .when(ssh_destination.is_none(), |this| {
                    this.child(
                        Button::new(format!("reveal-{id}"))
                            .small()
                            .ghost()
                            .icon(IconName::FolderOpen)
                            .tooltip("Reveal in Finder")
                            .on_click({
                                let model = model.clone();
                                let path = repository.path.clone();
                                move |_, _, cx| {
                                    if let Err(error) = macos::reveal_in_finder(&path) {
                                        model.update(cx, |model, cx| {
                                            model.show_error_message(error, cx)
                                        });
                                    }
                                }
                            }),
                    )
                })
                .child(
                    Button::new(format!("copy-path-{id}"))
                        .small()
                        .ghost()
                        .icon(if copied_repository == Some(id) {
                            IconName::Check
                        } else {
                            IconName::Copy
                        })
                        .tooltip("Copy path")
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(path_text.clone()));
                                flash_copied(&view, Some(id), cx);
                            }
                        }),
                )
                .child(
                    Button::new(format!("settings-remove-{id}"))
                        .small()
                        .ghost()
                        .icon(IconName::Delete)
                        .tooltip("Remove from GitSync")
                        .on_click({
                            let model = model.clone();
                            let name = repository.name.clone();
                            move |_, window, cx| {
                                let model = model.clone();
                                modal::confirm_in_window(
                                    window,
                                    &format!("Remove {name} from GitSync?"),
                                    macos::REMOVE_REPOSITORY_DETAIL,
                                    "Remove",
                                    move |cx| model.update(cx, |model, cx| model.remove(id, cx)),
                                    cx,
                                );
                            }
                        }),
                ),
        )
        .child(
            div().flex_none().pl_1().child(
                Switch::new(format!("repository-enabled-{id}"))
                    .small()
                    .checked(!repository.paused)
                    .tooltip(if repository.paused {
                        "Resume checking"
                    } else {
                        "Pause checking"
                    })
                    .accessibility_label("Monitor repository")
                    .on_click({
                        let model = model.clone();
                        move |_, _, cx| model.update(cx, |model, cx| model.toggle_paused(id, cx))
                    }),
            ),
        )
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_repository_group(
    group: RepositoryGroup,
    name_input: Option<Entity<InputState>>,
    collapsed: bool,
    members: Vec<SettingsRepository>,
    model: &Entity<AppModel>,
    view: &Entity<SettingsView>,
    copied_repository: Option<Uuid>,
    highlighted: Option<Uuid>,
    cx: &Context<SettingsView>,
) -> AnyElement {
    let theme = cx.theme();
    let RepositoryGroup {
        key,
        label,
        default_label,
        ssh_destination,
        paused,
        repository_count,
        attention,
    } = group;
    let description = repository_group_description(repository_count, attention);
    // A renamed SSH computer still shows where it connects.
    let description = match &ssh_destination {
        Some(destination) if label != default_label => format!("{destination} · {description}"),
        _ => description,
    };

    let mut controls = div().h_flex().flex_none().items_center().gap(px(2.0));
    {
        let destination = ssh_destination.clone();
        let (tooltip, title, detail) = match &destination {
            Some(destination) => (
                "Remove this computer from GitSync",
                format!("Remove {destination} from GitSync?"),
                macos::REMOVE_HOST_DETAIL,
            ),
            None => (
                "Remove this Mac's repositories from GitSync",
                format!("Remove every repository on {label} from GitSync?"),
                macos::REMOVE_LOCAL_DETAIL,
            ),
        };
        controls = controls.child(
            Button::new(format!("settings-group-remove-{key}"))
                .small()
                .ghost()
                .icon(IconName::Delete)
                .tooltip(tooltip)
                .on_click({
                    let model = model.clone();
                    move |_, window, cx| {
                        let model = model.clone();
                        let destination = destination.clone();
                        modal::confirm_in_window(
                            window,
                            &title,
                            detail,
                            "Remove",
                            move |cx| {
                                model.update(cx, |model, cx| {
                                    model.remove_destination(destination.as_deref(), cx);
                                });
                            },
                            cx,
                        );
                    }
                }),
        );
    }
    controls = controls.child(
        div().pl_2().child(
            Switch::new(format!("settings-group-enabled-{key}"))
                .small()
                .checked(!paused)
                .tooltip("Check every repository from this computer")
                .accessibility_label("Monitor every repository from this computer")
                .on_click({
                    let model = model.clone();
                    let ssh_destination = ssh_destination.clone();
                    move |enabled, _, cx| {
                        let paused = !*enabled;
                        model.update(cx, |model, cx| {
                            model.set_destination_paused(ssh_destination.as_deref(), paused, cx);
                        });
                    }
                }),
        ),
    );

    card(cx)
        .child(
            div()
                .h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_3()
                .pl(px(ROW_PADDING_X - 4.0))
                .pr(px(ROW_PADDING_X))
                .py(px(10.0))
                .child(
                    div()
                        .id(format!("settings-group-{key}"))
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .flex_none()
                        .py_1()
                        .px_1()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.background.opacity(0.6)))
                        .tooltip(move |window, cx| {
                            Tooltip::new(if collapsed {
                                "Show repositories"
                            } else {
                                "Hide repositories"
                            })
                            .build(window, cx)
                        })
                        .on_click({
                            let key = key.clone();
                            let view = view.clone();
                            move |_, _, cx| {
                                cx.global_mut::<RepositoryGroupState>()
                                    .set_collapsed(&key, !collapsed);
                                view.update(cx, |_, cx| cx.notify());
                            }
                        })
                        .child(
                            Icon::new(if collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall()
                            .text_color(theme.muted_foreground),
                        )
                        .child(
                            div()
                                .flex_none()
                                .size(px(28.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .bg(theme.background)
                                .border_1()
                                .border_color(theme.border)
                                .child(
                                    Icon::new(if ssh_destination.is_some() {
                                        IconName::Globe
                                    } else {
                                        IconName::HardDrive
                                    })
                                    .small()
                                    .text_color(theme.foreground),
                                ),
                        ),
                )
                .child(
                    div()
                        .v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(1.0))
                        .child(match name_input {
                            // The name field looks like a heading until it is
                            // hovered, like a repository's name.
                            Some(input) => div()
                                .rounded_md()
                                .border_1()
                                .border_color(gpui_kit::transparent_black())
                                .hover(|style| {
                                    style.border_color(theme.border).bg(theme.background)
                                })
                                .child(
                                    Input::new(&input)
                                        .small()
                                        .appearance(false)
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD),
                                )
                                .into_any_element(),
                            None => div()
                                .px(px(8.0))
                                .text_heading()
                                .truncate()
                                .child(label.clone())
                                .into_any_element(),
                        })
                        .child(
                            div()
                                .px(px(8.0))
                                .text_secondary()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(description),
                        ),
                )
                .child(controls),
        )
        .when(!collapsed, |this| {
            this.children(members.into_iter().map(|repository| {
                let is_highlighted = highlighted == Some(repository.id);
                render_repository(
                    repository,
                    model,
                    view,
                    copied_repository,
                    is_highlighted,
                    cx,
                )
            }))
        })
        .into_any_element()
}

pub(crate) fn open(model: Entity<AppModel>, cx: &mut App) {
    open_at(model, SettingsTab::General, None, cx);
}

/// Opens Settings on `tab`, optionally highlighting and focusing one
/// repository's name field.
pub(crate) fn open_at(
    model: Entity<AppModel>,
    tab: SettingsTab,
    repository: Option<Uuid>,
    cx: &mut App,
) {
    let theme = model.read(cx).theme_preference();
    AppModel::apply_theme(theme, cx);
    macos::activate_application();
    let state = cx.global::<SettingsWindowState>();
    if let (Some(window), Some(view)) = (state.window, state.view.clone()) {
        if window
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            view.update(cx, |view, cx| view.show(tab, repository, cx));
            return;
        }
        let state = cx.global_mut::<SettingsWindowState>();
        state.window = None;
        state.view = None;
    }

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(640.0), px(680.0)), cx)),
        window_min_size: Some(size(px(560.0), px(440.0))),
        titlebar: Some(TitlebarOptions {
            title: Some("GitSync Settings".into()),
            ..TitlebarOptions::default()
        }),
        focus: true,
        show: true,
        is_resizable: true,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Opaque,
        ..WindowOptions::default()
    };
    let mut created_view = None;
    match cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| SettingsView::new(model, tab, repository, window, cx));
        created_view = Some(view.clone());
        cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
    }) {
        Ok(window) => {
            let _ = window.update(cx, |_, window, _| window.activate_window());
            let state = cx.global_mut::<SettingsWindowState>();
            state.window = Some(window);
            state.view = created_view;
        }
        Err(error) => eprintln!("failed to open GitSync settings: {error:#}"),
    }
}

/// Whether the "Add SSH Computer" window is still open.
pub(crate) fn remote_window_open(cx: &mut App) -> bool {
    cx.global::<SettingsWindowState>()
        .remote_window
        .is_some_and(|window| window.update(cx, |_, _, _| ()).is_ok())
}

pub(crate) fn open_remote(model: Entity<AppModel>, cx: &mut App) {
    let theme = model.read(cx).theme_preference();
    AppModel::apply_theme(theme, cx);
    macos::activate_application();
    if let Some(window) = cx.global::<SettingsWindowState>().remote_window {
        if window
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            return;
        }
        cx.global_mut::<SettingsWindowState>().remote_window = None;
    }

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(500.0), px(420.0)), cx)),
        titlebar: Some(TitlebarOptions {
            title: Some("Add SSH Computer".into()),
            ..TitlebarOptions::default()
        }),
        focus: true,
        show: true,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Opaque,
        ..WindowOptions::default()
    };
    match cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| RemoteFolderView::new(model, window, cx));
        cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
    }) {
        Ok(window) => {
            let _ = window.update(cx, |_, window, _| window.activate_window());
            cx.global_mut::<SettingsWindowState>().remote_window = Some(window);
        }
        Err(error) => eprintln!("failed to open GitSync remote folder window: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::abbreviate_home;

    #[test]
    fn local_paths_under_home_read_as_tilde() {
        let home = std::env::var("HOME").expect("tests run with HOME set");
        assert_eq!(
            abbreviate_home(&format!("{home}/Git/app"), false),
            "~/Git/app"
        );
        assert_eq!(abbreviate_home(&home, false), "~");
        // A sibling folder that only shares the prefix is not under home.
        assert_eq!(
            abbreviate_home(&format!("{home}-other/app"), false),
            format!("{home}-other/app")
        );
        // Remote paths belong to another computer's home.
        assert_eq!(
            abbreviate_home(&format!("{home}/Git/app"), true),
            format!("{home}/Git/app")
        );
    }
}
