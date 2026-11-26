use std::cell::RefCell;
use std::ffi::c_void;
use std::path::Path;
use std::process::{Command, Stdio};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertSecondButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy,
    NSCellImagePosition, NSColor, NSFont, NSFontAttributeName, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSImage, NSScreen, NSStatusBar, NSStatusBarButton,
    NSStatusItem, NSStringDrawing as _, NSView,
};
use objc2_foundation::{
    NSAttributedStringKey, NSData, NSDictionary, NSError, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString, ns_string,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::app::StatusSummary;
use crate::panel::StatusItemAnchor;

#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

/// `NSVariableStatusItemLength`: the item sizes itself to its image and title.
const VARIABLE_STATUS_ITEM_LENGTH: f64 = -1.0;
/// The GitSync mark, shown when nothing has a problem. It is drawn as a
/// template, so macOS tints it for the menu bar.
const MARK_SVG: &[u8] = include_bytes!("../resources/icon/mark.svg");
/// Points per side of the menu bar mark; its glyph is about 14 pt tall.
const MARK_POINTS: f64 = 18.0;
/// Height of the status item image; AppKit centers it in the menu bar.
const STATUS_IMAGE_HEIGHT: f64 = 22.0;
/// Point size of the stacked pull and push counts.
const COUNT_FONT_POINTS: f64 = 9.0;
/// Space between the icon and the counts.
const COUNT_GAP: f64 = 3.0;
/// Used instead of the mark if AppKit cannot read it.
const CLEAN_SYMBOL: &str = "arrow.triangle.branch";
/// Shown when at least one repository has a problem the user must fix.
const PROBLEM_SYMBOL: &str = "exclamationmark.triangle";

type ClickHandler = Box<dyn FnMut(StatusItemAnchor)>;

struct StatusItemTargetIvars {
    on_click: RefCell<ClickHandler>,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements, and the class does
    // not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GitSyncStatusItemTarget"]
    #[ivars = StatusItemTargetIvars]
    struct StatusItemTarget;

    unsafe impl NSObjectProtocol for StatusItemTarget {}

    impl StatusItemTarget {
        // SAFETY: The signature matches the one-argument target/action form.
        #[unsafe(method(statusItemClicked:))]
        fn status_item_clicked(&self, sender: &NSStatusBarButton) {
            if let Some(anchor) = status_item_anchor(sender, self.mtm()) {
                (self.ivars().on_click.borrow_mut())(anchor);
            }
        }
    }
);

impl StatusItemTarget {
    fn new(mtm: MainThreadMarker, on_click: ClickHandler) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(StatusItemTargetIvars {
            on_click: RefCell::new(on_click),
        });

        // SAFETY: `NSObject`'s `init` initializer is valid for this subclass.
        unsafe { msg_send![super(this), init] }
    }
}

pub(crate) struct StatusItemController {
    status_bar: Retained<NSStatusBar>,
    status_item: Retained<NSStatusItem>,
    mark: Option<Retained<NSImage>>,
    _target: Retained<StatusItemTarget>,
}

impl gpui_kit::Global for StatusItemController {}

impl StatusItemController {
    pub(crate) fn install(on_click: impl FnMut(StatusItemAnchor) + 'static) -> Self {
        let mtm =
            MainThreadMarker::new().expect("GitSync must initialize AppKit on the main thread");
        let application = NSApplication::sharedApplication(mtm);
        application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(VARIABLE_STATUS_ITEM_LENGTH);
        let button = status_item
            .button(mtm)
            .expect("AppKit did not provide an NSStatusBarButton");
        // The counts are drawn into the image, stacked beside the icon.
        button.setImagePosition(NSCellImagePosition::ImageOnly);
        let target = StatusItemTarget::new(mtm, Box::new(on_click));

        // SAFETY: `statusItemClicked:` is implemented by `StatusItemTarget` with
        // the one-argument target/action signature expected by `NSButton`.
        unsafe {
            button.setTarget(Some(&target));
            button.setAction(Some(sel!(statusItemClicked:)));
        }

        let controller = Self {
            status_bar,
            status_item,
            mark: menu_bar_mark(),
            _target: target,
        };
        controller.set_summary(StatusSummary::default());
        controller
    }

    /// Renders the menu bar item: the GitSync mark (or a warning symbol when
    /// a repository has a problem) with `↓pull` stacked over `↑push` beside
    /// it, only when there is something to pull or push.
    pub(crate) fn set_summary(&self, summary: StatusSummary) {
        let mtm = MainThreadMarker::new().expect("status updates must run on the main thread");
        let description = NSString::from_str(&menu_bar_description(summary));
        let lines = menu_bar_lines(summary);

        let Some(button) = self.status_item.button(mtm) else {
            return;
        };
        unsafe {
            let image = match &self.mark {
                Some(mark) if summary.problems == 0 => {
                    mark.setAccessibilityDescription(Some(&description));
                    Some(mark.clone())
                }
                _ => {
                    let symbol = NSString::from_str(if summary.problems > 0 {
                        PROBLEM_SYMBOL
                    } else {
                        CLEAN_SYMBOL
                    });
                    NSImage::imageWithSystemSymbolName_accessibilityDescription(
                        &symbol,
                        Some(&description),
                    )
                }
            };
            let image = match image {
                Some(icon) if !lines.is_empty() => Some(stacked_counts_image(icon, &lines)),
                image => image,
            };
            if let Some(image) = &image {
                image.setTemplate(true);
                image.setAccessibilityDescription(Some(&description));
            }
            button.setImage(image.as_deref());
            button.setTitle(ns_string!(""));
            button.setToolTip(Some(&description));
        }
    }

    pub(crate) fn anchor(&self) -> Option<StatusItemAnchor> {
        let mtm = MainThreadMarker::new().expect("status item access must run on the main thread");
        let button = self.status_item.button(mtm)?;
        status_item_anchor(&button, mtm)
    }
}

/// The GitSync mark as a menu bar template image, read from its SVG.
fn menu_bar_mark() -> Option<Retained<NSImage>> {
    let data = NSData::with_bytes(MARK_SVG);
    let image = NSImage::initWithData(NSImage::alloc(), &data)?;
    image.setSize(NSSize::new(MARK_POINTS, MARK_POINTS));
    image.setTemplate(true);
    Some(image)
}

impl Drop for StatusItemController {
    fn drop(&mut self) {
        self.status_bar.removeStatusItem(&self.status_item);
    }
}

/// The counts beside the menu bar icon, top line first: a down arrow with
/// commits waiting to be pulled over an up arrow with commits waiting to be
/// pushed. A zero side is left out.
fn menu_bar_lines(summary: StatusSummary) -> Vec<String> {
    let mut lines = Vec::new();
    if summary.pull > 0 {
        lines.push(format!("↓{}", summary.pull));
    }
    if summary.push > 0 {
        lines.push(format!("↑{}", summary.push));
    }
    lines
}

/// Draws `icon` with `lines` stacked to its right, in a small font, as one
/// image. One line is centered; two lines fill the menu bar height. The
/// image is drawn on demand, so it stays sharp on every display.
///
/// # Safety
///
/// Runs on the main thread.
unsafe fn stacked_counts_image(icon: Retained<NSImage>, lines: &[String]) -> Retained<NSImage> {
    // SAFETY: The caller runs on the main thread. Every object the drawing
    // block uses is retained by the block for as long as AppKit keeps it.
    unsafe {
        // Digits keep a fixed width so the item does not jitter as counts
        // change.
        let font =
            NSFont::monospacedDigitSystemFontOfSize_weight(COUNT_FONT_POINTS, NSFontWeightSemibold);
        // A template image uses only alpha, so the color is any opaque one.
        let color = NSColor::blackColor();
        let keys: [&NSAttributedStringKey; 2] =
            [NSFontAttributeName, NSForegroundColorAttributeName];
        let attributes: Retained<NSDictionary<NSAttributedStringKey, AnyObject>> =
            NSDictionary::from_retained_objects(
                &keys,
                &[
                    Retained::cast_unchecked::<AnyObject>(font),
                    Retained::cast_unchecked::<AnyObject>(color),
                ],
            );
        let lines: Vec<(Retained<NSString>, NSSize)> = lines
            .iter()
            .map(|line| {
                let line = NSString::from_str(line);
                let size = line.sizeWithAttributes(Some(&attributes));
                (line, size)
            })
            .collect();
        let icon_size = icon.size();
        let text_width = lines
            .iter()
            .map(|(_, size)| size.width)
            .fold(0.0, f64::max)
            .ceil();
        let size = NSSize::new(
            icon_size.width + COUNT_GAP + text_width,
            STATUS_IMAGE_HEIGHT,
        );
        let draw = RcBlock::new(move |_: NSRect| -> Bool {
            icon.drawInRect(NSRect::new(
                NSPoint::new(0.0, (STATUS_IMAGE_HEIGHT - icon_size.height) / 2.0),
                icon_size,
            ));
            let x = icon_size.width + COUNT_GAP;
            let middle = STATUS_IMAGE_HEIGHT / 2.0;
            match lines.as_slice() {
                [(line, line_size)] => {
                    line.drawAtPoint_withAttributes(
                        NSPoint::new(x, middle - line_size.height / 2.0),
                        Some(&attributes),
                    );
                }
                [(top, _), (bottom, bottom_size)] => {
                    // The lines overlap by a point, so their glyphs sit close
                    // together and inside the menu bar.
                    top.drawAtPoint_withAttributes(
                        NSPoint::new(x, middle - 1.0),
                        Some(&attributes),
                    );
                    bottom.drawAtPoint_withAttributes(
                        NSPoint::new(x, middle - bottom_size.height + 1.0),
                        Some(&attributes),
                    );
                }
                _ => {}
            }
            Bool::YES
        });
        NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw)
    }
}

/// Accessibility description and tooltip for the status item.
fn menu_bar_description(summary: StatusSummary) -> String {
    if summary.total == 0 {
        return "GitSync: no monitored repositories".to_owned();
    }
    let mut parts = Vec::new();
    match (summary.push, summary.pull) {
        (0, 0) => {}
        (push, 0) => parts.push(format!("{push} to push")),
        (0, pull) => parts.push(format!("{pull} to pull")),
        (push, pull) => parts.push(format!("{push} to push, {pull} to pull")),
    }
    if summary.problems > 0 {
        parts.push(format!(
            "{} {}",
            summary.problems,
            if summary.problems == 1 {
                "repository has a problem"
            } else {
                "repositories have problems"
            }
        ));
    }
    if summary.uncommitted > 0 {
        parts.push(format!("{} with uncommitted changes", summary.uncommitted));
    }
    if parts.is_empty() {
        parts.push(format!(
            "{} {} up to date",
            summary.total,
            if summary.total == 1 {
                "repository is"
            } else {
                "repositories are"
            }
        ));
    }
    format!("GitSync: {}", parts.join(" · "))
}

/// The top edge of `window` in AppKit screen coordinates.
pub(crate) fn window_top(window: &impl HasWindowHandle) -> Option<f64> {
    let native_window = native_window(window)?;
    let frame = native_window.frame();
    Some(frame.origin.y + frame.size.height)
}

/// Moves `window` so its top edge is at `top` again, after a resize that
/// kept its bottom edge fixed, and rebuilds its shadow for the new size.
/// Only the origin changes, so GPUI's own idea of the window size stays
/// correct.
pub(crate) fn restore_window_top(window: &impl HasWindowHandle, top: f64) {
    let Some(native_window) = native_window(window) else {
        return;
    };
    let frame = native_window.frame();
    native_window.setFrameTopLeftPoint(NSPoint::new(frame.origin.x, top));
    native_window.invalidateShadow();
}

fn native_window(window: &impl HasWindowHandle) -> Option<Retained<objc2_app_kit::NSWindow>> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // SAFETY: GPUI's AppKit handle points to the window's live content view,
    // and callers run on the main thread.
    unsafe { (*handle.ns_view.as_ptr().cast::<NSView>()).window() }
}

/// Rounds a borderless window's corners. Clipping the content view's layer
/// also clips GPUI's blur view (a subview of the content view), so the
/// vibrancy follows the rounded shape, and the shadow is rebuilt to match.
pub(crate) fn round_window_corners(window: &impl HasWindowHandle, radius: f64) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    // SAFETY: GPUI's AppKit handle points to the window's live content view,
    // this runs on the main thread, and `layer` is the CALayer that AppKit
    // creates for a layer-backed view.
    unsafe {
        let view = handle.ns_view.as_ptr().cast::<NSView>();
        let Some(native_window) = (*view).window() else {
            return;
        };
        let Some(content_view) = native_window.contentView() else {
            return;
        };
        content_view.setWantsLayer(true);
        let layer: *mut AnyObject = msg_send![&*content_view, layer];
        if layer.is_null() {
            return;
        }
        let _: () = msg_send![layer, setCornerRadius: radius];
        let _: () = msg_send![layer, setMasksToBounds: true];
        native_window.setHasShadow(true);
        native_window.invalidateShadow();
    }
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: HotKeyHandler,
        type_count: u32,
        types: *const EventTypeSpec,
        user_data: *mut c_void,
        handler_ref: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        hot_key_id: EventHotKeyId,
        target: *mut c_void,
        options: u32,
        hot_key_ref: *mut *mut c_void,
    ) -> i32;
}

type HotKeyHandler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

/// `kEventClassKeyboard` ('keyb') and `kEventHotKeyPressed`.
const EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const EVENT_HOT_KEY_PRESSED: u32 = 5;
/// `cmdKey` modifier and the virtual key code of G (`kVK_ANSI_G`).
const COMMAND_MODIFIER: u32 = 1 << 8;
const KEY_CODE_G: u32 = 0x05;

thread_local! {
    static HOT_KEY_ACTION: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
}

extern "C" fn hot_key_pressed(_: *mut c_void, _: *mut c_void, _: *mut c_void) -> i32 {
    HOT_KEY_ACTION.with(|action| {
        if let Some(action) = action.borrow_mut().as_mut() {
            action();
        }
    });
    0
}

/// Registers ⌘G as a system-wide shortcut that runs `action`. Carbon hot keys
/// work without Accessibility permission. While GitSync runs, ⌘G no longer
/// reaches other apps (where it often means "Find Next").
pub(crate) fn register_toggle_hot_key(action: impl FnMut() + 'static) -> Result<(), String> {
    HOT_KEY_ACTION.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
    let types = [EventTypeSpec {
        event_class: EVENT_CLASS_KEYBOARD,
        event_kind: EVENT_HOT_KEY_PRESSED,
    }];
    // SAFETY: Called once on the main thread. The handler is a plain
    // `extern "C"` function, `types` outlives the call, and Carbon keeps the
    // returned references for the life of the process.
    unsafe {
        let target = GetApplicationEventTarget();
        let mut handler_ref = std::ptr::null_mut();
        let status = InstallEventHandler(
            target,
            hot_key_pressed,
            types.len() as u32,
            types.as_ptr(),
            std::ptr::null_mut(),
            &mut handler_ref,
        );
        if status != 0 {
            return Err(format!(
                "Could not install the ⌘G handler (error {status})."
            ));
        }
        let mut hot_key_ref = std::ptr::null_mut();
        let status = RegisterEventHotKey(
            KEY_CODE_G,
            COMMAND_MODIFIER,
            EventHotKeyId {
                signature: u32::from_be_bytes(*b"GtSy"),
                id: 1,
            },
            target,
            0,
            &mut hot_key_ref,
        );
        if status != 0 {
            return Err(format!(
                "Could not register ⌘G (error {status}). Another app may already use it."
            ));
        }
    }
    Ok(())
}

/// Whether GitSync is the active app. Focus moving between GitSync's own
/// windows keeps it active.
pub(crate) fn application_is_active() -> bool {
    let mtm = MainThreadMarker::new().expect("application state must be read on the main thread");
    NSApplication::sharedApplication(mtm).isActive()
}

/// Hides GitSync so the app that was active before regains focus.
pub(crate) fn hide_application() {
    let mtm = MainThreadMarker::new().expect("application hiding must run on the main thread");
    NSApplication::sharedApplication(mtm).hide(None);
}

pub(crate) fn activate_application() {
    let mtm = MainThreadMarker::new().expect("application activation must run on the main thread");
    NSApplication::sharedApplication(mtm).activate();
}

/// Detail text for removing one repository.
pub(crate) const REMOVE_REPOSITORY_DETAIL: &str =
    "This removes only GitSync’s saved record and cached status. Repository files are not changed.";
/// Detail text for removing a remote computer.
pub(crate) const REMOVE_HOST_DETAIL: &str = "This stops monitoring every repository on this remote computer and removes GitSync’s saved records and cached statuses for them. Files on the remote computer are not changed.";
/// Detail text for resetting GitSync.
pub(crate) const RESET_DETAIL: &str = "This removes GitSync’s monitored-repository list, settings, and cached statuses, and turns off launch at login. Repository files are not changed.";

/// Asks whether to remove one repository, as an app-modal alert. It blocks,
/// so callers run it through `modal::confirm`.
pub(crate) fn confirm_remove(repository_name: &str) -> bool {
    let mtm = MainThreadMarker::new().expect("confirmation must run on the main thread");
    activate_application();
    let title = NSString::from_str(&format!("Remove {repository_name} from GitSync?"));
    let detail = NSString::from_str(REMOVE_REPOSITORY_DETAIL);

    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&title);
    alert.setInformativeText(&detail);
    // Cancel comes first: a modal session that ends early reports the
    // first button, and that must never remove anything.
    alert.addButtonWithTitle(ns_string!("Cancel"));
    alert
        .addButtonWithTitle(ns_string!("Remove"))
        .setHasDestructiveAction(true);
    alert.runModal() == NSAlertSecondButtonReturn
}

/// Asks once, after a scan, whether the new repositories may fetch. It
/// blocks, so it must not run while GPUI holds the app (see `modal`).
pub(crate) fn confirm_fetch_consent(count: usize) -> bool {
    let title = if count == 1 {
        "Allow GitSync to fetch the new repository?".to_owned()
    } else {
        format!("Allow GitSync to fetch the {count} new repositories?")
    };
    confirm_fetch(&title, "Not Now", "Allow")
}

/// Asks before fetch is allowed for every monitored repository that does not
/// have it yet.
pub(crate) fn confirm_fetch_for_all(count: usize) -> bool {
    let title = if count == 1 {
        "Allow GitSync to fetch 1 repository?".to_owned()
    } else {
        format!("Allow GitSync to fetch {count} repositories?")
    };
    confirm_fetch(&title, "Cancel", "Allow")
}

/// Returns true only for `allow`. `decline` comes first: a modal session that
/// ends early reports the first button, and that must never grant consent.
fn confirm_fetch(title: &str, decline: &str, allow: &str) -> bool {
    let mtm = MainThreadMarker::new().expect("confirmation must run on the main thread");
    activate_application();
    let title = NSString::from_str(title);
    let allow = NSString::from_str(allow);
    let decline = NSString::from_str(decline);

    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&title);
    alert.setInformativeText(ns_string!(
        "GitSync fetches only each branch’s upstream, with the repository’s configured remote and credentials. It never pushes, pulls, or changes files. You can allow fetch later for each repository."
    ));
    alert.addButtonWithTitle(&decline);
    alert.addButtonWithTitle(&allow);
    alert.runModal() == NSAlertSecondButtonReturn
}

pub(crate) fn reveal_in_finder(path: &Path) -> Result<(), String> {
    open_path(["-R"], path, "reveal the repository in Finder")
}

pub(crate) fn open_in_terminal(path: &Path) -> Result<(), String> {
    open_path(["-a", "Terminal"], path, "open the repository in Terminal")
}

/// Opens Terminal with an SSH session to `destination`, already in `path`.
/// A `.command` file is the documented way to run a command in a new
/// Terminal window without Apple Events permission.
pub(crate) fn open_ssh_terminal(destination: &str, path: &Path) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let remote_command = format!(
        "cd {} && exec \"$SHELL\" -l",
        shell_quote(&path.to_string_lossy())
    );
    let script = format!(
        "#!/bin/sh\nrm -f \"$0\"\nexec /usr/bin/ssh -t -- {} {}\n",
        shell_quote(destination),
        shell_quote(&remote_command)
    );
    let file = std::env::temp_dir().join(format!("gitsync-ssh-{}.command", uuid::Uuid::new_v4()));
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(&file)
        .and_then(|mut handle| handle.write_all(script.as_bytes()))
        .map_err(|error| format!("Could not open SSH in Terminal: {error}"))?;
    Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(&file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open SSH in Terminal: {error}"))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn open_data_folder(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("Could not create the GitSync data folder: {error}"))?;
    open_path([], path, "open the GitSync data folder")
}

pub(crate) fn reveal_diagnostics(path: &Path, data_folder: &Path) -> Result<(), String> {
    if !path.exists() {
        return open_data_folder(data_folder);
    }
    Command::new("/usr/bin/open")
        .arg("-R")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not reveal diagnostic logs: {error}"))
}

pub(crate) fn open_source_repository() -> Result<(), String> {
    open_external_url("https://github.com/hizkiajuan/gitsync")
}

pub(crate) fn open_releases() -> Result<(), String> {
    open_external_url("https://github.com/hizkiajuan/gitsync/releases")
}

/// Opens THIRD_PARTY_LICENSES.html from the app bundle, which `bundle.sh`
/// generates with cargo-about.
pub(crate) fn open_third_party_licenses() -> Result<(), String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("Could not locate GitSync: {error}"))?;
    let path = executable
        .parent()
        .and_then(Path::parent)
        .map(|contents| contents.join("Resources/THIRD_PARTY_LICENSES.html"))
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            "Third-party licenses are only included in a bundle built with cargo-about installed."
                .to_string()
        })?;
    Command::new("/usr/bin/open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open the third-party licenses: {error}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoginItemStatus {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
}

impl LoginItemStatus {
    pub(crate) fn is_registered(self) -> bool {
        matches!(self, Self::Enabled | Self::RequiresApproval)
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::NotRegistered => "Off",
            Self::Enabled => "On",
            Self::RequiresApproval => "Approval required in System Settings",
            Self::NotFound => "Unavailable for this app bundle",
        }
    }
}

pub(crate) fn launch_at_login_status() -> Result<LoginItemStatus, String> {
    let service = login_item_service()?;
    // SAFETY: `service` is an `SMAppService`, retained for this call, and
    // `status` returns the documented integer-backed `SMAppServiceStatus`.
    let status: isize = unsafe { msg_send![&*service, status] };
    match status {
        0 => Ok(LoginItemStatus::NotRegistered),
        1 => Ok(LoginItemStatus::Enabled),
        2 => Ok(LoginItemStatus::RequiresApproval),
        3 => Ok(LoginItemStatus::NotFound),
        value => Err(format!("Launch at login returned unknown status {value}.")),
    }
}

pub(crate) fn set_launch_at_login(enabled: bool) -> Result<LoginItemStatus, String> {
    let service = login_item_service()?;
    let current = launch_at_login_status()?;
    if current.is_registered() == enabled {
        return Ok(current);
    }

    let mut error: *mut NSError = std::ptr::null_mut();
    // SAFETY: `service` is a retained `SMAppService`. The selector is
    // available from macOS 13, GitSync's deployment floor. The out pointer is
    // valid for the duration of the synchronous call.
    let succeeded: Bool = unsafe {
        if enabled {
            msg_send![&*service, registerAndReturnError: &mut error]
        } else {
            msg_send![&*service, unregisterAndReturnError: &mut error]
        }
    };
    if !succeeded.as_bool() {
        let detail = unsafe { error.as_ref() }
            .map(|error| error.localizedDescription().to_string())
            .unwrap_or_else(|| "macOS did not provide an error description.".to_owned());
        return Err(format!(
            "Could not {} launch at login: {detail}",
            if enabled { "enable" } else { "disable" }
        ));
    }
    launch_at_login_status()
}

pub(crate) fn open_login_items_settings() -> Result<(), String> {
    let class = AnyClass::get(c"SMAppService")
        .ok_or_else(|| "Launch at login requires macOS 13 or later.".to_owned())?;
    // SAFETY: The class is present and implements the macOS 13
    // `openSystemSettingsLoginItems` class method.
    unsafe {
        let _: () = msg_send![class, openSystemSettingsLoginItems];
    }
    Ok(())
}

pub(crate) fn diagnostic_environment() -> String {
    let macos_version = command_output("/usr/bin/sw_vers", &["-productVersion"])
        .unwrap_or_else(|| "unknown".into());
    format!(
        "macOS: {macos_version}\nArchitecture: {}",
        std::env::consts::ARCH
    )
}

fn login_item_service() -> Result<Retained<NSObject>, String> {
    let class = AnyClass::get(c"SMAppService")
        .ok_or_else(|| "Launch at login requires macOS 13 or later.".to_owned())?;
    // SAFETY: The linked ServiceManagement class implements the macOS 13
    // `mainAppService` class property and returns an Objective-C object.
    Ok(unsafe { msg_send![class, mainAppService] })
}

fn command_output(executable: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(executable)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let output = String::from_utf8_lossy(&output.stdout);
    Some(output.trim().chars().take(120).collect())
}

fn open_external_url(url: &str) -> Result<(), String> {
    Command::new("/usr/bin/open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open the link: {error}"))
}

fn open_path<const N: usize>(
    arguments: [&str; N],
    path: &Path,
    action: &str,
) -> Result<(), String> {
    if !path.is_dir() {
        return Err(format!(
            "Could not {action}: {} is no longer an accessible directory.",
            path.display()
        ));
    }
    Command::new("/usr/bin/open")
        .args(arguments)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not {action}: {error}"))
}

fn status_item_anchor(
    button: &NSStatusBarButton,
    mtm: MainThreadMarker,
) -> Option<StatusItemAnchor> {
    let status_window = button.window()?;
    let status_frame = status_window.frame();
    // AppKit's global coordinates are anchored to the primary screen (the
    // first one, with the menu bar), not to `mainScreen`, which is whichever
    // screen holds the key window.
    let primary_screen = NSScreen::screens(mtm).firstObject()?;
    let primary_top = primary_screen.frame().origin.y + primary_screen.frame().size.height;

    Some(StatusItemAnchor {
        center_x: status_frame.origin.x + status_frame.size.width / 2.0,
        bottom_y: primary_top - status_frame.origin.y,
    })
}

#[cfg(test)]
mod tests {
    use super::{StatusSummary, menu_bar_description, menu_bar_lines, shell_quote};

    #[test]
    fn menu_bar_lines_put_pull_over_push_and_skip_zeros() {
        assert!(menu_bar_lines(StatusSummary::default()).is_empty());
        assert_eq!(
            menu_bar_lines(StatusSummary {
                push: 3,
                pull: 1,
                ..StatusSummary::default()
            }),
            ["↓1", "↑3"]
        );
        assert_eq!(
            menu_bar_lines(StatusSummary {
                push: 4,
                ..StatusSummary::default()
            }),
            ["↑4"]
        );
    }

    #[test]
    fn menu_bar_description_summarizes_push_pull_and_problems() {
        assert_eq!(
            menu_bar_description(StatusSummary::default()),
            "GitSync: no monitored repositories"
        );
        assert_eq!(
            menu_bar_description(StatusSummary {
                total: 4,
                push: 3,
                pull: 1,
                ..StatusSummary::default()
            }),
            "GitSync: 3 to push, 1 to pull"
        );
        assert_eq!(
            menu_bar_description(StatusSummary {
                total: 2,
                problems: 1,
                ..StatusSummary::default()
            }),
            "GitSync: 1 repository has a problem"
        );
        assert_eq!(
            menu_bar_description(StatusSummary {
                total: 1,
                ..StatusSummary::default()
            }),
            "GitSync: 1 repository is up to date"
        );
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }
}
