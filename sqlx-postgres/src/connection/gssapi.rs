use std::borrow::Cow;

use crate::error::Error;
use cross_krb5::InitiateFlags;

use crate::{
    connection::PgStream,
    message::{Authentication, AuthenticationGss, GssResponse},
    PgConnectOptions,
};

pub async fn authenticate(stream: &mut PgStream, options: &PgConnectOptions) -> Result<(), Error> {
    let PgConnectOptions {
        host,
        gssapi_target_principal: gssapi_principal,
        ..
    } = options;
    let principal = gssapi_principal
        .as_ref()
        .map(Cow::Borrowed)
        .unwrap_or(Cow::Owned(format!("postgres/{host}")));
    let (mut ctx, token) =
        cross_krb5::ClientCtx::new(InitiateFlags::empty(), None, &principal, None)
            .map_err(|e| Error::GssApi(e.into()))?;
    let msg = GssResponse { token: &token };
    stream.send(msg).await?;
    loop {
        let token = match stream.recv_expect().await? {
            Authentication::GssContinue(AuthenticationGss { token }) => token,
            other => return Err(err_protocol!("expected GssContinue but receiver {other:?}")),
        };
        match ctx.step(&token).map_err(|e| Error::GssApi(e.into()))? {
            cross_krb5::Step::Finished((_context, last_token)) => {
                if let Some(last_token) = last_token {
                    stream.send(GssResponse { token: &last_token }).await?;
                }
                return Ok(());
            }
            cross_krb5::Step::Continue((pending, token)) => {
                ctx = pending;
                stream.send(GssResponse { token: &token }).await?;
            }
        }
    }
}
