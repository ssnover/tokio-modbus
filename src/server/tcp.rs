use crate::{
    codec,
    frame::*,
    server::service::{NewService, Service},
};

use futures::{self, future, select, Future};
use futures_util::{future::FutureExt, sink::SinkExt, stream::StreamExt};
use log::{error, trace};
use socket2::{Domain, Socket, Type};
use std::{
    io::{self, Error},
    net::SocketAddr,
    sync::Arc,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::Framed;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    socket_addr: SocketAddr,
    threads: Option<usize>,
}

impl Server {
    /// Set the address for the server (mandatory).
    pub fn new(socket_addr: SocketAddr) -> Self {
        Self {
            socket_addr,
            threads: None,
        }
    }

    /// Set the number of threads running simultaneous event loops (optional, Unix only).
    pub fn threads(mut self, threads: usize) -> Self {
        self.threads = Some(threads);
        self
    }

    /// Start a Modbus TCP server that blocks the current thread.
    pub fn serve<S>(self, service: S)
    where
        S: NewService<Request = Request, Response = Response> + Send + Sync + 'static,
        S::Request: From<Request>,
        S::Response: Into<Response>,
        S::Error: Into<Error>,
        S::Instance: Send + Sync + 'static,
    {
        self.serve_until(service, future::pending());
    }

    /// Start a Modbus TCP server that blocks the current thread.
    pub fn serve_until<S, Sd>(self, service: S, shutdown_signal: Sd)
    where
        S: NewService<Request = Request, Response = Response> + Send + Sync + 'static,
        Sd: Future<Output = ()> + Sync + Send + Unpin + 'static,
        S::Request: From<Request>,
        S::Response: Into<Response>,
        S::Error: Into<Error>,
        S::Instance: Send + Sync + 'static,
    {
        let mut server = Server::new(self.socket_addr);
        if let Some(threads) = self.threads {
            server = server.threads(threads);
        }
        serve_until(
            server.socket_addr,
            server.threads.unwrap_or(1),
            service,
            shutdown_signal,
        );
    }
}

/// Will start a TCP listener and will serve data with service provided
/// until shutdown signal will be triggered in shutdown_signal future
fn serve_until<S, Sd>(addr: SocketAddr, workers: usize, new_service: S, shutdown_signal: Sd)
where
    S: NewService<Request = Request, Response = Response> + Send + Sync + 'static,
    S::Error: Into<Error>,
    S::Instance: 'static + Send + Sync,
    Sd: Future<Output = ()> + Unpin + Send + Sync + 'static,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .build()
        .unwrap();

    let new_service = Arc::new(new_service);

    let server = async {
        let listener = listener(addr, workers).unwrap();

        loop {
            let (stream, _) = listener.accept().await?;
            let framed = Framed::new(stream, codec::tcp::ServerCodec::default());

            let new_service = new_service.clone();
            tokio::spawn(Box::pin(async move {
                let service = new_service.new_service().unwrap();
                let future = process(framed, service);

                if let Err(err) = future.await {
                    eprintln!("{:?}", err);
                }
            }));
        }

        // the only way found to specify the "task" future error type
        #[allow(unreachable_code)]
        io::Result::<()>::Ok(())
    };

    let mut server = Box::pin(server.fuse());
    let mut shutdown_signal = shutdown_signal.fuse();

    let task = async {
        select! {
            res = server => if let Err(e) = res { error!("error: {}", e) },
            _ = shutdown_signal => trace!("Shutdown signal received")
        }
    };

    rt.block_on(task);
}

/// The request-response loop spawned by serve_until for each client
async fn process<S>(
    framed: Framed<TcpStream, codec::tcp::ServerCodec>,
    service: S,
) -> io::Result<()>
where
    S: Service<Request = Request, Response = Response> + Send + Sync + 'static,
    S::Error: Into<Error>,
{
    let mut framed = framed;

    loop {
        let request = framed.next().await;

        // tcp socket closed
        if request.is_none() {
            break;
        }

        let request = request.unwrap()?;
        let hdr = request.hdr;
        let response = service.call(request.pdu.0).await.map_err(Into::into)?;

        framed
            .send(tcp::ResponseAdu {
                hdr,
                pdu: response.into(),
            })
            .await?;
    }
    Ok(())
}

/// Start TCP listener - configure and open TCP socket
fn listener(addr: SocketAddr, workers: usize) -> io::Result<TcpListener> {
    let listener = match addr {
        SocketAddr::V4(_) => Socket::new(Domain::IPV4, Type::STREAM, None)?,
        SocketAddr::V6(_) => Socket::new(Domain::IPV6, Type::STREAM, None)?,
    };
    configure_tcp(workers, &listener)?;
    listener.reuse_address()?;
    listener.bind(&addr.into())?;
    listener.listen(1024)?;
    TcpListener::from_std(listener.into())
}

#[cfg(unix)]
fn configure_tcp(workers: usize, tcp: &Socket) -> io::Result<()> {
    if workers > 1 {
        tcp.reuse_port()?;
    }
    Ok(())
}

#[cfg(windows)]
fn configure_tcp(_workers: usize, _tcp: &Socket) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Service;

    use futures::future;

    #[tokio::test]
    async fn service_wrapper() {
        #[derive(Clone)]
        struct DummyService {
            response: Response,
        }

        impl Service for DummyService {
            type Request = Request;
            type Response = Response;
            type Error = Error;
            type Future = future::Ready<Result<Self::Response, Self::Error>>;

            fn call(&self, _: Self::Request) -> Self::Future {
                future::ready(Ok(self.response.clone()))
            }
        }

        let service = DummyService {
            response: Response::ReadInputRegisters(vec![0x33]),
        };

        let pdu = Request::ReadInputRegisters(0, 1);
        let rsp_adu = service.call(pdu).await.unwrap();

        assert_eq!(rsp_adu, service.response);
    }
}
