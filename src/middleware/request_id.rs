use actix_web::{
    dev::{Payload, Service, ServiceRequest, ServiceResponse, Transform},
    Error, FromRequest, HttpMessage, HttpRequest,
};
use std::future::{ready, Future, Ready};
use std::pin::Pin;
use std::task::{Context, Poll};

/// A request ID assigned to every incoming request.
#[derive(Debug, Clone)]
pub struct RequestId {
    pub id: String,
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestId {
    pub fn new() -> Self {
        let uuid = uuid::Uuid::new_v4().simple().to_string();
        Self {
            id: format!("req_{}", &uuid[..12]),
        }
    }
}

impl FromRequest for RequestId {
    type Error = Error;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let id = req
            .extensions()
            .get::<RequestId>()
            .cloned()
            .unwrap_or_else(RequestId::new);
        ready(Ok(id))
    }
}

/// Middleware factory that generates a unique request ID for each request.
pub struct RequestIdMiddleware;

impl<S, B> Transform<S, ServiceRequest> for RequestIdMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = RequestIdMiddlewareService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(RequestIdMiddlewareService { service }))
    }
}

pub struct RequestIdMiddlewareService<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for RequestIdMiddlewareService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let req_id = RequestId::new();
        req.extensions_mut().insert(req_id.clone());

        let fut = self.service.call(req);
        let id = req_id.id;

        Box::pin(async move {
            let mut res = fut.await?;
            res.headers_mut().insert(
                actix_web::http::header::HeaderName::from_static("x-request-id"),
                actix_web::http::header::HeaderValue::from_str(&id).unwrap_or_else(|_| {
                    actix_web::http::header::HeaderValue::from_static("unknown")
                }),
            );
            Ok(res)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_id_starts_with_req_prefix() {
        let rid = RequestId::new();
        assert!(
            rid.id.starts_with("req_"),
            "request ID must start with 'req_', got: {}",
            rid.id
        );
    }

    #[test]
    fn new_id_has_correct_total_length() {
        let rid = RequestId::new();
        // "req_" (4) + 12 hex chars = 16
        assert_eq!(
            rid.id.len(),
            16,
            "expected length 16, got {} for id={}",
            rid.id.len(),
            rid.id
        );
    }

    #[test]
    fn new_id_suffix_is_hex() {
        let rid = RequestId::new();
        let suffix = &rid.id[4..]; // strip "req_"
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "suffix must be hex chars, got: {suffix}"
        );
    }

    #[test]
    fn successive_ids_are_unique() {
        let id1 = RequestId::new().id;
        let id2 = RequestId::new().id;
        assert_ne!(id1, id2, "successive request IDs must be unique");
    }

    #[test]
    fn default_impl_produces_valid_id() {
        let rid = RequestId::default();
        assert!(rid.id.starts_with("req_"));
        assert_eq!(rid.id.len(), 16);
    }
}
