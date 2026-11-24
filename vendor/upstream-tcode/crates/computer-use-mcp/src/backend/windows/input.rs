use uiautomation::inputs::{Keyboard, Mouse, MouseButton as UiaMouseButton};
use uiautomation::types::Point;

use super::super::{BackendError, BackendErrorCode, MouseButton, parse_key_chord};
use super::map::key_expression_for_chord;

pub(super) fn click(
    x: f64,
    y: f64,
    button: MouseButton,
    click_count: u32,
) -> Result<(), BackendError> {
    if !(1..=3).contains(&click_count) {
        return Err(invalid("click_count must be between 1 and 3"));
    }
    move_mouse(x, y)?;
    let mouse = Mouse::new().auto_move(false).move_time(0).interval(45);
    let button = mouse_button(button);
    for _ in 0..click_count {
        mouse
            .click_button(button)
            .map_err(|error| operation(format!("uiautomation mouse click failed: {error}")))?;
    }
    Ok(())
}

pub(super) fn move_mouse(x: f64, y: f64) -> Result<(), BackendError> {
    let point = input_point(x, y, "mouse coordinates")?;
    Mouse::set_cursor_pos(&point)
        .map_err(|error| operation(format!("uiautomation mouse move failed: {error}")))
}

pub(super) fn drag(path: &[[f64; 2]], button: MouseButton) -> Result<(), BackendError> {
    if path.len() < 2 {
        return Err(invalid("drag requires at least two path points"));
    }
    if path
        .iter()
        .flatten()
        .any(|coordinate| !coordinate.is_finite())
    {
        return Err(invalid("drag path coordinates must be finite"));
    }
    let first = path
        .first()
        .ok_or_else(|| invalid("drag requires at least two path points"))?;
    let last = path
        .last()
        .ok_or_else(|| invalid("drag requires at least two path points"))?;
    let first = input_point(first[0], first[1], "drag path coordinates")?;
    let last = input_point(last[0], last[1], "drag path coordinates")?;

    Mouse::set_cursor_pos(&first)
        .map_err(|error| operation(format!("uiautomation drag positioning failed: {error}")))?;
    Mouse::new()
        .interval(12)
        .move_time((path.len() as u64).saturating_mul(12))
        .drag_to(mouse_button(button), &last)
        .map_err(|error| operation(format!("uiautomation mouse drag failed: {error}")))
}

pub(super) fn keypress(keys: &[String]) -> Result<(), BackendError> {
    let chord = parse_key_chord(keys).map_err(invalid)?;
    let expression = key_expression_for_chord(chord).map_err(invalid)?;
    Keyboard::new()
        .interval(0)
        .send_keys(&expression)
        .map_err(|error| operation(format!("uiautomation keyboard chord failed: {error}")))
}

pub(super) fn type_text(text: &str) -> Result<(), BackendError> {
    Keyboard::new()
        .interval(0)
        .send_text(text)
        .map_err(|error| operation(format!("uiautomation Unicode text input failed: {error}")))
}

pub(super) fn scroll_with_keyboard(x: f64, y: f64) -> Result<(), BackendError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(invalid("scroll deltas must be finite"));
    }
    let mut expression = String::new();
    if y != 0.0 {
        let key = if y > 0.0 { "page_up" } else { "page_down" };
        expression.push_str(&repeated_key(key, y));
    }
    if x != 0.0 {
        let key = if x > 0.0 { "right" } else { "left" };
        expression.push_str(&repeated_key(key, x));
    }
    if expression.is_empty() {
        return Ok(());
    }
    Keyboard::new()
        .interval(0)
        .send_keys(&expression)
        .map_err(|error| {
            operation(format!(
                "uiautomation keyboard scroll fallback failed: {error}"
            ))
        })
}

fn repeated_key(name: &str, delta: f64) -> String {
    let repetitions = ((delta.abs() / 120.0).ceil() as u32).clamp(1, 10);
    if repetitions == 1 {
        format!("{{{name}}}")
    } else {
        format!("{{{name} {repetitions}}}")
    }
}

fn input_point(x: f64, y: f64, description: &str) -> Result<Point, BackendError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(invalid(format!("{description} must be finite")));
    }
    let x = x.round();
    let y = y.round();
    if x < f64::from(i32::MIN)
        || x > f64::from(i32::MAX)
        || y < f64::from(i32::MIN)
        || y > f64::from(i32::MAX)
    {
        return Err(invalid(format!("{description} exceed the Windows range")));
    }
    Ok(Point::new(x as i32, y as i32))
}

fn mouse_button(button: MouseButton) -> UiaMouseButton {
    match button {
        MouseButton::Left => UiaMouseButton::LEFT,
        MouseButton::Right => UiaMouseButton::RIGHT,
        MouseButton::Middle => UiaMouseButton::MIDDLE,
    }
}

fn invalid(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::InvalidAction, message)
}

fn operation(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::OperationFailed, message)
}
