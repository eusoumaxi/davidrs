//! Invocation metadata and the absolute deadline every adapter carries.
//!
//! [`Deadline`] is a value type and works with the empty crate. Only
//! [`Deadline::run`] needs a timer, so it exists with `runtime` and the other
//! features that await under a budget.

use std::time::{Duration, Instant};

/// Identity and timing of one Lambda invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Invocation {
    /// The Lambda request id.
    pub request_id: String,
    /// The raw `X-Amzn-Trace-Id` header, when the platform supplied one.
    pub trace_id: Option<String>,
    /// The invoked function ARN, when available.
    pub invoked_arn: Option<String>,
    /// The tenant Lambda isolates this invocation for, on a function that
    /// uses tenant isolation mode; `None` otherwise.
    ///
    /// It comes from the invoke request's tenant id, which the caller of
    /// `Invoke` chose: authorize the caller before trusting it as a tenant.
    pub tenant_id: Option<String>,
    /// The absolute budget for this invocation.
    pub deadline: Deadline,
    /// When the adapter received this invocation.
    ///
    /// A handler that honours a client-supplied time limit, such as a long
    /// poll's wait, measures it from here: the limit covers the whole
    /// request, including the admission and decoding that ran before the
    /// handler.
    pub started: Instant,
}

impl Invocation {
    /// The `Root=` id of the platform's trace header, or `None`.
    ///
    /// The full header also carries `Parent` and `Sampled`, which describe one
    /// hop rather than the trace. The root alone is what a client needs to find
    /// the trace, so it is what belongs in a response header.
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use davidrs::{Deadline, Invocation};
    /// let invocation = Invocation::new("r", Deadline::after(Duration::from_secs(1)))
    ///     .with_trace_id(Some("Root=1-abc;Parent=def;Sampled=1".to_owned()));
    /// assert_eq!(invocation.trace_root(), Some("1-abc"));
    /// ```
    #[must_use]
    pub fn trace_root(&self) -> Option<&str> {
        self.trace_id.as_deref().and_then(|header| {
            header
                .split(';')
                .find_map(|part| part.trim().strip_prefix("Root="))
        })
    }

    /// Invocation metadata with no trace header, ARN or tenant, received now.
    #[must_use]
    pub fn new(request_id: impl Into<String>, deadline: Deadline) -> Self {
        Self {
            request_id: request_id.into(),
            trace_id: None,
            invoked_arn: None,
            tenant_id: None,
            deadline,
            started: Instant::now(),
        }
    }

    /// How long ago this invocation was received.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Sets the raw trace header.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: Option<String>) -> Self {
        self.trace_id = trace_id;
        self
    }

    /// Sets the invoked function ARN.
    #[must_use]
    pub fn with_invoked_arn(mut self, arn: Option<String>) -> Self {
        self.invoked_arn = arn;
        self
    }

    /// Sets the tenant id of a tenant-isolated invocation.
    #[must_use]
    pub fn with_tenant_id(mut self, tenant_id: Option<String>) -> Self {
        self.tenant_id = tenant_id;
        self
    }
}

/// An absolute point in time after which work must stop.
///
/// Built from a monotonic [`Instant`], so it is immune to wall-clock steps.
/// Child budgets are derived with [`Deadline::child`], never by restarting a
/// timer: a retry inside an invocation does not get a fresh allowance.
///
/// ```
/// use std::time::Duration;
/// use davidrs::Deadline;
///
/// let invocation = Deadline::after(Duration::from_secs(10));
/// let call = invocation.child(Duration::from_secs(60));
/// assert!(call <= invocation);
/// assert!(invocation.with_margin(Duration::from_secs(1)) < invocation);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline {
    at: Instant,
}

impl Deadline {
    /// A deadline `budget` from now.
    #[must_use]
    pub fn after(budget: Duration) -> Self {
        Self {
            at: Instant::now() + budget,
        }
    }

    /// A deadline at an already-known instant.
    #[must_use]
    pub const fn at(at: Instant) -> Self {
        Self { at }
    }

    /// The instant this deadline expires.
    #[must_use]
    pub const fn instant(self) -> Instant {
        self.at
    }

    /// Time left, or [`Duration::ZERO`] once it has passed.
    #[must_use]
    pub fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    /// Whether the budget is gone.
    #[must_use]
    pub fn is_expired(self) -> bool {
        self.remaining().is_zero()
    }

    /// A deadline `margin` earlier than this one, never later.
    ///
    /// The margin is time kept for cleanup: saving state and releasing leases
    /// must still fit after the work budget ends.
    #[must_use]
    pub fn with_margin(self, margin: Duration) -> Self {
        Self {
            at: self.at.checked_sub(margin).unwrap_or(self.at),
        }
    }

    /// A child budget: `budget` from now, but never past the parent.
    #[must_use]
    pub fn child(self, budget: Duration) -> Self {
        Self {
            at: (Instant::now() + budget).min(self.at),
        }
    }

    /// The earlier of two deadlines.
    ///
    /// A named alias for [`Ord::min`], which reads as the wrong operation on a
    /// budget: the *smaller* deadline is the *earlier* one.
    #[must_use]
    pub fn earliest(self, other: Self) -> Self {
        self.min(other)
    }
}

/// What a handler receives besides its own input: the invocation, and the
/// scope its caller was authorized with.
///
/// `Scope` is whatever the HTTP `Policy` established, and `()` for triggers
/// and routes with no policy data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context<Scope> {
    invocation: Invocation,
    scope: Scope,
}

impl<Scope> Context<Scope> {
    /// Pairs a scope with its invocation.
    #[must_use]
    pub fn new(invocation: Invocation, scope: Scope) -> Self {
        Self { invocation, scope }
    }

    /// Borrows the data the policy established.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Borrows invocation identity and timing.
    pub fn invocation(&self) -> &Invocation {
        &self.invocation
    }

    /// The absolute budget for this invocation.
    pub fn deadline(&self) -> Deadline {
        self.invocation.deadline
    }

    /// Consumes the context, returning the owned scope.
    pub fn into_scope(self) -> Scope {
        self.scope
    }
}

#[cfg(feature = "_time")]
impl Deadline {
    /// Runs `work` under this deadline.
    ///
    /// When the budget runs out first, `work` is dropped at its next await
    /// point: anything that must survive cancellation belongs outside it. An
    /// expired deadline does not start `work` at all.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::DeadlineExceeded`](crate::RuntimeError::DeadlineExceeded)
    /// when the budget runs out before `work` completes.
    pub async fn run<F, T>(self, work: F) -> Result<T, crate::RuntimeError>
    where
        F: std::future::Future<Output = T>,
    {
        let started = Instant::now();
        if self.is_expired() {
            return Err(crate::RuntimeError::DeadlineExceeded {
                elapsed: Duration::ZERO,
            });
        }
        match tokio::time::timeout(self.remaining(), work).await {
            Ok(value) => Ok(value),
            Err(_) => Err(crate::RuntimeError::DeadlineExceeded {
                elapsed: started.elapsed(),
            }),
        }
    }
}
