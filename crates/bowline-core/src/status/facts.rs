use std::{
    cmp::{Ordering, Reverse},
    collections::BTreeMap,
};

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const MAX_STATUS_FACTS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusAvailability {
    Ready,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusFactAvailabilityImpact {
    None,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusAttention {
    None,
    Recommended,
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusFactScope {
    Account,
    Workspace,
    Project,
    Device,
    Session,
    WorkView,
    Lease,
    Path,
}

impl StatusFactScope {
    fn token(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Workspace => "workspace",
            Self::Project => "project",
            Self::Device => "device",
            Self::Session => "session",
            Self::WorkView => "work_view",
            Self::Lease => "lease",
            Self::Path => "path",
        }
    }

    fn specificity(self) -> u8 {
        match self {
            Self::Account => 0,
            Self::Workspace => 1,
            Self::Project | Self::Device => 2,
            Self::Session | Self::WorkView => 3,
            Self::Lease => 4,
            Self::Path => 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StatusFactId(String);

impl StatusFactId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StatusFactKind(String);

impl StatusFactKind {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StatusFactSource(String);

impl StatusFactSource {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StatusDedupeKey(String);

impl StatusDedupeKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusActionReference {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusFact {
    pub id: StatusFactId,
    pub kind: StatusFactKind,
    pub source: StatusFactSource,
    pub scope: StatusFactScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    pub availability_impact: StatusFactAvailabilityImpact,
    pub attention_impact: StatusAttention,
    pub summary_key: String,
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<StatusActionReference>,
    pub observed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_after: Option<String>,
    pub dedupe_key: StatusDedupeKey,
}

impl StatusFact {
    pub fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        source: impl Into<String>,
        scope: StatusFactScope,
        observed_at: impl Into<String>,
        dedupe_key: impl Into<String>,
    ) -> Self {
        let kind = StatusFactKind::new(kind);
        let policy = status_fact_policy(kind.as_str());
        Self {
            id: StatusFactId::new(id),
            kind,
            source: StatusFactSource::new(source),
            scope,
            scope_id: None,
            availability_impact: policy.availability,
            attention_impact: policy.attention,
            summary_key: policy.summary_key.to_string(),
            parameters: BTreeMap::new(),
            action: policy.action.map(|kind| StatusActionReference {
                kind: kind.to_string(),
                target_id: None,
            }),
            observed_at: observed_at.into(),
            stale_after: None,
            dedupe_key: StatusDedupeKey::new(dedupe_key),
        }
    }

    pub fn with_scope_id(mut self, scope_id: impl Into<String>) -> Self {
        self.scope_id = Some(scope_id.into());
        self
    }

    pub fn with_impacts(
        mut self,
        availability: StatusFactAvailabilityImpact,
        attention: StatusAttention,
    ) -> Self {
        let policy = status_fact_policy(self.kind.as_str());
        if policy.impacts_overridable {
            self.availability_impact = availability;
            self.attention_impact = attention;
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusFactPolicy {
    pub authority: &'static str,
    pub availability: StatusFactAvailabilityImpact,
    pub attention: StatusAttention,
    pub action: Option<&'static str>,
    pub summary_key: &'static str,
    pub priority_band: u8,
    pub hosted_allowed: bool,
    pub workspace_affecting: bool,
    pub impacts_overridable: bool,
}

pub fn status_fact_policy(kind: &str) -> StatusFactPolicy {
    if let Some(authority) =
        crate::wire::generated_status_fact_authorities::status_fact_authority(kind)
    {
        return StatusFactPolicy {
            authority: authority.authority,
            availability: match authority.availability_impact {
                "degraded" => StatusFactAvailabilityImpact::Degraded,
                "unavailable" => StatusFactAvailabilityImpact::Unavailable,
                _ => StatusFactAvailabilityImpact::None,
            },
            attention: match authority.attention_impact {
                "recommended" => StatusAttention::Recommended,
                "required" => StatusAttention::Required,
                _ => StatusAttention::None,
            },
            action: authority.action_kind,
            summary_key: authority.summary_key,
            priority_band: authority.priority_band,
            hosted_allowed: authority.hosted_allowed,
            workspace_affecting: authority.workspace_affecting,
            impacts_overridable: authority.impacts_overridable,
        };
    }
    StatusFactPolicy {
        authority: "unknown",
        availability: StatusFactAvailabilityImpact::None,
        attention: StatusAttention::None,
        action: None,
        summary_key: "status.generic",
        priority_band: 100,
        hosted_allowed: false,
        workspace_affecting: false,
        impacts_overridable: false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSummary {
    pub availability: StatusAvailability,
    pub attention: StatusAttention,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_fact_id: Option<StatusFactId>,
    pub facts: Vec<StatusFact>,
    pub snapshot_version: u64,
    pub observed_at: String,
    pub freshness: StatusSnapshotFreshness,
}

impl StatusSummary {
    pub fn presentation_level(&self) -> super::StatusLevel {
        if self.attention == StatusAttention::Required {
            super::StatusLevel::Attention
        } else if self.availability != StatusAvailability::Ready {
            super::StatusLevel::Limited
        } else if self.attention == StatusAttention::Recommended {
            super::StatusLevel::Attention
        } else {
            super::StatusLevel::Healthy
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusSnapshotFreshness {
    Fresh,
    Stale,
    Unknown,
}

pub fn reduce_status_facts(
    facts: impl IntoIterator<Item = StatusFact>,
    snapshot_version: u64,
    observed_at: impl Into<String>,
) -> StatusSummary {
    let observed_at = observed_at.into();
    let mut newest_by_authority =
        BTreeMap::<(StatusFactSource, StatusDedupeKey), StatusFact>::new();
    for mut fact in facts {
        let Some(authority) = crate::wire::generated_status_fact_authorities::status_fact_authority(
            fact.kind.as_str(),
        ) else {
            fact.availability_impact = StatusFactAvailabilityImpact::None;
            fact.attention_impact = StatusAttention::None;
            fact.action = None;
            let key = (fact.source.clone(), fact.dedupe_key.clone());
            insert_newest_fact(&mut newest_by_authority, key, fact);
            continue;
        };
        if fact.source.as_str() != authority.authority
            || !authority.valid_scopes.contains(&fact.scope.token())
        {
            continue;
        }
        let policy = status_fact_policy(fact.kind.as_str());
        if !authority.impacts_overridable {
            fact.availability_impact = policy.availability;
            fact.attention_impact = policy.attention;
        }
        if fact.action.as_ref().map(|action| action.kind.as_str()) != authority.action_kind {
            fact.action = None;
        }
        let key = (fact.source.clone(), fact.dedupe_key.clone());
        insert_newest_fact(&mut newest_by_authority, key, fact);
    }
    let mut facts = newest_by_authority.into_values().collect::<Vec<_>>();
    for fact in &mut facts {
        if fact
            .stale_after
            .as_ref()
            .is_some_and(|deadline| timestamp_is_before(deadline, &observed_at))
        {
            fact.availability_impact = fact
                .availability_impact
                .max(StatusFactAvailabilityImpact::Degraded);
            fact.attention_impact = fact.attention_impact.max(StatusAttention::Recommended);
        }
    }
    facts.sort_by(fact_order);
    facts.truncate(MAX_STATUS_FACTS);
    let availability = match facts
        .iter()
        .map(|fact| fact.availability_impact)
        .max()
        .unwrap_or(StatusFactAvailabilityImpact::None)
    {
        StatusFactAvailabilityImpact::None => StatusAvailability::Ready,
        StatusFactAvailabilityImpact::Degraded => StatusAvailability::Degraded,
        StatusFactAvailabilityImpact::Unavailable => StatusAvailability::Unavailable,
    };
    let attention = facts
        .iter()
        .map(|fact| fact.attention_impact)
        .max()
        .unwrap_or(StatusAttention::None);
    let freshness = if facts.iter().any(|fact| {
        fact.stale_after
            .as_ref()
            .is_some_and(|deadline| timestamp_is_before(deadline, &observed_at))
    }) {
        StatusSnapshotFreshness::Stale
    } else {
        StatusSnapshotFreshness::Fresh
    };
    let primary_fact_id = facts.first().map(|fact| fact.id.clone());
    StatusSummary {
        availability,
        attention,
        primary_fact_id,
        facts,
        snapshot_version,
        observed_at,
        freshness,
    }
}

fn insert_newest_fact(
    facts: &mut BTreeMap<(StatusFactSource, StatusDedupeKey), StatusFact>,
    key: (StatusFactSource, StatusDedupeKey),
    fact: StatusFact,
) {
    if let Some(existing) = facts.get(&key) {
        let ordering = compare_timestamps(&existing.observed_at, &fact.observed_at);
        if ordering == Ordering::Greater || (ordering == Ordering::Equal && existing.id >= fact.id)
        {
            return;
        }
    }
    facts.insert(key, fact);
}

fn parse_timestamp(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok()
}

fn timestamp_is_before(left: &str, right: &str) -> bool {
    compare_timestamps(left, right) == Ordering::Less
}

fn compare_timestamps(left: &str, right: &str) -> Ordering {
    match (parse_timestamp(left), parse_timestamp(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn fact_order(left: &StatusFact, right: &StatusFact) -> std::cmp::Ordering {
    let left_policy = status_fact_policy(left.kind.as_str());
    let right_policy = status_fact_policy(right.kind.as_str());
    (
        Reverse(left.attention_impact),
        Reverse(left.availability_impact),
        left_policy.priority_band,
        Reverse(left.scope.specificity()),
        left.kind.as_str(),
        left.id.as_str(),
    )
        .cmp(&(
            Reverse(right.attention_impact),
            Reverse(right.availability_impact),
            right_policy.priority_band,
            Reverse(right.scope.specificity()),
            right.kind.as_str(),
            right.id.as_str(),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fact(id: &str, kind: &str) -> StatusFact {
        let policy = status_fact_policy(kind);
        StatusFact::new(
            id,
            kind,
            policy.authority,
            StatusFactScope::Workspace,
            "2026-07-12T00:00:00Z",
            id,
        )
    }

    #[test]
    fn degraded_and_required_remain_independent() {
        let summary = reduce_status_facts(
            [
                fact("offline", "network.offline"),
                fact("conflict", "sync.conflict_unresolved"),
            ],
            1,
            "2026-07-12T00:00:01Z",
        );
        assert_eq!(summary.availability, StatusAvailability::Degraded);
        assert_eq!(summary.attention, StatusAttention::Required);
        assert_eq!(summary.facts.len(), 2);
    }

    #[test]
    fn recommended_attention_does_not_hide_limited_availability() {
        let summary = reduce_status_facts(
            [fact("unavailable", "sync.component_unavailable")],
            1,
            "2026-07-12T00:00:01Z",
        );

        assert_eq!(summary.availability, StatusAvailability::Unavailable);
        assert_eq!(summary.attention, StatusAttention::Recommended);
        assert_eq!(
            summary.presentation_level(),
            crate::status::StatusLevel::Limited
        );
    }

    #[test]
    fn reduction_is_permutation_invariant() {
        let left = fact("offline", "network.offline");
        let right = fact("conflict", "sync.conflict_unresolved");
        let forward = reduce_status_facts([left.clone(), right.clone()], 1, "2026-07-12T00:00:01Z");
        let reverse = reduce_status_facts([right, left], 1, "2026-07-12T00:00:01Z");
        assert_eq!(forward, reverse);
    }

    #[test]
    fn dedupe_is_scoped_to_authority_and_chooses_newest() {
        let mut old = fact("old", "network.offline");
        old.dedupe_key = StatusDedupeKey::new("network");
        let mut new = fact("new", "network.offline");
        new.dedupe_key = StatusDedupeKey::new("network");
        new.observed_at = "2026-07-12T00:01:00Z".to_string();
        let summary = reduce_status_facts([old, new], 2, "2026-07-12T00:01:01Z");
        assert_eq!(summary.facts.len(), 1);
        assert_eq!(summary.facts[0].id.as_str(), "new");
    }

    #[test]
    fn fact_policy_uses_generated_authority_metadata() {
        let policy = status_fact_policy("sync.conflict_unresolved");
        assert_eq!(policy.authority, "local-conflict-store");
        assert_eq!(policy.availability, StatusFactAvailabilityImpact::Degraded);
        assert_eq!(policy.attention, StatusAttention::Required);
        assert_eq!(policy.action, Some("resolve-conflict"));
        assert_eq!(policy.summary_key, "status.sync.conflict_unresolved");
        assert_eq!(policy.priority_band, 0);
        assert!(policy.hosted_allowed);
        assert!(policy.workspace_affecting);
        assert!(!policy.impacts_overridable);
    }

    #[test]
    fn neutral_fact_impact_uses_wire_none_token() {
        let value = serde_json::to_value(fact("update", "client.update_available"))
            .expect("status fact serializes");
        assert_eq!(value["availabilityImpact"], "none");
        assert_eq!(value["parameters"], serde_json::json!({}));
    }

    #[test]
    fn unknown_fact_policy_is_neutral_and_not_hosted() {
        let policy = status_fact_policy("future.fact-kind");
        assert_eq!(policy.authority, "unknown");
        assert_eq!(policy.availability, StatusFactAvailabilityImpact::None);
        assert_eq!(policy.attention, StatusAttention::None);
        assert_eq!(policy.action, None);
        assert_eq!(policy.summary_key, "status.generic");
        assert_eq!(policy.priority_band, 100);
        assert!(!policy.hosted_allowed);
        assert!(!policy.workspace_affecting);
        assert!(!policy.impacts_overridable);
    }

    #[test]
    fn native_reducer_matches_generated_conformance_vectors() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../tests/contracts/generated/status-reducer-vectors.json"
        ))
        .expect("status reducer vectors must be valid JSON");
        for case in fixture["cases"]
            .as_array()
            .expect("status reducer cases must be an array")
        {
            let expected = &case["expected"];
            let facts = case["input"]
                .as_array()
                .expect("status reducer input must be an array")
                .iter()
                .map(fact_from_vector)
                .collect::<Vec<_>>();
            let summary = reduce_status_facts(
                facts,
                expected["snapshotVersion"]
                    .as_u64()
                    .expect("snapshot version must be an integer"),
                expected["observedAt"]
                    .as_str()
                    .expect("observed time must be a string"),
            );

            assert_eq!(
                availability_token(summary.availability),
                expected["availability"].as_str().expect("availability")
            );
            assert_eq!(
                attention_token(summary.attention),
                expected["attention"].as_str().expect("attention")
            );
            assert_eq!(
                summary.primary_fact_id.as_ref().map(StatusFactId::as_str),
                expected["primaryFactId"].as_str()
            );
            assert_eq!(
                summary
                    .facts
                    .iter()
                    .map(|fact| fact.id.as_str())
                    .collect::<Vec<_>>(),
                expected["facts"]
                    .as_array()
                    .expect("expected facts")
                    .iter()
                    .map(|fact| fact["id"].as_str().expect("fact id"))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn fact_payload_is_deterministically_bounded() {
        let facts = (0..(MAX_STATUS_FACTS + 20))
            .map(|index| fact(&format!("fact-{index:03}"), "status.aggregate_input"))
            .collect::<Vec<_>>();
        let forward = reduce_status_facts(facts.clone(), 7, "2026-07-12T12:00:00Z");
        let reverse = reduce_status_facts(facts.into_iter().rev(), 7, "2026-07-12T12:00:00Z");
        let bounded = reduce_status_facts(forward.facts.clone(), 7, "2026-07-12T12:00:00Z");

        assert_eq!(forward.facts.len(), MAX_STATUS_FACTS);
        assert_eq!(forward, reverse);
        assert_eq!(forward, bounded);
    }

    fn fact_from_vector(value: &Value) -> StatusFact {
        let scope = match value["scope"].as_str().expect("fact scope") {
            "account" => StatusFactScope::Account,
            "workspace" => StatusFactScope::Workspace,
            "project" => StatusFactScope::Project,
            "device" => StatusFactScope::Device,
            "session" => StatusFactScope::Session,
            "work_view" => StatusFactScope::WorkView,
            "lease" => StatusFactScope::Lease,
            "path" => StatusFactScope::Path,
            unknown => panic!("unknown fact scope {unknown}"),
        };
        let mut fact = StatusFact::new(
            value["id"].as_str().expect("fact id"),
            value["kind"].as_str().expect("fact kind"),
            value["source"].as_str().expect("fact source"),
            scope,
            value["observedAt"].as_str().expect("fact observation time"),
            value["dedupeKey"].as_str().expect("fact dedupe key"),
        )
        .with_impacts(
            match value["availabilityImpact"]
                .as_str()
                .expect("availability impact")
            {
                "degraded" => StatusFactAvailabilityImpact::Degraded,
                "unavailable" => StatusFactAvailabilityImpact::Unavailable,
                _ => StatusFactAvailabilityImpact::None,
            },
            match value["attentionImpact"].as_str().expect("attention impact") {
                "recommended" => StatusAttention::Recommended,
                "required" => StatusAttention::Required,
                _ => StatusAttention::None,
            },
        );
        fact.scope_id = value["scopeId"].as_str().map(str::to_string);
        fact.stale_after = value["staleAfter"].as_str().map(str::to_string);
        fact
    }

    const fn availability_token(value: StatusAvailability) -> &'static str {
        match value {
            StatusAvailability::Ready => "ready",
            StatusAvailability::Degraded => "degraded",
            StatusAvailability::Unavailable => "unavailable",
        }
    }

    const fn attention_token(value: StatusAttention) -> &'static str {
        match value {
            StatusAttention::None => "none",
            StatusAttention::Recommended => "recommended",
            StatusAttention::Required => "required",
        }
    }
}
