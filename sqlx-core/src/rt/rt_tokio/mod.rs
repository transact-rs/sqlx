mod socket;

#[inline(always)]
pub fn available() -> bool {
    tokio::runtime::Handle::try_current().is_ok()
}
