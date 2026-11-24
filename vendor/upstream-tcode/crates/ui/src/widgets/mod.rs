/// Shared callback shape for toggle-style widget events (`&bool` new state).
pub(crate) type ToggleHandler = std::rc::Rc<dyn Fn(&bool, &mut gpui::Window, &mut gpui::App)>;

pub mod button;
pub mod checkbox;
pub mod choice;
pub(crate) mod copy;
mod fade_text;
pub mod input;
pub mod kbd;
pub mod menu;
mod popover;
pub mod progress;
pub(crate) mod ring;
pub mod slider;
pub mod spinner;
pub mod switch;
pub mod tabs;
pub mod tooltip;

pub use button::{Button, ButtonVariant, ButtonVariants};
pub use checkbox::Checkbox;
pub use fade_text::{FadeText, fade_text};
pub use input::{Input, Textarea};
pub use kbd::Kbd;
pub use popover::Popover;
pub use progress::Progress;
pub use slider::Slider;
pub use spinner::Spinner;
pub use switch::Switch;
pub use tooltip::Tooltip;
