use crate::message::{FrontendMessage, FrontendMessageFormat};

pub struct GssResponse<'g> {
    pub(crate) token: &'g [u8],
}
impl<'g> FrontendMessage for GssResponse<'g> {
    const FORMAT: FrontendMessageFormat = FrontendMessageFormat::PasswordPolymorphic;

    fn body_size_hint(&self) -> std::num::Saturating<usize> {
        let mut size = std::num::Saturating(0);
        size += 4;
        size += self.token.len();
        size
    }

    fn encode_body(&self, buf: &mut Vec<u8>) -> Result<(), sqlx_core::Error> {
        buf.extend_from_slice(&self.token);
        Ok(())
    }
}
