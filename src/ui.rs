//! GitSync's type scale, brand mark, and bundled assets.
//!
//! Every window sets text with these roles instead of GPUI's rem sizes, so
//! the panel, Settings, and the SSH window share one hierarchy. Sizes follow
//! the macOS text styles (13 pt body, 11 pt captions), with tighter line
//! heights than GPUI's default golden-ratio leading.

use std::borrow::Cow;

use gpui_kit::{AssetSource, FontWeight, Hsla, IntoElement, Pixels, SharedString, Styled, px, svg};

/// The rem size. GPUI Kit sizes controls in rems, and 14 px gives buttons,
/// inputs, and tabs about the height of native small controls.
pub(crate) const REM_SIZE: f32 = 14.0;

/// The brand mark, drawn in one color. `resources/icon/mark.svg` is the
/// source of the app icon too.
pub(crate) const MARK_SVG: &str = "gitsync/mark.svg";
const MARK_SVG_BYTES: &[u8] = include_bytes!("../resources/icon/mark.svg");

/// Text roles, largest first.
pub(crate) trait Typography: Styled + Sized {
    /// Window and panel titles.
    fn text_title(self) -> Self {
        self.text_size(px(17.0))
            .line_height(px(22.0))
            .font_weight(FontWeight::BOLD)
    }

    /// Group headings, such as a Settings section or a panel section.
    fn text_heading(self) -> Self {
        self.text_size(px(13.0))
            .line_height(px(18.0))
            .font_weight(FontWeight::SEMIBOLD)
    }

    /// The name of a row: a setting or a repository.
    fn text_row_title(self) -> Self {
        self.text_size(px(13.0))
            .line_height(px(18.0))
            .font_weight(FontWeight::MEDIUM)
    }

    /// Running text and values.
    fn text_body(self) -> Self {
        self.text_size(px(13.0))
            .line_height(px(18.0))
            .font_weight(FontWeight::NORMAL)
    }

    /// Secondary lines under a title: descriptions, branches, paths.
    fn text_secondary(self) -> Self {
        self.text_size(px(11.5))
            .line_height(px(15.0))
            .font_weight(FontWeight::NORMAL)
    }

    /// Small labels, counts, and footers.
    fn text_caption(self) -> Self {
        self.text_size(px(11.0))
            .line_height(px(14.0))
            .font_weight(FontWeight::NORMAL)
    }

    /// Short all-caps labels above a group.
    fn text_eyebrow(self) -> Self {
        self.text_size(px(10.5))
            .line_height(px(14.0))
            .font_weight(FontWeight::SEMIBOLD)
    }
}

impl<T: Styled> Typography for T {}

/// The GitSync mark at `size`, tinted with `color`.
pub(crate) fn mark(size: Pixels, color: Hsla) -> impl IntoElement {
    svg()
        .path(MARK_SVG)
        .flex_none()
        .size(size)
        .text_color(color)
}

/// GPUI Kit's icons plus GitSync's own.
pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if path == MARK_SVG {
            return Ok(Some(Cow::Borrowed(MARK_SVG_BYTES)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut list = gpui_kit::assets::Assets.list(path)?;
        if MARK_SVG.starts_with(path) {
            list.push(MARK_SVG.into());
        }
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::AssetSource as _;

    use super::{Assets, MARK_SVG};

    #[test]
    fn assets_serve_the_mark_and_the_kit_icons() {
        let mark = Assets.load(MARK_SVG).unwrap().expect("the mark is bundled");
        assert!(std::str::from_utf8(&mark).unwrap().contains("<svg"));
        assert!(Assets.load("icons/check.svg").unwrap().is_some());
        assert!(
            Assets
                .list("gitsync/")
                .unwrap()
                .iter()
                .any(|path| path == MARK_SVG)
        );
    }
}
