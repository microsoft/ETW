#[cfg(test)]
mod functional {
    use etw_helpers::SessionBuilder;

    #[allow(overflowing_literals)]
    static E_ACCESSDENIED: windows::core::HRESULT = windows::core::HRESULT(0x80070005);

    #[test]
    fn create_session() -> Result<(), windows::core::Error> {
        let _h = SessionBuilder::new_buffering_mode("EtwSession-Rust-Tests-Create").start(true)?;

        Ok(())
    }

    #[test]
    fn get_session() -> Result<(), windows::core::Error> {
        let access_denied = windows::core::Error::from_hresult(E_ACCESSDENIED);
        let res = etw_helpers::get_existing_session("Eventlog-Application");
        match res {
            Ok(_) => Ok(()),
            Err(x) => if x == access_denied {
                Ok(())
            } else {
                Err(x)
            }
        }
    }
}
