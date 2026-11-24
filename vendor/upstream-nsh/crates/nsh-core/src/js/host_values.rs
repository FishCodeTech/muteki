//! The host prop values that are objects or unions (sources, accessibility
//! descriptions, list layouts) and the props whose whole variant is
//! decoded by hand (`#[prop(with = …)]`): the shapes JS sends them in.

use super::host::{HostCx, HostInput, HostList, HostValue, field};
use crate::{
    AccessibilityCurrent, AccessibilityProp, AccessibilityRole, AccessibilityState, AutoFill,
    CellSlot, EdgeInsets, ImageScales, ImageSource, KeyboardTaps, ListContentStyle, ModalProp,
    Prop, ScrollProp, StatusBarSetting, StatusBarStyle, Viewability,
    element::TsTypes,
    geometry::{Bounds, Percentage, Points, point, size},
    mount::ModalConfig,
    ops::OverlayProp,
    overlay::{OverlayAnchor, TooltipContent},
    props::{AccessibilityActionDef, AccessibilityValue, Checked, PanActivation, Ripple},
};

fn number<I: HostInput>(input: &I, key: &str) -> Option<f32> {
    input
        .field(key)
        .and_then(|v| v.as_number())
        .map(|n| n as f32)
}

/// Field `key` of `input` as a number of points.
fn points<I: HostInput>(input: &I, key: &str) -> Option<Points> {
    number(input, key).map(Points)
}

/// The `scales` of an image asset (its first [`crate::ops::SCALES_READ`]
/// entries); values that are not [`crate::IMAGE_SCALES`] are left out.
fn scales<I: HostInput>(input: &I) -> ImageScales {
    let Some(list) = input.field("scales").and_then(|s| s.as_list()) else {
        return ImageScales::NONE;
    };
    (0..list.len().min(crate::ops::SCALES_READ))
        .filter_map(|i| list.item(i)?.as_number())
        .map(|n| n as f32)
        .collect()
}

/// `"https://…"` or a bundled asset `{ url, size: { width, height }, scales }`.
impl HostValue for ImageSource {
    fn decode<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Self> {
        if let Some(url) = input.as_string() {
            return (!url.is_empty()).then(|| ImageSource {
                uri: url.into(),
                size: None,
                scales: ImageScales::NONE,
                data: None,
            });
        }
        let url = input.field("url")?.as_string()?;
        let extent = input.field("size");
        let size = match extent
            .as_ref()
            .map(|e| (number(e, "width"), number(e, "height")))
        {
            Some((Some(w), Some(h))) => Some(size(Points(w), Points(h))),
            _ => None,
        };
        Some(ImageSource {
            uri: url.into(),
            size,
            scales: scales(input),
            data: None,
        })
    }

    fn ts_type(_: &mut TsTypes) -> String {
        "string | ImageAsset".into()
    }
}

/// A list's `contentStyle`: the padding properties (numbers) and
/// `gap` / `rowGap` / `columnGap`.
impl HostValue for ListContentStyle {
    fn decode<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Self> {
        let n = |key| points(input, key);
        let all = n("padding").unwrap_or_default();
        let inline = n("paddingHorizontal").unwrap_or(all);
        let block = n("paddingVertical").unwrap_or(all);
        let gap = n("gap").unwrap_or_default();
        Some(ListContentStyle {
            padding: EdgeInsets {
                top: n("paddingTop").unwrap_or(block),
                right: n("paddingRight").unwrap_or(inline),
                bottom: n("paddingBottom").unwrap_or(block),
                left: n("paddingLeft").unwrap_or(inline),
            },
            row_gap: n("rowGap").unwrap_or(gap),
            column_gap: n("columnGap").unwrap_or(gap),
        })
    }

    fn ts_type(types: &mut TsTypes) -> String {
        types.declare("ListContentStyle", |_| {
            "/** Padding and gaps of a list's content (`contentStyle`). */\n\
             export interface ListContentStyle {\n  padding?: number;\n  paddingHorizontal?: number;\n  \
             paddingVertical?: number;\n  paddingTop?: number;\n  paddingRight?: number;\n  \
             paddingBottom?: number;\n  paddingLeft?: number;\n  gap?: number;\n  rowGap?: number;\n  \
             columnGap?: number;\n}"
                .into()
        })
    }
}

/// A page's or dialog's `statusBar`: `{ foreground, hidden, animated }`
/// (`StatusBar`, declared with the navigation in `nsh/base`); the fields
/// left out keep what is below. An unknown `foreground` is left out.
impl HostValue for StatusBarSetting {
    fn decode<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Self> {
        if !input.is_object() {
            return None;
        }
        let flag = |key| input.field(key).and_then(|v| v.as_bool());
        Some(StatusBarSetting {
            style: input
                .field("foreground")
                .and_then(|v| v.as_string())
                .and_then(|s| StatusBarStyle::from_name(&s)),
            hidden: flag("hidden"),
            animated: flag("animated"),
        })
    }

    fn ts_type(_: &mut TsTypes) -> String {
        "StatusBar".into()
    }
}

impl HostValue for Viewability {
    fn decode<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Self> {
        Some(Viewability {
            view_area_coverage_threshold: number(input, "viewAreaCoverageThreshold")
                .map(Percentage),
            item_visible_threshold: number(input, "itemVisibleThreshold").map(Percentage),
        })
    }

    fn ts_type(_: &mut TsTypes) -> String {
        "ViewabilityConfig".into()
    }
}

/// `{ activeOffsetX, activeOffsetY, failOffsetX, failOffsetY }` (points;
/// the sign is ignored).
impl HostValue for PanActivation {
    fn decode<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Self> {
        if !input.is_object() {
            return None;
        }
        let offset = |key| points(input, key).map(Points::abs);
        Some(PanActivation {
            active_offset_x: offset("activeOffsetX"),
            active_offset_y: offset("activeOffsetY"),
            fail_offset_x: offset("failOffsetX"),
            fail_offset_y: offset("failOffsetY"),
        })
    }

    fn ts_type(_: &mut TsTypes) -> String {
        "PanActivation".into()
    }
}

/// `{ disabled, selected, checked: true | false | "mixed", busy, expanded,
/// pressed: true | false | "mixed", current: true | false | "page" | ... }`.
impl HostValue for AccessibilityState {
    fn decode<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Self> {
        let flag = |key| input.field(key).and_then(|v| v.as_bool()).unwrap_or(false);
        let tristate = |key| {
            let value = input.field(key);
            match value.as_ref().and_then(|c| c.as_bool()) {
                Some(true) => Some(Checked::True),
                Some(false) => Some(Checked::False),
                None => value
                    .and_then(|c| c.as_string())
                    .filter(|s| s == "mixed")
                    .map(|_| Checked::Mixed),
            }
        };
        let current = input.field("current");
        Some(AccessibilityState {
            disabled: flag("disabled"),
            selected: flag("selected"),
            checked: tristate("checked"),
            busy: flag("busy"),
            expanded: input.field("expanded").and_then(|v| v.as_bool()),
            pressed: tristate("pressed"),
            // `false` and unknown words are no current item, as ARIA has it.
            current: match current.as_ref().and_then(|c| c.as_bool()) {
                Some(true) => Some(AccessibilityCurrent::True),
                Some(false) => None,
                None => match current.and_then(|c| c.as_string()).as_deref() {
                    Some("page") => Some(AccessibilityCurrent::Page),
                    Some("step") => Some(AccessibilityCurrent::Step),
                    Some("location") => Some(AccessibilityCurrent::Location),
                    Some("date") => Some(AccessibilityCurrent::Date),
                    Some("time") => Some(AccessibilityCurrent::Time),
                    Some("true") => Some(AccessibilityCurrent::True),
                    _ => None,
                },
            },
        })
    }

    fn ts_type(types: &mut TsTypes) -> String {
        types.declare("AccessibilityState", |_| {
            "/** The state assistive technologies report (`accessibilityState`). */\n\
             export interface AccessibilityState {\n  disabled?: boolean;\n  selected?: boolean;\n  \
             checked?: boolean | \"mixed\";\n  busy?: boolean;\n  expanded?: boolean;\n  \
             /** A toggle button that is on or off (ARIA's `aria-pressed`). */\n  \
             pressed?: boolean | \"mixed\";\n  \
             /** The current item of a set (ARIA's `aria-current`): a pagination's page, a wizard's step. */\n  \
             current?: boolean | \"page\" | \"step\" | \"location\" | \"date\" | \"time\";\n}"
                .into()
        })
    }
}

/// `{ min, max, now, text }`.
impl HostValue for AccessibilityValue {
    fn decode<I: HostInput>(input: &I, cx: &mut HostCx) -> Option<Self> {
        if !input.is_object() {
            return None;
        }
        Some(AccessibilityValue {
            min: number(input, "min"),
            max: number(input, "max"),
            now: number(input, "now"),
            text: field(input, "text", cx),
        })
    }

    fn ts_type(_: &mut TsTypes) -> String {
        "{ min?: number; max?: number; now?: number; text?: string }".into()
    }
}

/// `{ name, label }`; without a name there is no action.
impl HostValue for AccessibilityActionDef {
    fn decode<I: HostInput>(input: &I, cx: &mut HostCx) -> Option<Self> {
        Some(AccessibilityActionDef {
            name: field(input, "name", cx)?,
            label: field(input, "label", cx),
        })
    }

    fn ts_type(_: &mut TsTypes) -> String {
        "{ name: string; label?: string }".into()
    }
}

/// The `autoComplete` names of the TypeScript type ([`AutoFill::parse`]
/// takes a few more).
const AUTO_COMPLETE: &[&str] = &[
    "off",
    "username",
    "password",
    "current-password",
    "new-password",
    "one-time-code",
    "sms-otp",
    "email",
    "tel",
    "name",
    "given-name",
    "family-name",
    "additional-name",
    "nickname",
    "organization",
    "street-address",
    "address-line1",
    "address-line2",
    "address-level1",
    "address-level2",
    "postal-code",
    "country",
    "cc-number",
    "cc-exp",
    "cc-csc",
    "cc-name",
    "url",
    "birthdate-full",
];

/// iOS's `textContentType` names.
const TEXT_CONTENT_TYPE: &[&str] = &[
    "none",
    "username",
    "password",
    "newPassword",
    "oneTimeCode",
    "emailAddress",
    "telephoneNumber",
    "name",
    "givenName",
    "middleName",
    "familyName",
    "nickname",
    "organizationName",
    "fullStreetAddress",
    "streetAddressLine1",
    "streetAddressLine2",
    "addressCity",
    "addressState",
    "postalCode",
    "countryName",
    "creditCardNumber",
    "creditCardExpiration",
    "creditCardSecurityCode",
    "creditCardName",
    "URL",
    "birthdate",
];

/// `autoComplete` values or iOS's `textContentType` names; others are the
/// platform's default (and a warning).
impl HostValue for AutoFill {
    fn decode<I: HostInput>(input: &I, cx: &mut HostCx) -> Option<Self> {
        let name = input.as_string()?;
        let fill = AutoFill::parse(&name);
        if fill.is_none() {
            cx.warn(format!(
                "{name:?} is not a supported autoComplete / textContentType value"
            ));
        }
        fill
    }

    /// Either list of names: the decoder takes both.
    fn ts_type(types: &mut TsTypes) -> String {
        let union = |names: &[&str]| {
            names
                .iter()
                .map(|n| format!("{n:?}"))
                .collect::<Vec<_>>()
                .join(" | ")
        };
        let web = types.declare("AutoComplete", |_| {
            format!(
                "/** What an input holds, for autofill (React Native's `autoComplete` values). */\n\
                 export type AutoComplete = {};",
                union(AUTO_COMPLETE)
            )
        });
        let ios = types.declare("TextContentType", |_| {
            format!(
                "/** iOS's names for `autoComplete` values. */\nexport type TextContentType = {};",
                union(TEXT_CONTENT_TYPE)
            )
        });
        format!("{web} | {ios}")
    }
}

/// `{ color, borderless, radius, foreground }`.
impl HostValue for Ripple {
    fn decode<I: HostInput>(input: &I, cx: &mut HostCx) -> Option<Self> {
        if !input.is_object() {
            return None;
        }
        let flag = |key| input.field(key).and_then(|v| v.as_bool()).unwrap_or(false);
        Some(Ripple {
            color: field(input, "color", cx),
            borderless: flag("borderless"),
            radius: points(input, "radius"),
            foreground: flag("foreground"),
        })
    }

    fn ts_type(_: &mut TsTypes) -> String {
        "{ color?: ColorValue; borderless?: boolean; radius?: number; foreground?: boolean }".into()
    }
}

// --- Whole variants ---------------------------------------------------------

/// `slot`: a recycling slot (a number) or a role.
pub(crate) fn slot<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::Slot(match input.as_string() {
        Some(role) => match role.as_str() {
            "header" => CellSlot::Header,
            "footer" => CellSlot::Footer,
            _ => CellSlot::Empty,
        },
        None => CellSlot::Item(input.as_number().unwrap_or(0.0) as u32),
    }))
}

/// `splice`: `[at, removed, inserted]`.
pub(crate) fn splice<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    let items = input.items();
    let at = |i: usize| {
        items
            .get(i)
            .and_then(|v| v.as_number())
            .unwrap_or(0.0)
            .max(0.0) as u32
    };
    Some(Prop::Splice(at(0), at(1), at(2)))
}

/// `stickyHeaderIndices`: the indexes, negative ones skipped.
pub(crate) fn sticky<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::StickyIndices(
        input
            .items()
            .iter()
            .filter_map(|v| v.as_number())
            .filter(|v| *v >= 0.0)
            .map(|v| v as u32)
            .collect(),
    ))
}

/// `selection`: `{ start, end }` (a caret without `end`); nothing without
/// `start`.
pub(crate) fn selection<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    let field = |key| input.field(key).and_then(|v| v.as_number());
    let start = field("start")?.max(0.0) as u32;
    Some(Prop::Selection(
        start,
        field("end").map_or(start, |e| e.max(0.0) as u32),
    ))
}

/// `pan`: `{ x, y }` animated values.
pub(crate) fn pan<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    let axis = |key| input.field(key).and_then(|v| v.animated_node());
    Some(Prop::PanValues(axis("x"), axis("y")))
}

/// `pinch`: `{ scale }`.
pub(crate) fn pinch<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::PinchValue(
        input.field("scale").and_then(|v| v.animated_node()),
    ))
}

/// `anchor`: a host element (a ref), `{ x, y }`, `{ origin, size }`, or
/// nothing.
pub(crate) fn overlay_anchor<I: HostInput>(input: &I, _: &mut HostCx) -> Option<OverlayProp> {
    let bounds = || {
        let origin = input.field("origin")?;
        let extent = input.field("size")?;
        Some(Bounds {
            origin: point(points(&origin, "x")?, points(&origin, "y")?),
            size: size(
                points(&extent, "width")?.max(Points::ZERO),
                points(&extent, "height")?.max(Points::ZERO),
            ),
        })
    };
    let anchor = match (input.node(), points(input, "x"), points(input, "y")) {
        (Some(id), ..) => OverlayAnchor::Node(id),
        (None, Some(x), Some(y)) => OverlayAnchor::Point(point(x, y)),
        _ => bounds().map_or(OverlayAnchor::None, OverlayAnchor::Bounds),
    };
    Some(OverlayProp::Anchor(anchor))
}

/// `tooltip`: an element (a node JS made), or nothing.
pub(crate) fn tooltip<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    let content = input
        .node()
        .map_or(TooltipContent::None, TooltipContent::Node);
    Some(Prop::Tooltip(content))
}

/// `rotation`: `{ angle }`.
pub(crate) fn rotation<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::RotationValue(
        input.field("angle").and_then(|v| v.animated_node()),
    ))
}

/// `scrollValue`: an animated value.
pub(crate) fn scroll_value<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::ScrollValue(input.animated_node()))
}

/// `pullValue`: an animated value.
pub(crate) fn pull_value<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::PullValue(input.animated_node()))
}

/// `stickyHeaderOffset`: a number, or an animated value.
pub(crate) fn sticky_offset<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    match input.as_number() {
        Some(n) => Some(Prop::StickyOffset(Points(n as f32))),
        None => input
            .animated_node()
            .map(|node| Prop::StickyOffsetNode(Some(node))),
    }
}

/// `hitSlop`: one number for every edge, or the edges.
pub(crate) fn hit_slop<I: HostInput>(input: &I, cx: &mut HostCx) -> Option<Prop> {
    Some(Prop::HitSlop(match input.as_number() {
        Some(n) => EdgeInsets::all(Points(n as f32)),
        None => EdgeInsets::decode(input, cx).unwrap_or_default(),
    }))
}

/// `classList`: the names whose value is truthy.
pub(crate) fn class_list<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::ClassList(
        input
            .entries()
            .into_iter()
            .filter(|(_, on)| on.truthy())
            .map(|(name, _)| name.into())
            .collect(),
    ))
}

/// `tailwindTheme`: JSON; nothing without it.
pub(crate) fn tailwind_theme<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::TailwindTheme(input.as_string()?.into()))
}

/// `uiTheme`: JSON; nothing without it.
pub(crate) fn ui_theme<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::UiTheme(input.as_string()?.into()))
}

/// `role`: one of the role names; others are no role (and a
/// warning).
pub(crate) fn role<I: HostInput>(input: &I, cx: &mut HostCx) -> Option<AccessibilityProp> {
    let role = AccessibilityRole::decode(input, cx);
    if role.is_none()
        && let Some(name) = input.as_string().filter(|n| !n.is_empty())
    {
        cx.warn(format!("unknown role `{name}`"));
    }
    Some(AccessibilityProp::Role(role.unwrap_or_default()))
}

/// `accessibilityLevel`: a whole number from 1; anything else is none.
pub(crate) fn accessibility_level<I: HostInput>(
    input: &I,
    _: &mut HostCx,
) -> Option<AccessibilityProp> {
    let level = input
        .as_number()
        .filter(|n| *n >= 1.0 && n.fract() == 0.0 && *n <= f64::from(u32::MAX))
        .map(|n| n as u32);
    Some(AccessibilityProp::Level(level))
}

/// `importantForAccessibility`.
pub(crate) fn important<I: HostInput>(input: &I, _: &mut HostCx) -> Option<AccessibilityProp> {
    Some(match input.as_string().as_deref() {
        Some("yes") => AccessibilityProp::Important(Some(true), false),
        Some("no") => AccessibilityProp::Important(Some(false), false),
        Some("no-hide-descendants") => AccessibilityProp::Important(Some(false), true),
        _ => AccessibilityProp::Important(None, false),
    })
}

/// `decelerationRate`: `"fast"`, `"normal"` (React Native's iOS rates:
/// velocity kept per millisecond) or the rate.
pub(crate) fn deceleration<I: HostInput>(input: &I, _: &mut HostCx) -> Option<ScrollProp> {
    Some(ScrollProp::Deceleration(match input.as_string() {
        Some(s) => match s.as_str() {
            "fast" => 0.99,
            "normal" => 0.998,
            _ => 0.0,
        },
        None => input.as_number().unwrap_or(0.0) as f32,
    }))
}

/// `maintainVisibleContentPosition`: `{ autoscrollToBottomThreshold }`.
pub(crate) fn visible_content<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::FollowEnd(
        points(input, "autoscrollToBottomThreshold").map(|t| t.max(Points::ZERO)),
    ))
}

/// `refreshIndicator`: `{ height }`.
pub(crate) fn refresh_indicator<I: HostInput>(input: &I, _: &mut HostCx) -> Option<ScrollProp> {
    Some(ScrollProp::RefreshIndicator(
        points(input, "height").map(|h| h.max(Points::ZERO)),
    ))
}

/// `contentOffset`: `{ x, y }`.
pub(crate) fn content_offset<I: HostInput>(input: &I, _: &mut HostCx) -> Option<ScrollProp> {
    let n = |key| points(input, key).unwrap_or_default();
    Some(ScrollProp::ContentOffset(point(n("x"), n("y"))))
}

/// `scrollbar`: a boolean, or the object of a theme (kept as JSON for the
/// set that draws the built-in bar).
pub(crate) fn scrollbar<I: HostInput>(input: &I, _: &mut HostCx) -> Option<Prop> {
    Some(Prop::Scrollbar(match input.as_bool() {
        Some(shows) => shows.into(),
        None if input.is_object() => {
            crate::ScrollbarSetting::Themed(std::sync::Arc::new(json_of(input)))
        }
        None => crate::ScrollbarSetting::Shown,
    }))
}

/// `input` as JSON: what is neither a boolean, number, string, array nor
/// object (a function) is `null`.
fn json_of<I: HostInput>(input: &I) -> crate::JsonValue {
    use crate::JsonValue;
    if let Some(b) = input.as_bool() {
        return JsonValue::Bool(b);
    }
    if let Some(n) = input.as_number() {
        return serde_json::Number::from_f64(n).map_or(JsonValue::Null, JsonValue::Number);
    }
    if let Some(s) = input.as_string() {
        return JsonValue::String(s);
    }
    if input.as_list().is_some() {
        return JsonValue::Array(input.items().iter().map(json_of).collect());
    }
    if input.is_object() {
        return JsonValue::Object(
            input
                .entries()
                .into_iter()
                .map(|(k, v)| (k, json_of(&v)))
                .collect(),
        );
    }
    JsonValue::Null
}

/// `keyboardShouldPersistTaps`: a boolean or `"never"` / `"always"` /
/// `"handled"`.
pub(crate) fn keyboard_taps<I: HostInput>(input: &I, _: &mut HostCx) -> Option<ScrollProp> {
    Some(ScrollProp::KeyboardTaps(match input.as_bool() {
        Some(true) => KeyboardTaps::Always,
        Some(false) => KeyboardTaps::Never,
        None => match input.as_string().as_deref() {
            Some("always") => KeyboardTaps::Always,
            Some("handled") => KeyboardTaps::Handled,
            _ => KeyboardTaps::Never,
        },
    }))
}

/// `supportedOrientations`: the orientation names.
pub(crate) fn orientations<I: HostInput>(input: &I, _: &mut HostCx) -> Option<ModalProp> {
    let mut bits = 0;
    for name in input.items() {
        bits |= match name.as_string().as_deref() {
            Some("portrait") => ModalConfig::PORTRAIT,
            Some("portrait-upside-down") => ModalConfig::PORTRAIT_UPSIDE_DOWN,
            Some("landscape") => ModalConfig::LANDSCAPE_LEFT | ModalConfig::LANDSCAPE_RIGHT,
            Some("landscape-left") => ModalConfig::LANDSCAPE_LEFT,
            Some("landscape-right") => ModalConfig::LANDSCAPE_RIGHT,
            _ => 0,
        };
    }
    Some(ModalProp::Orientations(bits))
}

/// A shader's `uniforms`: `{ name: value }`, a value a number, a CSS color,
/// an animated value or a list of them; what is none of them is left out
/// with a warning.
impl HostValue for crate::shader::ShaderUniforms {
    fn decode<I: HostInput>(input: &I, cx: &mut HostCx) -> Option<Self> {
        if !input.is_object() {
            return None;
        }
        let mut values = Vec::new();
        for (name, value) in input.entries() {
            match uniform_value(&value, 0) {
                Some(v) => values.push((name.as_str().into(), v)),
                None => cx.warn(format!(
                    "uniform `{name}`: a number, a color, an animated value or a list of them"
                )),
            }
        }
        Some(crate::shader::ShaderUniforms(values))
    }

    fn ts_type(types: &mut TsTypes) -> String {
        let value = types.declare("ShaderUniform", |_| {
            "/** A value of a shader's uniform: a number (`f32`, `i32`, `u32`), a color\n \
             * (`vec3f` / `vec4f`, sRGB 0..1), an animated value, or a list of them (a\n \
             * vector's components, an `array<vec4f, N>`'s items). */\n\
             export type ShaderUniform = number | ColorValue | AnimatedNode | readonly ShaderUniform[];"
                .into()
        });
        format!("Record<string, {value}>")
    }
}

fn uniform_value<I: HostInput>(input: &I, depth: u32) -> Option<crate::shader::UniformValue> {
    use crate::shader::UniformValue;
    if let Some(n) = input.as_number() {
        return Some(UniformValue::Number(n as f32));
    }
    if let Some(s) = input.as_string() {
        return crate::Color::parse(&s).map(UniformValue::Color);
    }
    if let Some(node) = input.animated_node() {
        return Some(UniformValue::Animated(node));
    }
    let list = input.as_list()?;
    // Vectors in arrays: two levels.
    if depth > 1 {
        return None;
    }
    (0..list.len())
        .map(|i| {
            list.item(i)
                .and_then(|item| uniform_value(&item, depth + 1))
        })
        .collect::<Option<Vec<_>>>()
        .map(UniformValue::List)
}
