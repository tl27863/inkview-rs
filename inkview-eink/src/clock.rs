//! Time source of the e-ink presenter, so tests can drive time.

use std::{cell::Cell, rc::Rc, time::Instant};

/// `Instant::now()`, or a manual time in tests. The presenter reads it at the
/// same points where it read `Instant::now()` before, so timing does not
/// change.
#[derive(Clone, Default)]
pub(crate) struct Clock(Option<Rc<Cell<Instant>>>);

impl Clock {
    pub fn now(&self) -> Instant {
        match &self.0 {
            Some(time) => time.get(),
            None => Instant::now(),
        }
    }

    /// Time stands still until `advance`.
    #[cfg(test)]
    pub fn manual() -> Self {
        Self(Some(Rc::new(Cell::new(Instant::now()))))
    }

    #[cfg(test)]
    pub fn advance(&self, by: std::time::Duration) {
        let time = self.0.as_ref().expect("manual clock");
        time.set(time.get() + by);
    }
}
