#[cfg(test)]
mod functional {
    use etw_helpers::controller::SessionBuilder;

    #[allow(overflowing_literals)]
    static E_ACCESSDENIED: windows_sys::core::HRESULT = 0x80070005;

    #[test]
    fn create_session() -> Result<(), etw_helpers::Error> {
        let _h = SessionBuilder::new_buffering_mode("EtwSession-Rust-Tests-Create").start(true)?;

        Ok(())
    }

    #[test]
    fn get_session() -> Result<(), etw_helpers::Error> {
        let access_denied = etw_helpers::Error::from_hresult(E_ACCESSDENIED);
        let res = etw_helpers::controller::get_existing_session("Eventlog-Application");
        match res {
            Ok(_) => Ok(()),
            Err(x) => {
                if x == access_denied {
                    Ok(())
                } else {
                    Err(x)
                }
            }
        }
    }
}
