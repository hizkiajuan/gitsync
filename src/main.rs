mod app;
mod macos;
mod modal;
mod panel;
mod settings;
mod ui;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use app::{AppModel, RepositoryGroupState};
use gpui_kit::{AppContext as _, KeyBinding, Subscription, SystemNotificationResponse};
use macos::StatusItemController;
use panel::{
    CopySelectedPath, DismissPanel, OpenSelected, PanelState, SelectNext, SelectPrevious, toggle,
};
use settings::SettingsWindowState;
use uuid::Uuid;

gpui_kit::actions!(gitsync, [OpenSettings, RefreshAll, AddRepository]);

struct LifecycleSubscriptions {
    _wake: Subscription,
    _quit: Subscription,
}

impl gpui_kit::Global for LifecycleSubscriptions {}

fn main() {
    // Icons used by the panel and Settings (chevrons, menus, status) come
    // from the bundled GPUI Kit asset set, plus the GitSync mark.
    gpui_kit::application().with_assets(ui::Assets).run(|cx| {
        gpui_kit::init(cx);
        cx.set_global(PanelState::default());
        cx.set_global(SettingsWindowState::default());
        cx.set_global(RepositoryGroupState::default());
        cx.bind_keys([
            KeyBinding::new("escape", DismissPanel, None),
            KeyBinding::new("down", SelectNext, Some("GitSyncPanel")),
            KeyBinding::new("up", SelectPrevious, Some("GitSyncPanel")),
            KeyBinding::new("enter", OpenSelected, Some("GitSyncPanel")),
            KeyBinding::new("cmd-c", CopySelectedPath, Some("GitSyncPanel")),
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-r", RefreshAll, None),
            KeyBinding::new("cmd-o", AddRepository, None),
        ]);

        let model = cx.new(|_| AppModel::load().unwrap_or_else(AppModel::empty_for_error));
        cx.on_action({
            let model = model.clone();
            move |_: &OpenSettings, cx| settings::open(model.clone(), cx)
        });
        cx.on_action({
            let model = model.clone();
            move |_: &RefreshAll, cx| {
                model.update(cx, |model, cx| model.refresh_all(cx));
            }
        });
        cx.on_action({
            let model = model.clone();
            move |_: &AddRepository, cx| modal::choose_folder_and_scan(model.clone(), cx)
        });

        let theme = model.read(cx).theme_preference();
        AppModel::apply_theme(theme, cx);
        let app = cx.to_async();
        let panel_model = model.clone();
        let status_item = StatusItemController::install(move |anchor| {
            app.update(|cx| toggle(anchor, panel_model.clone(), cx));
        });
        status_item.set_summary(model.read(cx).status_summary());
        cx.set_global(status_item);

        // ⌘G opens or closes the panel from any app.
        let shortcut_app = cx.to_async();
        let shortcut_model = model.clone();
        if let Err(error) = macos::register_toggle_hot_key(move || {
            shortcut_app.update(|cx| {
                if let Some(anchor) = cx.global::<StatusItemController>().anchor() {
                    panel::toggle_from_shortcut(anchor, shortcut_model.clone(), cx);
                }
            });
        }) {
            model.update(cx, |model, cx| model.show_error_message(error, cx));
        }

        cx.on_system_notification_response({
            let model = model.clone();
            move |response, cx| {
                let SystemNotificationResponse { tag, .. } = response;
                // A single-repository banner focuses that repository; a
                // combined banner opens the panel with its usual sorting.
                let repository_id = tag
                    .strip_prefix("repository:")
                    .and_then(|id| Uuid::parse_str(id).ok());
                if (repository_id.is_some() || tag == "repositories")
                    && let Some(anchor) = cx.global::<StatusItemController>().anchor()
                {
                    panel::show_repository(anchor, model.clone(), repository_id, cx);
                }
            }
        });

        let wake_generation = Rc::new(Cell::new(0_u64));
        let wake_subscription = cx.on_system_wake({
            let model = model.clone();
            let background = cx.background_executor().clone();
            move |cx| {
                if !model.read(cx).refresh_on_wake() {
                    return;
                }
                let generation = wake_generation.get().wrapping_add(1);
                wake_generation.set(generation);
                let model = model.clone();
                let wake_generation = wake_generation.clone();
                let background = background.clone();
                cx.spawn(async move |cx| {
                    background.timer(Duration::from_secs(5)).await;
                    if wake_generation.get() == generation
                        && model.read_with(cx, |model, _| model.refresh_on_wake())
                    {
                        model.update(cx, |model, cx| model.refresh_all(cx));
                    }
                })
                .detach();
            }
        });
        let quit_subscription = cx.on_app_quit({
            let model = model.clone();
            move |cx| {
                model.update(cx, |model, _| model.flush_cache());
                async {}
            }
        });
        cx.set_global(LifecycleSubscriptions {
            _wake: wake_subscription,
            _quit: quit_subscription,
        });

        model.update(cx, |model, cx| model.refresh_all(cx));
        #[cfg(debug_assertions)]
        debug_open_window(&model, cx);
        // The model remembers when the last full refresh started, so a
        // manual refresh also restarts the interval.
        let scheduler_model = model;
        let background = cx.background_executor().clone();
        cx.spawn(async move |cx| {
            loop {
                const HEARTBEAT: Duration = Duration::from_secs(15);
                background.timer(HEARTBEAT).await;
                if scheduler_model.read_with(cx, |model, _| model.refresh_due()) {
                    scheduler_model.update(cx, |model, cx| model.refresh_all(cx));
                }
            }
        })
        .detach();
    });
}

/// Development aid for smoke tests that cannot click the menu bar: debug
/// builds open `panel`, `settings`, `repositories`, `advanced`, or `remote`
/// at launch when `GITSYNC_DEBUG_OPEN` names it.
#[cfg(debug_assertions)]
fn debug_open_window(model: &gpui_kit::Entity<AppModel>, cx: &mut gpui_kit::App) {
    use settings::SettingsTab;

    let Ok(target) = std::env::var("GITSYNC_DEBUG_OPEN") else {
        return;
    };
    let model = model.clone();
    // `panel:<group key>` opens the panel on one computer, such as
    // `panel:local` or `panel:remote:workstation`.
    //
    // `panel:<first>><second>` opens on the first computer, then switches to
    // the second after two seconds, like a rail click.
    let target = match target.split_once(':') {
        Some(("panel", keys)) => {
            let (first, second) = match keys.split_once('>') {
                Some((first, second)) => (first, Some(second.to_owned())),
                None => (keys, None),
            };
            let view = |key: &str| match key {
                "overview" => panel::PanelView::Overview,
                key => panel::PanelView::Computer(key.to_owned()),
            };
            panel::set_view(view(first), cx);
            if let Some(second) = second {
                let next = view(&second);
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(2))
                        .await;
                    cx.update(|cx| panel::switch_view(next, cx));
                })
                .detach();
            }
            "panel".to_owned()
        }
        _ => target,
    };
    // A spawned task, not `cx.defer`: with nothing to refresh, no effect
    // cycle follows startup, and a deferred callback would never run.
    let open = move |cx: &mut gpui_kit::App| match target.as_str() {
        "panel" => {
            // A launch from a terminal leaves the app inactive, and the
            // panel closes itself when it is not active.
            macos::activate_application();
            match cx.global::<StatusItemController>().anchor() {
                Some(anchor) => panel::show_repository(anchor, model, None, cx),
                None => eprintln!("GITSYNC_DEBUG_OPEN: the status item has no window yet"),
            }
        }
        "settings" => settings::open_at(model, SettingsTab::General, None, cx),
        "repositories" => settings::open_at(model, SettingsTab::Repositories, None, cx),
        "advanced" => settings::open_at(model, SettingsTab::Advanced, None, cx),
        "remote" => {
            settings::open_remote(model.clone(), cx);
            // `GITSYNC_DEBUG_REMOTE=destination|folder` also starts a scan, so
            // a smoke test can cover a full remote scan without clicks.
            if let Ok(remote) = std::env::var("GITSYNC_DEBUG_REMOTE")
                && let Some((destination, folder)) = remote.split_once('|')
            {
                model.update(cx, |model, cx| {
                    model.add_remote_folder(destination, folder, cx)
                });
            }
        }
        _ => {}
    };
    cx.spawn(async move |cx| cx.update(open)).detach();
}
