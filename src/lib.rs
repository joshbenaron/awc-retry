use actix_service::Service;
use actix_web::body::Body;
use awc::error::{ConnectError, SendRequestError};
use awc::middleware::Transform;
use awc::{ClientResponse, ConnectRequest, ConnectResponse};
use bytes::Bytes;
use futures::future::{ok, LocalBoxFuture, Ready};
use futures::task::{Context, Poll};
use std::rc::Rc;
use actix_http::RequestHeadType;
use actix_web::dev::{RequestHead, ResponseHead};
use actix_http::http::StatusCode;
use actix_web::guard::Guard;
use std::ops::Deref;
use std::borrow::Borrow;

pub struct Retry(Inner);

struct Inner {
    /// Number of retries. So each request will be tried [max_retries + 1] times
    max_retries: u8,
    policies: Vec<RetryPolicy>
}

impl Inner {
    pub fn is_valid_response(&self, head: &ResponseHead) -> bool {
        self.policies.iter().all(|policy| {
            match policy {
                RetryPolicy::Status(v) => {
                    true
                }
                RetryPolicy::Custom(func) => {
                    (func.deref())(head)
                }
            }
        })
    }
}

impl Retry {
    pub fn new(retries: u8) -> Self {
        Retry(Inner {
            max_retries: retries,
            policies: vec![]
        })
    }

    pub fn policy<T>(mut self, p: T) -> Self
        where T: IntoRetryPolicy
    {
        self.0.policies.push(p.into_policy());
        self
    }

}

#[non_exhaustive]
pub enum RetryPolicy {
    Status(Vec<StatusCode>),
    Custom(Box<dyn Fn(&ResponseHead) -> bool>)
}

pub trait IntoRetryPolicy {
    fn into_policy(self) -> RetryPolicy;
}

impl<T> IntoRetryPolicy for T
    where T: for<'a> Fn(&'a ResponseHead) -> bool + 'static
{
    fn into_policy(self) -> RetryPolicy {
        RetryPolicy::Custom(Box::new(self))
    }
}

impl IntoRetryPolicy for Vec<StatusCode> {
    fn into_policy(self) -> RetryPolicy {
        RetryPolicy::Status(self)
    }
}

impl<S> Transform<S, ConnectRequest> for Retry
    where
        S: Service<ConnectRequest, Response = ConnectResponse, Error = SendRequestError> + 'static,
{
    type Transform = RetryService<S>;

    fn new_transform(self, service: S) -> Self::Transform {
        RetryService {
            inner: Rc::new(self.0),
            connector: Rc::new(service),
        }
    }
}

pub struct RetryService<S> {
    inner: Rc<Inner>,
    connector: Rc<S>,
}

impl<S> Service<ConnectRequest> for RetryService<S>
    where
        S: Service<ConnectRequest, Response = ConnectResponse, Error = SendRequestError> + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connector.poll_ready(ctx)
    }

    fn call(&self, req: ConnectRequest) -> Self::Future {
        let connector = self.connector.clone();
        let inner = self.inner.clone();

        Box::pin(async move {
            let mut tries = 0;
            match req {
                ConnectRequest::Client(head, body, addr) => {
                    match body {
                        Body::Bytes(b) => {
                            println!("{}", "Bytes received");
                            loop {
                                let h = clone_request_head_type(&head);

                                match connector.call(ConnectRequest::Client(h, Body::Bytes(b.clone()), addr)).await
                                {
                                    Ok(res) => {
                                        // ConnectResponse
                                        match &res {
                                            ConnectResponse::Client(ref r) => {
                                                // TODO: Need to work out how to get the ResponseHead
                                                if inner.is_valid_response(&ResponseHead::new(StatusCode::OK)) {
                                                    return Ok(res)
                                                } else {
                                                    tries += 1;
                                                }
                                            },
                                            ConnectResponse::Tunnel(ref head, _) => {
                                                if inner.is_valid_response(head) {
                                                    tries += 1;
                                                } else {
                                                    tries += 1;
                                                }
                                            }
                                        };

                                        return Ok(res)
                                    },
                                    // SendRequestError
                                    Err(e) => {
                                        if tries == inner.max_retries {
                                            return Err(e);
                                        } else {
                                            tries += 1;
                                        }
                                    }
                                }
                            }
                        },
                        Body::Empty => {
                            loop {
                                let h = clone_request_head_type(&head);

                                match connector.call(ConnectRequest::Client(h, Body::Empty, addr)).await
                                {
                                    Ok(res) => return Ok(res),
                                    Err(e) => {
                                        println!("{}", e);
                                        if tries == inner.max_retries {
                                            return Err(e);
                                        } else {
                                            tries += 1;
                                        }
                                    }
                                }
                            }
                        },
                        _ => {
                            loop {
                                let h = clone_request_head_type(&head);

                                match connector.call(ConnectRequest::Client(h, Body::None, addr)).await
                                {
                                    Ok(res) => {
                                        /// This is [ConnectResponse]
                                        return Ok(res)
                                    },
                                    Err(e) => {
                                        if tries == inner.max_retries {
                                            return Err(e);
                                        } else {
                                            tries += 1;
                                        }
                                    }
                                }
                            }
                        }


                    }
                }
                ConnectRequest::Tunnel(head, addr) => {
                    match connector.call(ConnectRequest::Tunnel(head, addr)).await {
                        Ok(r) => Ok(r),
                        Err(e) => Err(e)
                    }
                }
            }
        })
    }
}

/// Clones [RequestHeadType] except for the extensions (not required for this middleware)
fn clone_request_head_type(head_type: &RequestHeadType) -> RequestHeadType {
    match head_type {
        RequestHeadType::Owned(h) => {
            let mut inner_head = RequestHead::default();
            inner_head.uri = h.uri.clone();
            inner_head.method = h.method.clone();
            inner_head.version = h.version.clone();
            inner_head.peer_addr = h.peer_addr.clone();
            inner_head.headers = h.headers.clone();

            RequestHeadType::Owned(inner_head)
        },
        RequestHeadType::Rc(h, header_map) => {
            RequestHeadType::Rc(h.clone(), header_map.clone())
        }
    }
}
