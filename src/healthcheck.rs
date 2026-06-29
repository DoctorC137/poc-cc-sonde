use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use std::convert::Infallible;
use std::net::{SocketAddr, TcpListener};
use tracing::{error, info};

async fn handle_request(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("Probe is running"))
        .unwrap())
}

pub fn bind_healthcheck_server(host: &str, port: u16) -> Result<TcpListener, std::io::Error> {
    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .expect("Invalid healthcheck bind address");
    TcpListener::bind(addr)
}

pub async fn serve_healthcheck(listener: TcpListener) {
    let addr = listener.local_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
    let make_svc =
        make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(handle_request)) });

    let server = match Server::from_tcp(listener) {
        Ok(builder) => builder.serve(make_svc),
        Err(e) => {
            error!(error = %e, "Failed to create health check server from listener");
            return;
        }
    };

    info!(
        address = %addr,
        "Health check server started"
    );

    if let Err(e) = server.await {
        error!(
            error = %e,
            "Health check server error"
        );
    }
}
