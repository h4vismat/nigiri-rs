use std::{future::Future, time::Duration};

use tokio::time::Instant;

use crate::{FixtureError, diagnostics::redacted_tail};

pub(crate) struct Deadline {
    started: Instant,
    duration: Duration,
}

impl Deadline {
    pub(crate) fn new(duration: Duration) -> Result<Self, FixtureError> {
        if duration.is_zero() {
            return Err(FixtureError::InvalidConfiguration {
                detail: "startup deadline must be greater than zero".to_owned(),
            });
        }

        Ok(Self {
            started: Instant::now(),
            duration,
        })
    }

    /// The whole startup budget, independent of how much of it has been spent.
    pub(crate) fn budget(&self) -> Duration {
        self.duration
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.duration.saturating_sub(self.started.elapsed())
    }

    /// The remaining budget, or the readiness timeout it has already become.
    ///
    /// Callers that hand the remaining budget to another timeout cannot use a zero duration, and an
    /// exhausted budget is a readiness timeout rather than a configuration error.
    pub(crate) fn remaining_or_expired(
        &self,
        service: &'static str,
        last_observation: &str,
    ) -> Result<Duration, FixtureError> {
        let remaining = self.remaining();
        if remaining.is_zero() {
            return Err(self.readiness_timeout(service, last_observation));
        }

        Ok(remaining)
    }

    fn readiness_timeout(&self, service: &'static str, last_observation: &str) -> FixtureError {
        FixtureError::ReadinessTimeout {
            service,
            duration: self.duration,
            last_observation: redacted_tail(last_observation),
            diagnostics: String::new(),
        }
    }

    pub(crate) async fn run<T, F>(
        &self,
        service: &'static str,
        last_observation: &str,
        future: F,
    ) -> Result<T, FixtureError>
    where
        F: Future<Output = T>,
    {
        tokio::time::timeout(self.remaining(), future)
            .await
            .map_err(|_| self.readiness_timeout(service, last_observation))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::{advance, sleep};

    use super::Deadline;
    use crate::FixtureError;

    // Catches a regression that lets a zero startup budget create an immediately-expired fixture.
    #[test]
    fn zero_duration_is_rejected() {
        let error = match Deadline::new(Duration::ZERO) {
            Err(error) => error,
            Ok(_) => panic!("zero startup time must be invalid"),
        };

        assert!(matches!(error, FixtureError::InvalidConfiguration { .. }));
    }

    // Catches a regression that restarts the startup clock for each awaited operation.
    #[tokio::test(start_paused = true)]
    async fn run_uses_the_remaining_shared_budget_without_resetting_it() {
        let deadline =
            Deadline::new(Duration::from_secs(10)).expect("a positive deadline is valid");
        advance(Duration::from_secs(4)).await;

        let error = deadline
            .run("bitcoind", "waiting for root RPC", async {
                sleep(Duration::from_secs(7)).await;
            })
            .await
            .expect_err(
                "the operation must exhaust the six seconds remaining, not a new ten seconds",
            );

        let FixtureError::ReadinessTimeout {
            service,
            duration,
            last_observation,
            diagnostics,
        } = error
        else {
            panic!("deadline expiration must become a readiness timeout");
        };
        assert_eq!(service, "bitcoind");
        assert_eq!(duration, Duration::from_secs(10));
        assert_eq!(last_observation, "waiting for root RPC");
        assert!(diagnostics.is_empty());
        assert_eq!(deadline.remaining(), Duration::ZERO);
    }

    // Catches a regression that derives a caller-facing request timeout from the budget's residue, so
    // a client handed back after a slow startup would inherit a near-zero timeout.
    #[tokio::test(start_paused = true)]
    async fn the_whole_budget_is_reported_independently_of_what_remains() {
        let deadline =
            Deadline::new(Duration::from_secs(60)).expect("a positive deadline is valid");
        assert_eq!(deadline.budget(), Duration::from_secs(60));

        advance(Duration::from_secs(59)).await;

        assert_eq!(deadline.remaining(), Duration::from_secs(1));
        assert_eq!(deadline.budget(), Duration::from_secs(60));
    }

    // Catches a regression that reports an exhausted budget as a configuration error rather than the
    // readiness timeout it already is, which would misclassify a caller that derives a nested
    // timeout from the remaining budget.
    #[tokio::test(start_paused = true)]
    async fn an_exhausted_budget_is_reported_as_a_readiness_timeout() {
        let deadline =
            Deadline::new(Duration::from_secs(10)).expect("a positive deadline is valid");
        assert_eq!(
            deadline
                .remaining_or_expired("bitcoind", "deriving the fixture RPC timeout")
                .expect("a fresh budget is available"),
            Duration::from_secs(10)
        );
        advance(Duration::from_secs(10)).await;

        let error = deadline
            .remaining_or_expired("bitcoind", "deriving the fixture RPC timeout")
            .expect_err("an exhausted budget must not yield a zero timeout");

        let FixtureError::ReadinessTimeout {
            service,
            duration,
            last_observation,
            ..
        } = error
        else {
            panic!("an exhausted budget must become a readiness timeout");
        };
        assert_eq!(service, "bitcoind");
        assert_eq!(duration, Duration::from_secs(10));
        assert_eq!(last_observation, "deriving the fixture RPC timeout");
    }

    // Catches a regression that reports a caller's observation verbatim: the observation is built
    // from Docker and RPC text, so the deadline must bound and redact it rather than trusting every
    // call site to have done so.
    #[tokio::test(start_paused = true)]
    async fn a_reported_observation_is_bounded_and_redacted() {
        let deadline = Deadline::new(Duration::from_secs(1)).expect("a positive deadline is valid");
        let observation = format!("{} admin1:123", "root RPC: ".repeat(4 * 1024));

        let error = deadline
            .run("bitcoind", &observation, async {
                sleep(Duration::from_secs(2)).await;
            })
            .await
            .expect_err("the operation must exhaust the budget");

        let FixtureError::ReadinessTimeout {
            last_observation, ..
        } = error
        else {
            panic!("deadline expiration must become a readiness timeout");
        };
        assert!(last_observation.len() <= 16 * 1024);
        assert!(!last_observation.contains("admin1:123"));
        assert!(last_observation.ends_with("[REDACTED]"));
    }

    // Catches a regression that unwraps or remaps a ready operation's own Result.
    #[tokio::test(start_paused = true)]
    async fn run_leaves_a_completed_nested_result_unchanged() {
        let deadline =
            Deadline::new(Duration::from_secs(10)).expect("a positive deadline is valid");

        let result: Result<Result<(), &'static str>, FixtureError> = deadline
            .run("bitcoind", "waiting for root RPC", async {
                Err("RPC rejected")
            })
            .await;

        match result {
            Ok(Err("RPC rejected")) => {}
            Ok(Err(other)) => panic!("unexpected inner error: {other}"),
            Ok(Ok(())) => panic!("the inner RPC error must remain intact"),
            Err(error) => panic!("a ready operation must not become a deadline error: {error}"),
        }
    }
}
