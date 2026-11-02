//! The event wait of a backend loop.

use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

/// The sending side of the event channel is gone; the loop should end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Disconnected;

/// Blocks for the first event for at most `delay` (does not block if `delay`
/// is `None`), then drains everything already queued into `out`, so
/// back-to-back Show/Foreground produce a single present.
pub fn wait_events<T>(
    receiver: &Receiver<T>,
    delay: Option<Duration>,
    out: &mut Vec<T>,
) -> Result<(), Disconnected> {
    let first = match delay {
        Some(delay) => match receiver.recv_timeout(delay) {
            Ok(evt) => Some(evt),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Err(Disconnected),
        },
        None => match receiver.try_recv() {
            Ok(evt) => Some(evt),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => return Err(Disconnected),
        },
    };
    if let Some(evt) = first {
        out.push(evt);
        while let Ok(evt) = receiver.try_recv() {
            out.push(evt);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn drains_everything_queued_after_the_first_event() {
        let (sender, receiver) = channel();
        let mut out = Vec::new();
        assert_eq!(wait_events(&receiver, None, &mut out), Ok(()));
        assert!(out.is_empty());

        sender.send(1).unwrap();
        sender.send(2).unwrap();
        assert_eq!(
            wait_events(&receiver, Some(Duration::from_secs(1)), &mut out),
            Ok(())
        );
        assert_eq!(out, [1, 2]);

        drop(sender);
        assert_eq!(wait_events(&receiver, None, &mut out), Err(Disconnected));
    }
}
