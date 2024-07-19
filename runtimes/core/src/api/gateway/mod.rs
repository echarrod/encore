mod router;

use std::borrow::Cow;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::{BufMut, Bytes, BytesMut};
use http::header::SEC_WEBSOCKET_PROTOCOL;
use hyper::header;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::protocols::http::error_resp;
use pingora::proxy::{http_proxy_service, ProxyHttp, Session};
use pingora::server::configuration::{Opt, ServerConf};
use pingora::services::Service;
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorSource, ErrorType};
use serde::Deserialize;
use tokio::sync::watch;
use url::Url;

use crate::api::auth;
use crate::api::call::{CallDesc, ServiceRegistry};
use crate::api::paths::PathSet;
use crate::api::reqauth::caller::Caller;
use crate::api::reqauth::{svcauth, CallMeta};
use crate::api::schema::Method;
use crate::{api, model, EncoreName};

use super::cors::cors_headers_config::CorsHeadersConfig;
use super::encore_routes::healthz;

#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Arc<SharedGatewayData>,
    service_registry: Arc<ServiceRegistry>,
    router: router::Router,
    cors_config: CorsHeadersConfig,
    healthz: healthz::Handler,
    own_api_address: Option<SocketAddr>,
}

pub struct GatewayCtx {
    upstream_service_name: EncoreName,
    upstream_base_path: String,
    upstream_host: Option<String>,
}

impl GatewayCtx {
    fn prepend_base_path(&self, uri: &http::Uri) -> anyhow::Result<http::Uri> {
        let mut builder = http::Uri::builder();
        if let Some(scheme) = uri.scheme() {
            builder = builder.scheme(scheme.clone());
        }
        if let Some(authority) = uri.authority() {
            builder = builder.authority(authority.clone());
        }

        let base_path = self.upstream_base_path.trim_end_matches('/');
        builder = builder.path_and_query(format!(
            "{}{}",
            base_path,
            uri.path_and_query().map_or("", |pq| pq.as_str())
        ));

        builder.build().context("failed to build uri")
    }
}

impl Gateway {
    pub fn new(
        name: EncoreName,
        service_registry: Arc<ServiceRegistry>,
        service_routes: PathSet<EncoreName, Arc<api::Endpoint>>,
        auth_handler: Option<auth::Authenticator>,
        cors_config: CorsHeadersConfig,
        healthz: healthz::Handler,
        own_api_address: Option<SocketAddr>,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(SharedGatewayData {
            name,
            auth: auth_handler,
        });

        let mut router = router::Router::new();
        for (svc, routes) in [&service_routes.main, &service_routes.fallback]
            .into_iter()
            .flatten()
        {
            router.add_routes(svc, routes)?;
        }

        Ok(Gateway {
            inner: Arc::new(Inner {
                shared,
                service_registry,
                router,
                cors_config,
                healthz,
                own_api_address,
            }),
        })
    }

    pub fn auth_handler(&self) -> Option<&auth::Authenticator> {
        self.inner.shared.auth.as_ref()
    }

    pub async fn serve(self, listen_addr: &str) -> anyhow::Result<()> {
        let conf = Arc::new(
            ServerConf::new_with_opt_override(&Opt {
                upgrade: false,
                daemon: false,
                nocapture: false,
                test: false,
                conf: None,
            })
            .unwrap(),
        );
        let mut proxy = http_proxy_service(&conf, self);

        proxy.add_tcp(listen_addr);

        let (_tx, rx) = watch::channel(false);
        proxy.start_service(None, rx).await;

        Ok(())
    }
}

#[async_trait]
impl ProxyHttp for Gateway {
    type CTX = Option<GatewayCtx>;

    fn new_ctx(&self) -> Self::CTX {
        None
    }

    // see https://github.com/cloudflare/pingora/blob/main/docs/user_guide/internals.md for
    // details on when different filters are called.

    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora::Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        if session.req_header().uri.path() == "/__encore/healthz" {
            let healthz_resp = self.inner.healthz.clone().health_check();
            let healthz_bytes: Vec<u8> = serde_json::to_vec(&healthz_resp).map_err(|e| {
                Error::because(ErrorType::HTTPStatus(500), "could not encode response", e)
            })?;

            let mut header = ResponseHeader::build(200, None)?;
            header.insert_header(header::CONTENT_LENGTH, healthz_bytes.len())?;
            header.insert_header(header::CONTENT_TYPE, "application/json")?;
            session.write_response_header(Box::new(header)).await?;
            session
                .write_response_body(Bytes::from(healthz_bytes))
                .await?;

            return Ok(true);
        }

        // preflight request, return early with cors headers
        if axum::http::Method::OPTIONS == session.req_header().method {
            let mut resp = ResponseHeader::build(200, None)?;
            self.inner
                .cors_config
                .apply(session.req_header(), &mut resp)?;
            resp.insert_header(header::CONTENT_LENGTH, 0)?;
            session.write_response_header(Box::new(resp)).await?;

            return Ok(true);
        }

        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<HttpPeer>> {
        let path = session.req_header().uri.path();

        if let Some(own_api_addr) = &self.inner.own_api_address {
            if path.starts_with("/__encore/") {
                return Ok(Box::new(HttpPeer::new(own_api_addr, false, "".to_string())));
            }
        }

        let method: Method = session
            .req_header()
            .method
            .as_ref()
            .try_into()
            .map_err(|e| Error::because(ErrorType::HTTPStatus(400), "invalid http method", e))?;

        let service_name = self.inner.router.route_to_service(method, path)?;

        let upstream = self
            .inner
            .service_registry
            .service_base_url(service_name)
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "couldn't find upstream"))?;

        let upstream_url: Url = upstream
            .parse()
            .map_err(|e| Error::because(ErrorType::InternalError, "upstream not a valid url", e))?;

        let upstream_addrs = upstream_url
            .socket_addrs(|| match upstream_url.scheme() {
                "https" => Some(443),
                "http" => Some(80),
                _ => None,
            })
            .map_err(|e| {
                Error::because(
                    ErrorType::InternalError,
                    "couldn't lookup upstream ip address",
                    e,
                )
            })?;

        let upstream_addr = upstream_addrs.first().ok_or_else(|| {
            Error::explain(
                ErrorType::InternalError,
                "didn't find any upstream ip addresses",
            )
        })?;

        let tls = upstream_url.scheme() == "https";
        let host = upstream_url.host().map(|h| h.to_string());
        let peer = HttpPeer::new(upstream_addr, tls, host.clone().unwrap_or_default());

        ctx.replace(GatewayCtx {
            upstream_base_path: upstream_url.path().to_string(),
            upstream_host: host,
            upstream_service_name: service_name.clone(),
        });

        Ok(Box::new(peer))
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        if ctx.is_some() {
            self.inner
                .cors_config
                .apply(session.req_header(), upstream_response)?;
        }

        Ok(())
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        if let Some(gateway_ctx) = ctx.as_ref() {
            let new_uri = gateway_ctx
                .prepend_base_path(&upstream_request.uri)
                .map_err(|e| {
                    Error::because(
                        ErrorType::InternalError,
                        "failed to prepend upstream base path",
                        e,
                    )
                })?;

            upstream_request.set_uri(new_uri);

            // Do we need to set the host header here?
            // It means the upstream service won't be able to tell
            // what the original Host header was, which is sometimes useful.
            if let Some(ref host) = gateway_ctx.upstream_host {
                upstream_request.insert_header(header::HOST, host)?;
            }

            if session.is_upgrade_req() {
                update_request_from_websocket_protocol(upstream_request).map_err(|e| {
                    Error::because(
                        ErrorType::UnknownError,
                        "failed parsing websocket protocol header",
                        e,
                    )
                })?;
            }

            let svc_auth_method = self
                .inner
                .service_registry
                .service_auth_method(&gateway_ctx.upstream_service_name)
                .unwrap_or_else(|| Arc::new(svcauth::Noop));

            let headers = &upstream_request.headers;

            let mut call_meta = CallMeta::parse_without_caller(headers).map_err(|e| {
                Error::because(
                    ErrorType::InternalError,
                    "couldn't parse CallMeta from request",
                    e,
                )
            })?;
            if call_meta.parent_span_id.is_none() {
                call_meta.parent_span_id = Some(model::SpanId::generate());
            }

            let caller = Caller::Gateway {
                gateway: self.inner.shared.name.clone(),
            };
            let mut desc = CallDesc {
                caller: &caller,
                parent_span: call_meta
                    .parent_span_id
                    .map(|sp| call_meta.trace_id.with_span(sp)),
                parent_event_id: None,
                ext_correlation_id: call_meta
                    .ext_correlation_id
                    .as_ref()
                    .map(|s| Cow::Borrowed(s.as_str())),
                auth_user_id: None,
                auth_data: None,
                svc_auth_method: svc_auth_method.as_ref(),
            };

            if let Some(auth_handler) = &self.inner.shared.auth {
                let auth_response = auth_handler
                    .authenticate(upstream_request, call_meta.clone())
                    .await
                    .map_err(|e| {
                        Error::because(ErrorType::InternalError, "couldn't authenticate request", e)
                    })?;

                if let auth::AuthResponse::Authenticated {
                    auth_uid,
                    auth_data,
                } = auth_response
                {
                    desc.auth_user_id = Some(Cow::Owned(auth_uid));
                    desc.auth_data = Some(auth_data);
                }
            }

            desc.add_meta(upstream_request).map_err(|e| {
                Error::because(ErrorType::InternalError, "couldn't set request meta", e)
            })?;
        }

        Ok(())
    }

    async fn fail_to_proxy(&self, session: &mut Session, e: &Error, _ctx: &mut Self::CTX) -> u16
    where
        Self::CTX: Send + Sync,
    {
        // modified version of `Session::respond_error` that adds cors headers,
        // and handles specific errors

        let code = match e.etype() {
            ErrorType::HTTPStatus(code) => *code,
            _ => {
                match e.esource() {
                    ErrorSource::Upstream => 502,
                    ErrorSource::Downstream => {
                        match e.etype() {
                            ErrorType::WriteError
                            | ErrorType::ReadError
                            | ErrorType::ConnectionClosed => {
                                /* conn already dead */
                                return 0;
                            }
                            _ => 400,
                        }
                    }
                    ErrorSource::Internal | ErrorSource::Unset => 500,
                }
            }
        };

        let (mut resp, body) = if let Some(api_error) = as_api_error(e) {
            let (resp, body) = api_error_response(api_error);
            (resp, Some(body))
        } else {
            (
                match code {
                    /* common error responses are pre-generated */
                    502 => error_resp::HTTP_502_RESPONSE.clone(),
                    400 => error_resp::HTTP_400_RESPONSE.clone(),
                    _ => error_resp::gen_error_response(code),
                },
                None,
            )
        };

        if let Err(e) = self
            .inner
            .cors_config
            .apply(session.req_header(), &mut resp)
        {
            log::error!("failed setting cors header in error response: {e}");
        }
        session.set_keepalive(None);
        session
            .write_response_header(Box::new(resp))
            .await
            .unwrap_or_else(|e| {
                log::error!("failed to send error response to downstream: {e}");
            });

        if let Some(body) = body {
            session
                .write_response_body(body)
                .await
                .unwrap_or_else(|e| log::error!("failed to write body: {e}"));
        }

        code
    }
}

#[derive(Deserialize)]
struct AuthHeaders(HashMap<String, String>);

const AUTH_DATA_PREFIX: &str = "encore.dev.auth_data.";

// hack to be able to have browsers send request headers when setting up a websocket
// inspired by https://github.com/kubernetes/kubernetes/commit/714f97d7baf4975ad3aa47735a868a81a984d1f0
fn update_request_from_websocket_protocol(
    upstream_request: &mut RequestHeader,
) -> anyhow::Result<()> {
    let headers = upstream_request
        .headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();

    if upstream_request
        .remove_header(&SEC_WEBSOCKET_PROTOCOL)
        .is_none()
    {
        return Ok(());
    }

    for header_value in headers {
        let mut filterd_protocols = Vec::new();

        for protocol in header_value.to_str()?.split(',') {
            let protocol = protocol.trim();
            if protocol.starts_with(AUTH_DATA_PREFIX) {
                let data = protocol.strip_prefix(AUTH_DATA_PREFIX).unwrap();
                let decoded = URL_SAFE_NO_PAD.decode(data)?;
                let auth_data: AuthHeaders = serde_json::from_slice(&decoded)?;

                for (name, value) in auth_data.0 {
                    // TODO: error on headers that are not allowed to be set
                    upstream_request.append_header(name, value)?;
                }
            } else {
                filterd_protocols.push(protocol);
            }
        }

        if !filterd_protocols.is_empty() {
            upstream_request.append_header(SEC_WEBSOCKET_PROTOCOL, filterd_protocols.join(", "))?;
        }
    }

    Ok(())
}

fn as_api_error(err: &pingora::Error) -> Option<&api::Error> {
    if let Some(cause) = &err.cause {
        cause.downcast_ref::<api::Error>()
    } else {
        None
    }
}

fn api_error_response(err: &api::Error) -> (ResponseHeader, bytes::Bytes) {
    let mut buf = BytesMut::with_capacity(128).writer();
    serde_json::to_writer(&mut buf, &err).unwrap();

    let mut resp = ResponseHeader::build(err.code.status_code(), Some(5)).unwrap();
    resp.insert_header(header::SERVER, &pingora::protocols::http::SERVER_NAME[..])
        .unwrap();
    resp.insert_header(header::DATE, "Sun, 06 Nov 1994 08:49:37 GMT")
        .unwrap(); // placeholder
    resp.insert_header(header::CONTENT_LENGTH, buf.get_ref().len())
        .unwrap();
    resp.insert_header(header::CACHE_CONTROL, "private, no-store")
        .unwrap();
    resp.insert_header(header::CONTENT_TYPE, mime::APPLICATION_JSON.as_ref())
        .unwrap();

    (resp, buf.into_inner().into())
}

impl crate::api::auth::InboundRequest for RequestHeader {
    fn headers(&self) -> &axum::http::HeaderMap {
        &self.headers
    }

    fn query(&self) -> Option<&str> {
        self.uri.query()
    }
}

struct SharedGatewayData {
    name: EncoreName,
    auth: Option<auth::Authenticator>,
}
