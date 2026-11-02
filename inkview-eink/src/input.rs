//! Pointer input next to the system panel.

/// The kind of a pointer event, for [`content_pointer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerKind {
    Down,
    Move,
    Up,
}

/// Maps a physical pointer position to content coordinates below `top`
/// (the system panel's rows). Presses on the panel itself are dropped, since
/// the firmware handles them; moves and releases there are clamped to the
/// first content row.
pub fn content_pointer(top: i32, kind: PointerKind, x: i32, y: i32) -> Option<(i32, i32)> {
    match kind {
        PointerKind::Down if y < top => None,
        PointerKind::Down => Some((x, y - top)),
        PointerKind::Move | PointerKind::Up => Some((x, y.max(top) - top)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_on_the_panel_rows() {
        assert_eq!(content_pointer(20, PointerKind::Down, 5, 10), None);
        assert_eq!(content_pointer(20, PointerKind::Down, 5, 30), Some((5, 10)));
        assert_eq!(content_pointer(20, PointerKind::Move, 5, 10), Some((5, 0)));
        assert_eq!(content_pointer(20, PointerKind::Up, 5, 10), Some((5, 0)));
        assert_eq!(content_pointer(0, PointerKind::Down, 5, 0), Some((5, 0)));
    }
}
