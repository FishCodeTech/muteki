use nsh_core::element::FileAsset;
use nsh_core::prelude::*;
pub const LOGO: FileAsset = asset!("logo.svg");
pub const CHAT: FileAsset = asset!("chat.svg");
pub const PLUS: FileAsset = asset!("plus.svg");
pub const SEARCH: FileAsset = asset!("search.svg");
pub const USAGE: FileAsset = asset!("usage.svg");
pub const SETTINGS: FileAsset = asset!("settings.svg");
pub const ARCHIVE: FileAsset = asset!("archive.svg");
pub const ARROW: FileAsset = asset!("arrow.svg");
pub const PANEL: FileAsset = asset!("panel.svg");
pub const STOP: FileAsset = asset!("stop.svg");
pub const REFRESH: FileAsset = asset!("refresh.svg");
pub fn icon(asset: FileAsset) -> impl IntoElement {
    image(asset.uri).size(px(18.0))
}
