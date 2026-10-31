use std::num::Saturating;

use crate::message::{FrontendMessage, FrontendMessageFormat};

pub struct GssResponse<'t> {
    pub token: &'t [u8],
}
impl FrontendMessage for GssResponse<'_> {
    const FORMAT: FrontendMessageFormat = FrontendMessageFormat::PasswordPolymorphic;

    fn body_size_hint(&self) -> Saturating<usize> {
        Saturating(self.token.len())
    }

    fn encode_body(&self, buf: &mut Vec<u8>) -> Result<(), sqlx_core::Error> {
        buf.extend_from_slice(self.token);
        Ok(())
    }
}
