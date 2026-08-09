//! `TEARDOWN` — the sender is done with the stream. No params: the body
//! carries nothing this receiver acts on, and the command cannot fail.

use log::debug;

use crate::session::Session;

pub fn teardown(session: &mut Session) {
    debug!("ack TEARDOWN");
    session.end_session();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_helpers::{session, start_stream};
    use crate::events::Event;

    #[tokio::test]
    async fn ends_a_started_session_exactly_once() {
        let (mut session, mut events) = session();
        start_stream(&mut session).await;
        assert!(matches!(
            events.try_recv(),
            Ok(Event::SessionStarted { .. })
        ));

        teardown(&mut session);
        assert_eq!(events.try_recv(), Ok(Event::SessionEnded));

        // A second TEARDOWN (or the later connection drop) must not repeat it.
        teardown(&mut session);
        assert!(events.try_recv().is_err());
    }
}
