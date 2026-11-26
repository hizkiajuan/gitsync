//! Native file pickers and confirmation questions, started where they cannot
//! abort GitSync.
//!
//! A blocking `NSOpenPanel` or `NSAlert` modal loop keeps serving GPUI's
//! timers and window callbacks. Started from inside a click handler, while
//! GPUI holds the app borrowed, the first of those callbacks to read the app
//! panics on the borrow and aborts the process: choosing "Folder…" in
//! Settings closed GitSync this way. File pickers here use GPUI's
//! non-blocking prompt, and alerts run from a spawned task, after the click
//! handler has returned and released the app.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::{App, Entity, PathPromptOptions, PromptLevel, Window};

use crate::app::AppModel;
use crate::macos;

/// How long the panel takes to close before a question replaces it. Closing
/// a window during an `NSAlert` modal session ends the session as if its
/// first button had been pressed, which would confirm what nobody confirmed.
pub(crate) const AFTER_PANEL_CLOSES: Duration = Duration::from_millis(400);

/// Asks for a local folder, then scans it for repositories.
pub(crate) fn choose_folder_and_scan(model: Entity<AppModel>, cx: &mut App) {
    choose_path(false, "Scan Folder", cx, move |path, cx| {
        model.update(cx, |model, cx| model.add_repository(path, cx));
    });
}

/// Asks for the folder a repository moved to, then points it there.
pub(crate) fn choose_folder_and_relocate(model: Entity<AppModel>, id: uuid::Uuid, cx: &mut App) {
    choose_path(false, "Use Folder", cx, move |path, cx| {
        model.update(cx, |model, cx| {
            model.relocate_repository(id, &path.to_string_lossy(), cx)
        });
    });
}

/// Asks for a Git executable, then uses it for every check.
pub(crate) fn choose_git_executable(model: Entity<AppModel>, cx: &mut App) {
    choose_path(true, "Choose Git", cx, move |path, cx| {
        model.update(cx, |model, cx| model.set_git_executable(path, cx));
    });
}

fn choose_path(
    files: bool,
    prompt: &'static str,
    cx: &mut App,
    then: impl FnOnce(PathBuf, &mut App) + 'static,
) {
    // An accessory app is not active after its panel closes, so the picker
    // would otherwise open behind the frontmost app.
    macos::activate_application();
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files,
        directories: !files,
        multiple: false,
        prompt: Some(prompt.into()),
    });
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(paths))) = paths.await
            && let Some(path) = paths.into_iter().next()
        {
            cx.update(|cx| then(path, cx));
        }
    })
    .detach();
}

/// Runs `ask`, a blocking native question, from a task of its own after
/// `delay`, and runs `then` when it returns true.
pub(crate) fn confirm(
    delay: Duration,
    ask: impl FnOnce() -> bool + 'static,
    then: impl FnOnce(&mut App) + 'static,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        if !delay.is_zero() {
            cx.background_executor().timer(delay).await;
        }
        if ask() {
            cx.update(then);
        }
    })
    .detach();
}

/// Asks a destructive question as a sheet on `window`, and runs `then` when
/// the user picks `action`. A sheet does not block, so GPUI keeps running.
pub(crate) fn confirm_in_window(
    window: &mut Window,
    message: &str,
    detail: &str,
    action: &'static str,
    then: impl FnOnce(&mut App) + 'static,
    cx: &mut App,
) {
    let answer = window.prompt(
        PromptLevel::Warning,
        message,
        Some(detail),
        &[action, "Cancel"],
        cx,
    );
    cx.spawn(async move |cx| {
        if answer.await == Ok(0) {
            cx.update(then);
        }
    })
    .detach();
}
