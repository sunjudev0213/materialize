// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! gRPC transport for the [client](crate::client) module.

use std::fmt::{self, Debug};
use std::pin::Pin;
use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use futures::future;
use futures::stream::{Stream, StreamExt, TryStreamExt};
use once_cell::sync::Lazy;
use semver::Version;
use tokio::net::UnixStream;
use tokio::select;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::{oneshot, Mutex};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::body::BoxBody;
use tonic::codegen::InterceptedService;
use tonic::metadata::{AsciiMetadataKey, AsciiMetadataValue};
use tonic::service::Interceptor;
use tonic::transport::{Body, Channel, Endpoint, NamedService, Server};
use tonic::{Request, Response, Status, Streaming};
use tower::Service;
use tracing::{debug, error, info};

use mz_ore::netio::{Listener, SocketAddr, SocketAddrType};
use mz_proto::{ProtoType, RustType};

use crate::client::{GenericClient, Partitionable, Partitioned};

pub type ResponseStream<PR> = Pin<Box<dyn Stream<Item = Result<PR, Status>> + Send>>;

pub type ClientTransport = InterceptedService<Channel, VersionAttachInterceptor>;

/// A client to a remote dataflow server using gRPC and protobuf based
/// communication.
///
/// The client opens a connection using the proto client stubs that are
/// generated by tonic from a service definition. When the client is connected,
/// it will call automatically the only RPC defined in the service description,
/// encapsulated by the `BidiProtoClient` trait. This trait bound is not on the
/// `Client` type parameter here, but it IS on the impl blocks. Bidirectional
/// protobuf RPC sets up two streams that persist after the RPC has returned: A
/// Request (Command) stream (for us, backed by a unbounded mpsc queue) going
/// from this instance to the server and a response stream coming back
/// (represented directly as a `Streaming<Response>` instance). The recv and send
/// functions interact with the two mpsc channels or the streaming instance
/// respectively.
#[derive(Debug)]
pub struct GrpcClient<G>
where
    G: BidiProtoClient,
{
    /// The sender for commands.
    tx: UnboundedSender<G::PC>,
    /// The receiver for responses.
    rx: Streaming<G::PR>,
}

impl<G> GrpcClient<G>
where
    G: BidiProtoClient,
{
    /// Connects to the server at the given address, announcing the specified
    /// client version.
    pub async fn connect(addr: String, version: Version) -> Result<Self, anyhow::Error> {
        debug!("GrpcClient {}: Attempt to connect", addr);

        let channel = match SocketAddrType::guess(&addr) {
            SocketAddrType::Inet => Endpoint::new(format!("http://{}", addr))?.connect().await?,
            SocketAddrType::Unix => {
                let addr = addr.clone();
                Endpoint::from_static("http://localhost") // URI is ignored
                    .connect_with_connector(tower::service_fn(move |_| {
                        UnixStream::connect(addr.clone())
                    }))
                    .await?
            }
        };
        let service = InterceptedService::new(channel, VersionAttachInterceptor::new(version));
        let mut client = G::new(service);
        let (tx, rx) = mpsc::unbounded_channel();
        let rx = client
            .establish_bidi_stream(UnboundedReceiverStream::new(rx))
            .await?
            .into_inner();
        info!("GrpcClient {}: connected", &addr);
        Ok(GrpcClient { tx, rx })
    }

    /// Like [`GrpcClient::connect`], but for multiple partitioned servers.
    pub async fn connect_partitioned<C, R>(
        addrs: Vec<String>,
        version: Version,
    ) -> Result<Partitioned<Self, C, R>, anyhow::Error>
    where
        (C, R): Partitionable<C, R>,
    {
        let clients = future::try_join_all(
            addrs
                .into_iter()
                .map(|addr| Self::connect(addr, version.clone())),
        )
        .await?;
        Ok(Partitioned::new(clients))
    }
}

#[async_trait]
impl<G, C, R> GenericClient<C, R> for GrpcClient<G>
where
    C: RustType<G::PC> + Send + Sync + 'static,
    R: RustType<G::PR> + Send + Sync + 'static,
    G: BidiProtoClient,
{
    async fn send(&mut self, cmd: C) -> Result<(), anyhow::Error> {
        self.tx.send(cmd.into_proto())?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<R>, anyhow::Error> {
        match self.rx.try_next().await? {
            None => Ok(None),
            Some(response) => Ok(Some(response.into_rust()?)),
        }
    }
}

/// Encapsulates the core functionality of a tonic gRPC client for a service
/// that exposes a single bidirectional RPC stream.
///
/// See the documentation on [`GrpcClient`] for details.
//
// TODO(guswynn): if tonic ever presents the client API as a trait, use it
// instead of requiring an implementation of this trait.
#[async_trait]
pub trait BidiProtoClient: Debug + Send {
    type PC: Debug + Send + Sync + 'static;
    type PR: Debug + Send + Sync + 'static;

    fn new(inner: ClientTransport) -> Self
    where
        Self: Sized;

    async fn establish_bidi_stream(
        &mut self,
        rx: UnboundedReceiverStream<Self::PC>,
    ) -> Result<Response<Streaming<Self::PR>>, Status>;
}

/// A gRPC server that stitches a gRPC service with a single bidirectional
/// stream to a [`GenericClient`].
///
/// It is the counterpart of [`GrpcClient`].
///
/// To use, implement the tonic-generated `ProtoService` trait for this type.
/// The implementation of the bidirectional stream method should call
/// [`GrpcServer::forward_bidi_stream`] to stitch the bidirectional stream to
/// the client underlying this server.
pub struct GrpcServer<F> {
    state: Arc<GrpcServerState<F>>,
}

struct GrpcServerState<F> {
    cancel_tx: Mutex<oneshot::Sender<()>>,
    client_builder: F,
}

impl<F, G> GrpcServer<F>
where
    F: Fn() -> G + Send + Sync + 'static,
{
    /// Starts the server, listening for gRPC connections on `listen_addr`.
    ///
    /// The trait bounds on `S` are intimidating, but it is the return type of
    /// `service_builder`, which is a function that
    /// turns a `GrpcServer<ProtoCommandType, ProtoResponseType>` into a
    /// [`Service`] that represents a gRPC server. This is always encapsulated
    /// by the tonic-generated `ProtoServer::new` method for a specific Protobuf
    /// service.
    pub async fn serve<S, Fs>(
        listen_addr: SocketAddr,
        version: Version,
        client_builder: F,
        service_builder: Fs,
    ) -> Result<(), anyhow::Error>
    where
        S: Service<
                http::Request<Body>,
                Response = http::Response<BoxBody>,
                Error = std::convert::Infallible,
            > + NamedService
            + Clone
            + Send
            + 'static,
        S::Future: Send + 'static,
        Fs: FnOnce(Self) -> S + Send + 'static,
    {
        let (cancel_tx, _cancel_rx) = oneshot::channel();
        let state = GrpcServerState {
            cancel_tx: Mutex::new(cancel_tx),
            client_builder,
        };
        let server = Self {
            state: Arc::new(state),
        };
        let service = InterceptedService::new(
            service_builder(server),
            VersionCheckExactInterceptor::new(version),
        );

        info!("Starting to listen on {}", listen_addr);
        let listener = Listener::bind(listen_addr).await?;

        Server::builder()
            .add_service(service)
            .serve_with_incoming(listener)
            .await?;
        Ok(())
    }

    /// Handles a bidirectional stream request by forwarding commands to and
    /// responses from the server's underlying client.
    ///
    /// Call this method from the implementation of the tonic-generated
    /// `ProtoService`.
    pub async fn forward_bidi_stream<C, R, PC, PR>(
        &self,
        request: Request<Streaming<PC>>,
    ) -> Result<Response<ResponseStream<PR>>, Status>
    where
        G: GenericClient<C, R> + 'static,
        C: RustType<PC> + Send + Sync + 'static + fmt::Debug,
        R: RustType<PR> + Send + Sync + 'static + fmt::Debug,
        PC: fmt::Debug + Send + Sync + 'static,
        PR: fmt::Debug + Send + Sync + 'static,
    {
        info!("GrpcServer: remote client connected");

        // Install our cancellation token. This may drop an existing
        // cancellation token. We're allowed to run until someone else drops our
        // cancellation token.
        //
        // TODO(benesch): rather than blindly dropping the existing cancellation
        // token, we should check epochs, and only drop the existing connection
        // if it is at a lower epoch.
        // See: https://github.com/MaterializeInc/materialize/issues/13377
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        *self.state.cancel_tx.lock().await = cancel_tx;

        // Construct a new client and forward commands and responses until
        // canceled.
        let mut request = request.into_inner();
        let state = Arc::clone(&self.state);
        let response = stream! {
            let mut client = (state.client_builder)();
            loop {
                select! {
                    command = request.next() => {
                        let command = match command {
                            None => break,
                            Some(Ok(command)) => command,
                            Some(Err(e)) => {
                                error!("error handling client: {e}");
                                break;
                            }
                        };
                        let command = match command.into_rust() {
                            Ok(command) => command,
                            Err(e) => {
                                error!("error converting command from protobuf: {}", e);
                                break;
                            }
                        };
                        if let Err(e) = client.send(command).await {
                            yield Err(Status::unknown(e.to_string()));
                        }
                    }
                    response = client.recv() => {
                        match response {
                            Ok(Some(response)) => yield Ok(response.into_proto()),
                            Ok(None) => break,
                            Err(e) => yield Err(Status::unknown(e.to_string())),
                        }
                    }
                    _ = &mut cancel_rx => break,
                }
            }
            info!("GrpcServer: remote client disconnected");
        };
        Ok(Response::new(Box::pin(response)))
    }
}

static VERSION_METADATA_KEY: Lazy<AsciiMetadataKey> =
    Lazy::new(|| AsciiMetadataKey::from_static("x-mz-version"));

/// A gRPC interceptor that attaches a version as metadata to each request.
#[derive(Debug, Clone)]
pub struct VersionAttachInterceptor {
    version: AsciiMetadataValue,
}

impl VersionAttachInterceptor {
    fn new(version: Version) -> VersionAttachInterceptor {
        VersionAttachInterceptor {
            version: version
                .to_string()
                .try_into()
                .expect("semver versions are valid metadata values"),
        }
    }
}

impl Interceptor for VersionAttachInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert(VERSION_METADATA_KEY.clone(), self.version.clone());
        Ok(request)
    }
}

/// A gRPC interceptor that ensures the version attached to the request by the
/// `VersionAttachInterceptor` exactly matches the expected version.
#[derive(Debug, Clone)]
struct VersionCheckExactInterceptor {
    version: AsciiMetadataValue,
}

impl VersionCheckExactInterceptor {
    fn new(version: Version) -> VersionCheckExactInterceptor {
        VersionCheckExactInterceptor {
            version: version
                .to_string()
                .try_into()
                .expect("semver versions are valid metadata values"),
        }
    }
}

impl Interceptor for VersionCheckExactInterceptor {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        match request.metadata().get(&*VERSION_METADATA_KEY) {
            None => Err(Status::permission_denied(
                "request missing version metadata",
            )),
            Some(version) if version == self.version => Ok(request),
            Some(version) => Err(Status::permission_denied(format!(
                "request version {:?} but {:?} required",
                version, self.version
            ))),
        }
    }
}
