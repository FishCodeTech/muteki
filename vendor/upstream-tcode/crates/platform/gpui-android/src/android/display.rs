use anyhow::Result;
use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay};
use std::{cell::Cell, fmt};
use uuid::Uuid;

pub(crate) struct AndroidDisplay {
    bounds: Cell<Bounds<Pixels>>,
    refresh_rate: Cell<f32>,
}

impl AndroidDisplay {
    pub(crate) fn new(bounds: Bounds<Pixels>) -> Self {
        Self {
            bounds: Cell::new(bounds),
            refresh_rate: Cell::new(0.0),
        }
    }

    pub(crate) fn set_refresh_rate(&self, rate: f32) -> bool {
        self.refresh_rate.replace(rate) != rate
    }

    pub(crate) fn set_bounds(&self, bounds: Bounds<Pixels>) {
        self.bounds.set(bounds);
    }
}

impl fmt::Debug for AndroidDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AndroidDisplay")
            .field("bounds", &self.bounds.get())
            .finish()
    }
}

impl PlatformDisplay for AndroidDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(0)
    }

    fn refresh_interval(&self) -> Option<std::time::Duration> {
        let rate = f64::from(self.refresh_rate.get());
        (rate > 0.0).then(|| std::time::Duration::from_secs_f64(1.0 / rate))
    }

    fn uuid(&self) -> Result<Uuid> {
        Ok(Uuid::new_v5(&Uuid::NAMESPACE_OID, b"gpui-android-primary"))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    fn visible_bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }
}
