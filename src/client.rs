use crate::{
    id::RequestIdGenerator,
    meta::{BeginRequestRec, EndRequestRec, Header, ParamPairs, RequestType, Role},
    params::Params,
    request::Request,
    ClientError, ClientResult,
};
use log::debug;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Async client for handling communication between fastcgi server.
pub struct Client<S: AsyncRead + AsyncWrite + Send + Sync + Unpin> {
    stream: S,
    keep_alive: bool,
    request_id_generator: RequestIdGenerator,
}

impl<S: AsyncRead + AsyncWrite + Send + Sync + Unpin> Client<S> {
    /// Construct a `Client` Object with stream, such as `tokio::net::TcpStream`
    /// or `tokio::net::UnixStream`.
    pub fn new(stream: S, keep_alive: bool) -> Self {
        Self {
            stream,
            keep_alive,
            request_id_generator: RequestIdGenerator::new(Duration::from_millis(1500)),
        }
    }

    /// Send request and receive response from fastcgi server.
    pub async fn execute<I: AsyncRead + Unpin>(
        &mut self,
        mut request: Request<'_, I>,
        stdout: &mut (impl AsyncWrite + Unpin),
        stderr: &mut (impl AsyncWrite + Unpin),
    ) -> ClientResult<()> {
        let id = self.handle_new_request(&request.params, &mut request.stdin)
            .await?;
        self.handle_response(id, stdout, stderr).await
    }

    pub async fn generate_id(&mut self) -> ClientResult<u16> {
        self.request_id_generator.alloc().await
    }

    pub async fn handle_new_request<'a>(
        &mut self,
        params: &Params<'a>,
        body: &mut (dyn AsyncRead + Unpin),
    ) -> ClientResult<u16> {
        let id = self.request_id_generator.alloc().await?;
        match self.handle_request(id, params, body).await {
            Ok(()) => Ok(id),
            Err(err) => {
                self.request_id_generator.release(id).await;
                Err(err)
            }
        }
    }

    pub async fn handle_request<'a>(
        &mut self,
        id: u16,
        params: &Params<'a>,
        body: &mut (dyn AsyncRead + Unpin),
    ) -> ClientResult<()> {
        let write_stream = &mut self.stream;

        debug!("[id = {}] Start handle request.", id);

        handle_fastcgi_request(write_stream, self.keep_alive, id, params, body).await
    }

    pub async fn handle_response(&mut self, id: u16,
                             stdout: &mut (impl AsyncWrite + Unpin),
                             stderr: &mut (impl AsyncWrite + Unpin),
    ) -> ClientResult<()> {
        let read_stream = &mut self.stream;
        let rv = handle_fastcgi_response(read_stream, id, stdout, stderr).await;

        self.request_id_generator.release(id).await;

        rv
    }
}

pub async fn handle_fastcgi_request<'a>(
    write_stream: &mut (dyn AsyncWrite + Unpin),
    keep_alive: bool,
    id: u16,
    params: &Params<'a>,
    body: &mut (dyn AsyncRead + Unpin),
) -> ClientResult<()> {
    debug!("[id = {}] Start handle request.", id);

    let begin_request_rec = BeginRequestRec::new(id, Role::Responder, keep_alive).await?;
    debug!("[id = {}] Send to stream: {:?}.", id, &begin_request_rec);
    begin_request_rec.write_to_stream(write_stream).await?;

    let param_pairs = ParamPairs::new(params);
    debug!("[id = {}] Params will be sent: {:?}.", id, &param_pairs);

    Header::write_to_stream_batches(
        RequestType::Params,
        id,
        write_stream,
        &mut &param_pairs.to_content().await?[..],
        Some(|header| {
            debug!("[id = {}] Send to stream for Params: {:?}.", id, &header);
            header
        }),
    )
        .await?;

    // this empty record marks the end of the Params-stream
    Header::write_to_stream_batches(
        RequestType::Params,
        id,
        write_stream,
        &mut tokio::io::empty(),
        Some(|header| {
            debug!("[id = {}] Send to stream for Params: {:?}.", id, &header);
            header
        }),
    )
        .await?;

    Header::write_to_stream_batches(
        RequestType::Stdin,
        id,
        write_stream,
        body,
        Some(|header| {
            debug!("[id = {}] Send to stream for Stdin: {:?}.", id, &header);
            header
        }),
    )
        .await?;

    // this empty record marks the end of the Stdin-stream
    Header::write_to_stream_batches(
        RequestType::Stdin,
        id,
        write_stream,
        &mut tokio::io::empty(),
        Some(|header| {
            debug!("[id = {}] Send to stream for Stdin: {:?}.", id, &header);
            header
        }),
    )
        .await?;

    write_stream.flush().await?;

    Ok(())
}

pub async fn handle_fastcgi_response(
    read_stream: &mut (impl AsyncRead + Unpin + Send),
    id: u16,
    stdout: &mut (impl AsyncWrite + Unpin),
    stderr: &mut (impl AsyncWrite + Unpin),
) -> ClientResult<()> {
    let global_end_request_rec = loop {
        let header = Header::new_from_stream(read_stream).await?;
        debug!("[id = {}] Receive from stream: {:?}.", id, &header);

        if header.request_id != id {
            return Err(ClientError::ResponseNotFound { id }.into());
        }

        match header.r#type {
            RequestType::Stdout => {
                let content = header.read_content_from_stream(read_stream).await?;
                if header.content_length > 100 {
                    debug!("PHP stdout: '{}...{}'", String::from_utf8_lossy(&content[0..50]), String::from_utf8_lossy(&content[(header.content_length-50) as usize..]));
                }
                else {
                    debug!("PHP stdout: '{}'", String::from_utf8_lossy(&content));
                }
                let len = content.len();
                let written_len = AsyncWriteExt::write(stdout, content.as_ref()).await?;
                if len != written_len {
                    return Err(ClientError::UnexpectedEndOfOutput{
                        id,
                        output_type: RequestType::Stdout,
                        written: written_len,
                        expected: len
                    }.into())
                }
            }
            RequestType::Stderr => {
                let content = header.read_content_from_stream(read_stream).await?;
                debug!("PHP stderr: '{}'", String::from_utf8_lossy(&content));
                let len = content.len();
                let written_len = AsyncWriteExt::write(stderr, content.as_ref()).await?;
                if len != written_len {
                    return Err(ClientError::UnexpectedEndOfOutput{
                        id,
                        output_type: RequestType::Stderr,
                        written: written_len,
                        expected: len
                    }.into())
                }
            }
            RequestType::EndRequest => {
                let end_request_rec = EndRequestRec::from_header(&header, read_stream).await?;
                debug!("[id = {}] Receive from stream: {:?}.", id, &end_request_rec);
                break Some(end_request_rec);
            }
            r#type => {
                return Err(ClientError::UnknownRequestType {
                    request_type: r#type,
                }
                    .into())
            }
        }
    };

    match global_end_request_rec {
        Some(end_request_rec) => end_request_rec
            .end_request
            .protocol_status
            .convert_to_client_result(end_request_rec.end_request.app_status),
        None => unreachable!(),
    }
}