use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use crate::organizers::{Organizer, OrganizerDb, OrganizerError, OrganizerRegistry};

/// Limits for one background job. A turn is the uninterrupted work for one Organizer; a pass
/// gives turns to every Organizer in rounds until none has work left for this pass.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub turn_rows: usize,
    pub turn_time: Duration,
    pub pass_rows: usize,
}

impl Budget {
    /// Mail jobs keep their limit of 200 rows per Organizer per pass.
    pub const MAIL: Self = Self {
        turn_rows: 25,
        turn_time: Duration::from_secs(30),
        pass_rows: 200,
    };
    /// Refund work attempts every due Order once per pass.
    pub const REFUNDS: Self = Self {
        turn_rows: 25,
        turn_time: Duration::from_secs(30),
        pass_rows: usize::MAX,
    };

    /// The whole pass limit in one turn, for a caller that serves one Organizer.
    pub const fn single_turn(rows: usize) -> Self {
        Self {
            turn_rows: rows,
            turn_time: Duration::MAX,
            pass_rows: rows,
        }
    }
}

pub enum Admission {
    Granted,
    AlreadyAttempted,
    OutOfBudget,
}

pub struct Turn<'a> {
    attempted: &'a mut HashSet<String>,
    deferred: &'a mut bool,
    budget: Budget,
    started: Instant,
    granted: usize,
    stop: Option<Arc<AtomicBool>>,
}

impl Turn<'_> {
    /// Decides before each row and never interrupts a row in flight, so a provider call keeps
    /// its claim and its recorded result. Unless shutdown has started, the first new row of
    /// a turn is always granted: every round then makes progress, and a pass ends.
    pub fn admit(&mut self, row: &str) -> Admission {
        if self
            .stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::SeqCst))
        {
            return Admission::OutOfBudget;
        }
        if self.attempted.contains(row) {
            return Admission::AlreadyAttempted;
        }
        if self.attempted.len() >= self.budget.pass_rows {
            return Admission::OutOfBudget;
        }
        if self.granted > 0
            && (self.granted >= self.budget.turn_rows
                || self.started.elapsed() >= self.budget.turn_time)
        {
            *self.deferred = true;
            return Admission::OutOfBudget;
        }
        self.attempted.insert(row.to_owned());
        self.granted += 1;
        Admission::Granted
    }
}

/// One pass of a job. Rows already attempted in this pass are not attempted again, so a row
/// that keeps failing is retried once per pass and cannot hold a turn. The attempted set lives
/// in memory only: after a restart the durable row states decide what is due.
pub struct Pass {
    budget: Budget,
    attempted: HashMap<String, HashSet<String>>,
    started: bool,
    deferred: bool,
}

impl Pass {
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            attempted: HashMap::new(),
            started: false,
            deferred: false,
        }
    }

    /// The Organizers of the next round, or `None` when no turn of the last round ran out of
    /// budget.
    pub async fn next_round(
        &mut self,
        registry: &OrganizerRegistry,
    ) -> Result<Option<Vec<Organizer>>, OrganizerError> {
        if registry.background_jobs_stopping() || (self.started && !self.deferred) {
            return Ok(None);
        }
        self.started = true;
        self.deferred = false;
        registry.active_organizers().await.map(Some)
    }

    pub async fn turn(
        &mut self,
        registry: &OrganizerRegistry,
        organizer: &Organizer,
    ) -> Option<(OrganizerDb, Turn<'_>)> {
        if registry.background_jobs_stopping() {
            return None;
        }
        match registry.resolve_organizer(organizer).await {
            Ok(scope) => {
                let mut turn = self.single_turn(&organizer.id);
                turn.stop = Some(registry.background_stop());
                Some((scope, turn))
            }
            Err(error) => {
                tracing::error!(%error, organizer_id = %organizer.id, "Tenant background database unavailable");
                None
            }
        }
    }

    pub fn single_turn(&mut self, organizer_id: &str) -> Turn<'_> {
        Turn {
            attempted: self.attempted.entry(organizer_id.to_owned()).or_default(),
            deferred: &mut self.deferred,
            budget: self.budget,
            started: Instant::now(),
            granted: 0,
            stop: None,
        }
    }
}
