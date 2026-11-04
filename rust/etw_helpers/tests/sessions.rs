#[cfg(test)]
mod functional {
    use etw_helpers::controller::{SessionBuilder, get_existing_session};

    #[test]
    fn create_session() -> Result<(), etw_helpers::Error> {
        let _h = SessionBuilder::new_buffering_mode("EtwSession-Rust-Tests-Create").start(true)?;

        Ok(())
    }

    #[test]
    fn get_session() -> Result<(), etw_helpers::Error> {
        const SESSION_NAME: &str = "EtwSession-Rust-Tests-Get";

        // A session of our own: the handle `get_existing_session` returns would stop a system
        // session such as Eventlog-Application when dropped.
        let _h = SessionBuilder::new_buffering_mode(SESSION_NAME).start(true)?;

        // `_h` stops the session.
        let _existing = get_existing_session(SESSION_NAME)?.manual_stop();

        Ok(())
    }
}
