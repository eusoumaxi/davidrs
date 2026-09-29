//! Authorization and admission: the two steps an application supplies.
//!
//! Both are async because both can need I/O (a key fetch, a rate-limit
//! round trip). Decoding stays synchronous: it only reads bytes already in
//! memory.

use std::future::Future;

use super::failure::Failure;
use super::request::Request;
use crate::Invocation;

/// Decides who is calling and establishes the scope a handler runs with.
///
/// The returned `Scope` is evidence: it exists only because this policy
/// succeeded. Keep its fields private and its constructors restricted, so no
/// other code can fabricate one.
///
/// # Examples
///
/// ```
/// use davidrs::http::{Failure, Policy, Request, StatusCode};
/// use davidrs::Invocation;
///
/// pub struct Caller {
///     user: String,
/// }
///
/// struct UserHeader;
///
/// impl Policy for UserHeader {
///     type Scope = Caller;
///
///     async fn authorize(&self, request: &Request<'_>, _: &Invocation) -> Result<Caller, Failure> {
///         match request.header("x-user") {
///             Some(user) if !user.is_empty() => Ok(Caller { user: user.to_owned() }),
///             _ => Err(Failure::new(StatusCode::UNAUTHORIZED, "ERROR_UNAUTHORIZED", "Sign in first")),
///         }
///     }
/// }
/// ```
pub trait Policy: Send + Sync + 'static {
    /// What a successful authorization establishes.
    type Scope: Send + 'static;

    /// Authorizes the request, after it has been decoded.
    ///
    /// # Errors
    ///
    /// Returns a [`Failure`] (usually a `401` or `403`) that the pipeline
    /// renders without calling the handler.
    fn authorize(
        &self,
        request: &Request<'_>,
        invocation: &Invocation,
    ) -> impl Future<Output = Result<Self::Scope, Failure>> + Send;
}

/// A policy for public endpoints: everyone is admitted and the scope is `()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Public;

impl Policy for Public {
    type Scope = ();

    async fn authorize(
        &self,
        _request: &Request<'_>,
        _invocation: &Invocation,
    ) -> Result<(), Failure> {
        Ok(())
    }
}

/// A check that runs before the body is read: rate limits, quotas, anything
/// that should refuse a request before any parsing.
///
/// It returns the headers to add to the response, so a rate limiter reports
/// its budget on a success as well as on a refusal. Each name replaces the
/// handler's header of the same name; a name returned more than once, such as
/// `set-cookie`, keeps every value.
pub trait Admission: Send + Sync + 'static {
    /// Admits the request with headers for the response, or refuses it.
    ///
    /// # Errors
    ///
    /// Returns a [`Failure`] when the request must not proceed. Attach the
    /// budget headers to it with [`Failure::with_headers`] so a `429` still
    /// reports them.
    fn check(
        &self,
        request: &Request<'_>,
        invocation: &Invocation,
    ) -> impl Future<Output = Result<Vec<(String, String)>, Failure>> + Send;
}

/// Admits every request and adds no headers; the default admission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdmitAll;

impl Admission for AdmitAll {
    async fn check(
        &self,
        _request: &Request<'_>,
        _invocation: &Invocation,
    ) -> Result<Vec<(String, String)>, Failure> {
        Ok(Vec::new())
    }
}
