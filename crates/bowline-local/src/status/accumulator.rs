use super::*;

pub(super) struct StatusAccumulator {
    pub(super) items: Vec<StatusItem>,
    pub(super) limits: Vec<LimitedCapability>,
    pub(super) attention_items: Vec<String>,
    pub(super) next_actions: Vec<RepairCommand>,
    pub(super) facts: Vec<StatusFact>,
    observed_at: String,
}

impl StatusAccumulator {
    pub(super) fn new(observed_at: &str) -> Self {
        Self {
            items: Vec::new(),
            limits: Vec::new(),
            attention_items: Vec::new(),
            next_actions: Vec::new(),
            facts: Vec::new(),
            observed_at: observed_at.to_string(),
        }
    }

    pub(super) fn observe_fact(
        &mut self,
        kind: &str,
        id: impl Into<String>,
        dedupe_key: impl Into<String>,
        scope: StatusFactScope,
        scope_id: Option<&str>,
    ) {
        let policy = status_fact_policy(kind);
        let mut fact = StatusFact::new(
            id,
            kind,
            policy.authority,
            scope,
            self.observed_at.clone(),
            dedupe_key,
        );
        if let Some(scope_id) = scope_id {
            fact = fact.with_scope_id(scope_id);
        }
        self.facts.push(fact);
    }

    pub(super) fn observe_aggregate_fact(
        &mut self,
        id: impl Into<String>,
        dedupe_key: impl Into<String>,
        scope: StatusFactScope,
        scope_id: Option<&str>,
        availability: StatusFactAvailabilityImpact,
        attention: StatusAttention,
    ) {
        let mut fact = StatusFact::new(
            id,
            "status.aggregate_input",
            "status-reducer",
            scope,
            self.observed_at.clone(),
            dedupe_key,
        )
        .with_impacts(availability, attention);
        if let Some(scope_id) = scope_id {
            fact = fact.with_scope_id(scope_id);
        }
        self.facts.push(fact);
    }
}
