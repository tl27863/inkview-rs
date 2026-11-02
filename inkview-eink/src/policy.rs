//! Which InkView update the backend issues for each class of screen change.
//!
//! The defaults reproduce the backend's tuned behaviour on the PB710. Other
//! choices are for measuring on a device, through `INKVIEW_SLINT_UPDATES`
//! (e.g. `busy_content=dynamic-bw,present=ex:gl16`).

use core::fmt;

use inkview::screen::{UpdFlags, UpdateKind, Waveform};

use crate::screen::EinkScreen;

/// Environment variable with per-class overrides.
pub const UPDATES_ENV: &str = "INKVIEW_SLINT_UPDATES";

/// A named InkView update, or an explicit waveform through `UpdateEx`
/// (SDK 6.10+; falls back to `Partial` where unavailable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateMode {
    Named(UpdateKind),
    Ex(Waveform),
}

impl UpdateMode {
    /// Parses `partial`, `dynamic-bw`, … or `ex:<waveform>` (`ex:gl16`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.strip_prefix("ex:") {
            Some(wf) => Waveform::from_name(wf).map(Self::Ex),
            None => UpdateKind::from_name(s).map(Self::Named),
        }
    }

    /// Issues this update on `screen`.
    pub fn issue<S: EinkScreen + ?Sized>(self, screen: &mut S, x: i32, y: i32, w: u32, h: u32) {
        match self {
            Self::Named(kind) => screen.update(kind, x, y, w, h),
            Self::Ex(waveform) => {
                if !screen.update_ex(waveform, UpdFlags::NONE, x, y, w, h) {
                    screen.update(UpdateKind::Partial, x, y, w, h);
                }
            }
        }
    }
}

impl fmt::Display for UpdateMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(kind) => write!(f, "{kind}"),
            Self::Ex(waveform) => write!(f, "ex:{waveform}"),
        }
    }
}

/// One update mode per class of screen change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdatePolicy {
    /// Damage while the panel is idle (answer reveal, next card, page turn).
    pub idle_content: UpdateMode,
    /// Small damage while the panel is still busy (taps, feedback).
    pub busy_content: UpdateMode,
    /// The union of damage deferred while busy, once the panel is idle.
    pub finalize: UpdateMode,
    /// Full presents: screen transitions and show/resume.
    pub present: UpdateMode,
    /// Recovery when the panel seems stuck.
    pub fallback: UpdateMode,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        Self {
            idle_content: UpdateMode::Named(UpdateKind::Partial),
            busy_content: UpdateMode::Named(UpdateKind::Dynamic),
            finalize: UpdateMode::Named(UpdateKind::Partial),
            present: UpdateMode::Named(UpdateKind::Partial),
            fallback: UpdateMode::Named(UpdateKind::Full),
        }
    }
}

impl UpdatePolicy {
    /// The default policy for a panel: colour panels present with
    /// `PartialUpdateHQ`, as KOReader does for their flashing refreshes.
    pub fn for_panel(color: bool) -> Self {
        let mut policy = Self::default();
        if color {
            policy.present = UpdateMode::Named(UpdateKind::PartialHq);
        }
        policy
    }

    /// Applies `class=mode` pairs separated by commas. Returns one message per
    /// entry that was ignored.
    pub fn apply_overrides(&mut self, spec: &str) -> Vec<String> {
        let mut ignored = Vec::new();
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let Some((class, mode)) = entry.split_once('=') else {
                ignored.push(format!("'{entry}': expected class=mode"));
                continue;
            };
            let Some(mode) = UpdateMode::parse(mode.trim()) else {
                ignored.push(format!("'{entry}': unknown mode"));
                continue;
            };
            let slot = match class.trim() {
                "idle_content" => &mut self.idle_content,
                "busy_content" => &mut self.busy_content,
                "finalize" => &mut self.finalize,
                "present" => &mut self.present,
                "fallback" => &mut self.fallback,
                _ => {
                    ignored.push(format!("'{entry}': unknown class"));
                    continue;
                }
            };
            *slot = mode;
        }
        ignored
    }
}

impl fmt::Display for UpdatePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "idle_content={} busy_content={} finalize={} present={} fallback={}",
            self.idle_content, self.busy_content, self.finalize, self.present, self.fallback
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_matches_tuned_backend() {
        assert_eq!(
            UpdatePolicy::default().to_string(),
            "idle_content=partial busy_content=dynamic finalize=partial present=partial fallback=full"
        );
        assert_eq!(
            UpdatePolicy::for_panel(true).present,
            UpdateMode::Named(UpdateKind::PartialHq)
        );
        assert_eq!(UpdatePolicy::for_panel(false), UpdatePolicy::default());
    }

    #[test]
    fn overrides_apply_and_report_bad_entries() {
        let mut policy = UpdatePolicy::default();
        let ignored = policy.apply_overrides(
            " busy_content=dynamic-bw, present=ex:gl16,finalize=gl16,colour=partial,oops,",
        );
        assert_eq!(
            policy.busy_content,
            UpdateMode::Named(UpdateKind::DynamicBw)
        );
        assert_eq!(policy.present, UpdateMode::Ex(Waveform::Gl16));
        assert_eq!(policy.finalize, UpdateMode::Named(UpdateKind::Partial));
        assert_eq!(ignored.len(), 3, "{ignored:?}");
    }
}
