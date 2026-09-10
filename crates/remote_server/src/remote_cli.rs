use anyhow::{Context as _, Result, bail, ensure};
use clap::Args;
use futures::{
    AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, FutureExt as _, StreamExt as _,
    channel::oneshot, stream::FuturesUnordered,
};
use gpui::{App, AppContext as _, BackgroundExecutor, Entity, FutureExt as _};
use net::async_net::{UnixListener, UnixStream};
use remote::protocol::{read_message_with_len, write_message};
use rpc::{
    AnyProtoClient, TypedEnvelope,
    proto::{self, EnvelopedMessage as _},
};
use std::{fs, io::ErrorKind, os::unix::fs::PermissionsExt as _, path::Path, time::Duration};
use tempfile::TempDir;
use util::{ResultExt as _, paths::PathWithPosition};

use crate::HeadlessProject;

const SOCKET_ENV: &str = "ZED_REMOTE_CLI_SOCKET";
const MAX_MESSAGE_BYTES: u32 = 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

#[derive(Args, Debug)]
pub struct RemoteCliArgs {
    /// Wait until the opened files are closed, or the workspace is closed for directories.
    #[arg(short, long)]
    wait: bool,
    /// Add paths to the remote workspace that owns this terminal.
    #[arg(short, long)]
    add: bool,
    /// Print the remote CLI version.
    #[arg(short, long)]
    version: bool,
    /// Files or directories to open, optionally followed by :line:column.
    paths: Vec<String>,
}

pub fn run_cli(arguments: RemoteCliArgs) -> Result<()> {
    if arguments.version {
        println!("Zed remote {}", *crate::VERSION);
        return Ok(());
    }

    let socket = std::env::var_os(SOCKET_ENV).context(
        "No Zed remote session. Run zed from a terminal opened in a Zed remote workspace.",
    )?;
    let directory = std::env::current_dir().context("reading current directory")?;
    let paths = if arguments.paths.is_empty() {
        vec![".".to_owned()]
    } else {
        arguments.paths
    };
    let paths = paths
        .iter()
        .map(|path| resolve_cli_path(path, &directory))
        .collect::<Result<Vec<_>>>()?;
    let request = proto::OpenRemoteCliPaths {
        project_id: proto::REMOTE_SERVER_PROJECT_ID,
        request_id: 0,
        paths,
        wait: arguments.wait,
    };

    smol::block_on(async {
        let mut stream = UnixStream::connect(Path::new(&socket)).await.context(
            "connecting to Zed remote session; open a new terminal if the session restarted",
        )?;
        let mut buffer = Vec::new();
        let envelope = request.into_envelope(0, None, None);
        ensure!(
            envelope.encoded_size() <= MAX_MESSAGE_BYTES as usize,
            "too many paths to open"
        );
        write_message(&mut stream, &mut buffer, envelope).await?;
        stream.flush().await?;
        let response = read_cli_message(&mut stream, &mut buffer)
            .await
            .context("Zed remote session ended before the request completed")?;
        match response.payload {
            Some(proto::envelope::Payload::Ack(_)) => Ok(()),
            Some(proto::envelope::Payload::Error(error)) => bail!("{}", error.message),
            _ => bail!("invalid response from Zed remote session"),
        }
    })
}

fn resolve_cli_path(value: &str, directory: &Path) -> Result<proto::RemoteCliPath> {
    let value = shellexpand::tilde(value);
    let original = directory.join(value.as_ref());
    let literal_exists = match fs::metadata(&original) {
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(error).with_context(|| format!("reading {}", original.display())),
    };
    let mut parsed = if literal_exists {
        PathWithPosition::from_path(original)
    } else {
        PathWithPosition::parse_str(&value)
    };
    parsed.path = directory.join(parsed.path);
    let is_directory = match fs::metadata(&parsed.path) {
        Ok(metadata) => {
            parsed.path = fs::canonicalize(&parsed.path)?;
            metadata.is_dir()
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if let Some((parent, filename)) = parsed.path.parent().zip(parsed.path.file_name()) {
                match fs::canonicalize(parent) {
                    Ok(parent) => parsed.path = parent.join(filename),
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("resolving {}", parent.display()));
                    }
                }
            }
            false
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", parsed.path.display()));
        }
    };
    Ok(proto::RemoteCliPath {
        path: parsed
            .path
            .to_str()
            .context("remote CLI paths must be valid UTF-8")?
            .to_owned(),
        row: parsed.row,
        column: parsed.column,
        is_directory,
    })
}

pub(crate) struct RemoteCliServer {
    directory: TempDir,
    listener: UnixListener,
    info: proto::RemoteCliInfo,
}

impl RemoteCliServer {
    pub fn new() -> Result<Self> {
        let directory = tempfile::Builder::new().prefix("zed-cli-").tempdir()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let socket_path = directory.path().join("cli.sock");
        let listener = UnixListener::bind(&socket_path).context("binding remote CLI socket")?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        let bin_path = directory.path().join("bin");
        fs::create_dir(&bin_path)?;
        let launcher_path = bin_path.join("zed");
        let executable = std::env::current_exe().context("locating remote server executable")?;
        let executable = executable
            .to_str()
            .context("remote server path must be valid UTF-8")?;
        fs::write(&launcher_path, launcher_script(executable))?;
        fs::set_permissions(&launcher_path, fs::Permissions::from_mode(0o700))?;
        let info = proto::RemoteCliInfo {
            socket_path: socket_path
                .to_str()
                .context("remote CLI socket path must be valid UTF-8")?
                .to_owned(),
            bin_path: bin_path
                .to_str()
                .context("remote CLI bin path must be valid UTF-8")?
                .to_owned(),
        };
        Ok(Self {
            directory,
            listener,
            info,
        })
    }

    pub fn start(
        self,
        project: &Entity<HeadlessProject>,
        session: AnyProtoClient,
        connection: watch::Receiver<bool>,
        cx: &mut App,
    ) {
        let info = self.info.clone();
        session.add_request_handler(
            project.downgrade(),
            move |_, _: TypedEnvelope<proto::GetRemoteCliInfo>, _| {
                let info = info.clone();
                async move { Ok(info) }
            },
        );
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let executor = cx.background_executor().clone();
        let task = cx.background_spawn(async move {
            self.serve(session, connection, shutdown_receiver, executor)
                .await
                .log_err();
        });
        let mut shutdown = Some((shutdown_sender, task));
        cx.on_app_quit(move |_| {
            let shutdown = shutdown.take();
            async move {
                if let Some((sender, task)) = shutdown {
                    if sender.send(()).is_err() {
                        log::debug!("remote CLI listener has already stopped");
                    }
                    task.await;
                }
            }
        })
        .detach();
    }

    async fn serve(
        self,
        session: AnyProtoClient,
        connection: watch::Receiver<bool>,
        shutdown: oneshot::Receiver<()>,
        executor: BackgroundExecutor,
    ) -> Result<()> {
        let Self {
            directory: _directory,
            listener,
            info: _,
        } = self;
        let mut requests = FuturesUnordered::new();
        let mut next_request_id = 0_u64;
        let shutdown = shutdown.fuse();
        futures::pin_mut!(shutdown);
        loop {
            let accept_connection = requests.len() < MAX_CONNECTIONS;
            let accepted = async {
                if !accept_connection {
                    futures::future::pending::<()>().await;
                }
                listener.accept().await
            }
            .fuse();
            futures::pin_mut!(accepted);
            futures::select_biased! {
                _ = shutdown => break,
                result = requests.select_next_some() => {
                    let result: Result<()> = result;
                    result.log_err();
                }
                accepted = accepted => {
                    let (stream, _) = accepted.context("accepting remote CLI connection")?;
                    next_request_id = next_request_id.checked_add(1).context("remote CLI request ID overflow")?;
                    requests.push(serve_connection(stream, session.clone(), connection.clone(), next_request_id, executor.clone()));
                }
            }
        }
        drop(listener);
        // Give completions already received from the UI a chance to reach their CLI before teardown.
        let drain = async {
            while let Some(result) = requests.next().await {
                result.log_err();
            }
        }
        .fuse();
        futures::pin_mut!(drain);
        futures::select_biased! {
            _ = drain => {},
            _ = executor.timer(SHUTDOWN_GRACE).fuse() => {},
        }
        Ok(())
    }
}

fn launcher_script(executable: &str) -> String {
    let executable = executable.replace('\'', "'\\''");
    format!("#!/bin/sh\nexec '{executable}' cli \"$@\"\n")
}

async fn read_cli_message(
    stream: &mut (impl AsyncRead + Unpin),
    buffer: &mut Vec<u8>,
) -> Result<proto::Envelope> {
    let mut length = [0; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length);
    ensure!(
        length <= MAX_MESSAGE_BYTES,
        "remote CLI message exceeds size limit"
    );
    read_message_with_len(stream, buffer, length).await
}

async fn serve_connection(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    session: AnyProtoClient,
    mut connection: watch::Receiver<bool>,
    request_id: u64,
    executor: BackgroundExecutor,
) -> Result<()> {
    let mut buffer = Vec::new();
    let result = async {
        let message = read_cli_message(&mut stream, &mut buffer)
            .with_timeout(IO_TIMEOUT, &executor)
            .await
            .context("timed out reading remote CLI request")??;
        let Some(proto::envelope::Payload::OpenRemoteCliPaths(mut request)) = message.payload
        else {
            bail!("expected a remote CLI open request");
        };
        ensure!(*connection.borrow(), "Zed remote session is disconnected");
        request.project_id = proto::REMOTE_SERVER_PROJECT_ID;
        request.request_id = request_id;
        let response = session.request(request).fuse();
        let mut unexpected_input = [0];
        let disconnected = stream.read(&mut unexpected_input).fuse();
        futures::pin_mut!(response, disconnected);
        futures::select_biased! {
            response = response => response,
            _ = connection.changed().fuse() => {
                session.send(proto::CancelRemoteCliRequest {
                    project_id: proto::REMOTE_SERVER_PROJECT_ID,
                    request_id,
                }).log_err();
                bail!("Zed remote connection was lost before the request completed");
            }
            input = disconnected => {
                session.send(proto::CancelRemoteCliRequest {
                    project_id: proto::REMOTE_SERVER_PROJECT_ID,
                    request_id,
                }).log_err();
                input.context("reading remote CLI connection")?;
                bail!("remote CLI disconnected before the request completed");
            }
        }
    }
    .await;
    let response = match result {
        Ok(response) => response.into_envelope(0, Some(0), None),
        Err(error) => proto::Error {
            message: format!("{error:#}"),
            code: proto::ErrorCode::Internal as i32,
            tags: Vec::new(),
        }
        .into_envelope(0, Some(0), None),
    };
    async {
        write_message(&mut stream, &mut buffer, response).await?;
        stream.flush().await?;
        anyhow::Ok(())
    }
    .with_timeout(IO_TIMEOUT, &executor)
    .await
    .context("timed out writing remote CLI response")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use gpui::{Task, TestAppContext};
    use parking_lot::Mutex;
    use rpc::{ProtoClient, ProtoMessageHandlerSet};
    use std::{
        os::unix::fs::symlink,
        pin::Pin,
        sync::Arc,
        task::{Context as TaskContext, Poll},
    };

    struct TestStream {
        input: futures::io::Cursor<Vec<u8>>,
        closed: oneshot::Receiver<()>,
        output: Arc<Mutex<Vec<u8>>>,
    }

    impl AsyncRead for TestStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buffer: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.input.position() < self.input.get_ref().len() as u64 {
                Pin::new(&mut self.input).poll_read(cx, buffer)
            } else {
                Pin::new(&mut self.closed).poll(cx).map(|_| Ok(0))
            }
        }
    }

    impl AsyncWrite for TestStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.output.lock().extend_from_slice(buffer);
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    type TestRequest = (proto::Envelope, oneshot::Sender<proto::Envelope>);

    struct TestClient {
        requests: async_channel::Sender<TestRequest>,
        messages: async_channel::Sender<proto::Envelope>,
        handlers: Mutex<ProtoMessageHandlerSet>,
    }

    impl ProtoClient for TestClient {
        fn request(
            &self,
            envelope: proto::Envelope,
            _: &'static str,
        ) -> futures::future::BoxFuture<'static, Result<proto::Envelope>> {
            let (sender, receiver) = oneshot::channel();
            let sent = self.requests.try_send((envelope, sender));
            async move {
                sent.context("test request receiver closed")?;
                receiver.await.context("test response sender dropped")
            }
            .boxed()
        }

        fn send(&self, envelope: proto::Envelope, _: &'static str) -> Result<()> {
            self.messages
                .try_send(envelope)
                .context("test message receiver closed")
        }

        fn send_response(&self, _: proto::Envelope, _: &'static str) -> Result<()> {
            bail!("unexpected server response in test client")
        }

        fn message_handler_set(&self) -> &Mutex<ProtoMessageHandlerSet> {
            &self.handlers
        }

        fn is_via_collab(&self) -> bool {
            false
        }
        fn has_wsl_interop(&self) -> bool {
            false
        }
    }

    struct TestConnection {
        task: Task<Result<()>>,
        closed: oneshot::Sender<()>,
        connection: watch::Sender<bool>,
        requests: async_channel::Receiver<TestRequest>,
        messages: async_channel::Receiver<proto::Envelope>,
        output: Arc<Mutex<Vec<u8>>>,
    }

    fn test_connection(wait: bool, cx: &mut TestAppContext) -> Result<TestConnection> {
        let (requests_sender, requests) = async_channel::unbounded();
        let (messages_sender, messages) = async_channel::unbounded();
        let session = Arc::new(TestClient {
            requests: requests_sender,
            messages: messages_sender,
            handlers: Mutex::default(),
        })
        .into();
        let (mut connection, connection_receiver) = watch::channel(false);
        connection.send(true)?;
        let (closed, closed_receiver) = oneshot::channel();
        let request = proto::OpenRemoteCliPaths {
            project_id: 999,
            request_id: 999,
            paths: vec![proto::RemoteCliPath {
                path: "/repo/file.txt".into(),
                row: Some(3),
                column: Some(2),
                is_directory: false,
            }],
            wait,
        }
        .into_envelope(0, None, None);
        let mut encoded = Vec::new();
        request.encode_to_buffer(&mut encoded)?;
        let mut input = (encoded.len() as u32).to_le_bytes().to_vec();
        input.extend(encoded);
        let output = Arc::new(Mutex::new(Vec::new()));
        let stream = TestStream {
            input: futures::io::Cursor::new(input),
            closed: closed_receiver,
            output: output.clone(),
        };
        let executor = cx.background_executor.clone();
        let task = cx.background_executor.spawn(serve_connection(
            stream,
            session,
            connection_receiver,
            42,
            executor,
        ));
        Ok(TestConnection {
            task,
            closed,
            connection,
            requests,
            messages,
            output,
        })
    }

    async fn test_response(output: Arc<Mutex<Vec<u8>>>) -> proto::envelope::Payload {
        let mut stream = futures::io::Cursor::new(output.lock().clone());
        read_cli_message(&mut stream, &mut Vec::new())
            .await
            .expect("decode CLI response")
            .payload
            .expect("response payload")
    }

    #[gpui::test]
    async fn test_remote_cli_socket_success(cx: &mut TestAppContext) {
        for wait in [false, true] {
            let mut connection = test_connection(wait, cx).expect("test connection");
            let (request, response) = connection.requests.recv().await.expect("forwarded request");
            let request = proto::OpenRemoteCliPaths::from_envelope(request).expect("open request");
            assert_eq!(
                (request.project_id, request.request_id),
                (proto::REMOTE_SERVER_PROJECT_ID, 42)
            );
            assert_eq!(request.wait, wait);
            assert_eq!(request.paths.first().expect("path").row, Some(3));
            assert!(futures::poll!(&mut connection.task).is_pending());
            assert!(connection.output.lock().is_empty());
            response
                .send(proto::Ack {}.into_envelope(0, None, None))
                .expect("send response");
            connection.task.await.expect("serve CLI");
            assert!(matches!(
                test_response(connection.output).await,
                proto::envelope::Payload::Ack(_)
            ));
            assert!(connection.messages.try_recv().is_err());
        }
    }

    #[gpui::test]
    async fn test_remote_cli_completed_response_wins_disconnect(cx: &mut TestAppContext) {
        let mut connection = test_connection(true, cx).expect("test connection");
        let (_, response) = connection.requests.recv().await.expect("forwarded request");
        response
            .send(proto::Ack {}.into_envelope(0, None, None))
            .expect("send response");
        connection.connection.send(false).expect("disconnect");
        connection.task.await.expect("serve CLI");
        assert!(matches!(
            test_response(connection.output).await,
            proto::envelope::Payload::Ack(_)
        ));
        assert!(connection.messages.try_recv().is_err());
    }

    #[gpui::test]
    async fn test_remote_cli_disconnect_cancels_request(cx: &mut TestAppContext) {
        let mut connection = test_connection(true, cx).expect("test connection");
        let (_, response) = connection.requests.recv().await.expect("forwarded request");
        connection.connection.send(false).expect("disconnect");
        connection.connection.send(true).expect("reconnect");
        connection.task.await.expect("serve CLI");
        let cancel = proto::CancelRemoteCliRequest::from_envelope(
            connection.messages.recv().await.expect("cancel message"),
        )
        .expect("cancel payload");
        assert_eq!(cancel.request_id, 42);
        assert!(response.is_canceled());
        assert!(matches!(
            test_response(connection.output).await,
            proto::envelope::Payload::Error(_)
        ));
    }

    #[gpui::test]
    async fn test_remote_cli_socket_eof_cancels_request(cx: &mut TestAppContext) {
        let connection = test_connection(true, cx).expect("test connection");
        let (_, response) = connection.requests.recv().await.expect("forwarded request");
        connection.closed.send(()).expect("close CLI");
        connection.task.await.expect("serve CLI");
        let cancel = proto::CancelRemoteCliRequest::from_envelope(
            connection.messages.recv().await.expect("cancel message"),
        )
        .expect("cancel payload");
        assert_eq!(cancel.request_id, 42);
        assert!(response.is_canceled());
    }

    #[derive(Parser)]
    struct TestArgs {
        #[command(flatten)]
        cli: RemoteCliArgs,
    }

    #[test]
    fn test_remote_cli_arguments() -> Result<()> {
        let args = TestArgs::try_parse_from(["zed", "--wait", "--add", "file:10:2"])?;
        assert!(args.cli.wait);
        assert!(args.cli.add);
        assert_eq!(args.cli.paths, ["file:10:2"]);
        let args = TestArgs::try_parse_from(["zed", "file"])?;
        assert!(!args.cli.wait);
        assert!(TestArgs::try_parse_from(["zed", "--new", "file"]).is_err());
        Ok(())
    }

    #[test]
    fn test_remote_cli_paths() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let directory = fs::canonicalize(directory.path())?;
        fs::write(directory.join("file"), "text")?;
        fs::write(directory.join("file:10"), "literal")?;
        fs::create_dir(directory.join("Folder (3)"))?;
        symlink(directory.join("file"), directory.join("link"))?;
        let literal = resolve_cli_path("file:10", &directory)?;
        assert_eq!(
            literal.path,
            directory.join("file:10").to_str().context("path")?
        );
        assert_eq!(literal.row, None);
        let positioned = resolve_cli_path("file:20:3", &directory)?;
        assert_eq!(
            positioned.path,
            directory.join("file").to_str().context("path")?
        );
        assert_eq!((positioned.row, positioned.column), (Some(20), Some(3)));
        let missing = resolve_cli_path("new file 日本語:2", &directory)?;
        assert_eq!(
            missing.path,
            directory.join("new file 日本語").to_str().context("path")?
        );
        assert_eq!(missing.row, Some(2));
        assert!(!directory.join("new file 日本語").exists());
        let relative_missing = resolve_cli_path("../new.txt", &directory.join("Folder (3)"))?;
        assert_eq!(
            relative_missing.path,
            directory.join("new.txt").to_str().context("path")?
        );
        let folder = resolve_cli_path("Folder (3)", &directory)?;
        assert!(folder.is_directory);
        assert_eq!(folder.row, None);
        assert_eq!(
            resolve_cli_path("link", &directory)?.path,
            directory.join("file").to_str().context("path")?
        );
        Ok(())
    }

    #[test]
    fn test_remote_cli_launcher_quotes_executable() {
        assert_eq!(
            launcher_script("/some directory/zed's server"),
            "#!/bin/sh\nexec '/some directory/zed'\\''s server' cli \"$@\"\n"
        );
    }

    #[test]
    fn test_remote_cli_private_session() -> Result<()> {
        let server = RemoteCliServer::new()?;
        let other = RemoteCliServer::new()?;
        assert_ne!(server.info, other.info);
        assert_eq!(
            fs::metadata(server.directory.path())?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&server.info.socket_path)?.permissions().mode() & 0o777,
            0o600
        );
        let launcher = Path::new(&server.info.bin_path).join("zed");
        assert_eq!(fs::metadata(launcher)?.permissions().mode() & 0o777, 0o700);
        let directory = server.directory.path().to_owned();
        drop(server);
        assert!(!directory.exists());
        Ok(())
    }

    #[test]
    fn test_remote_cli_rejects_oversized_messages() {
        smol::block_on(async {
            let bytes = (MAX_MESSAGE_BYTES + 1).to_le_bytes();
            let mut stream = futures::io::Cursor::new(bytes);
            let mut buffer = Vec::new();
            assert!(read_cli_message(&mut stream, &mut buffer).await.is_err());
            assert!(buffer.is_empty());
        });
    }
}
