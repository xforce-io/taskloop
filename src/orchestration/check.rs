use super::{plan::bounded_json, registry::FrozenRegistry, schema::invalid, *};
use crate::{Error, Result, json::digest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckInputPhase {
    Plan,
    NodeCheckpoint,
    GoalFinal,
    NodeEvaluation,
}
impl From<&CheckPhase> for CheckInputPhase {
    fn from(phase: &CheckPhase) -> Self {
        match phase {
            CheckPhase::Plan => Self::Plan,
            CheckPhase::NodeCheckpoint => Self::NodeCheckpoint,
            CheckPhase::GoalFinal => Self::GoalFinal,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckTarget {
    pub artifact_id: String,
    pub sha256: String,
    pub schema_digest: String,
    pub value: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckSubject {
    Proposal {
        proposal_digest: String,
        proposal: PlanProposal,
    },
    Artifacts {
        plan_digest: String,
        targets: BTreeMap<String, CheckTarget>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvaluationSubject {
    Proposal {
        proposal_digest: String,
    },
    Artifacts {
        plan_digest: String,
        targets: BTreeMap<String, CheckTargetIdentity>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckTargetIdentity {
    pub artifact_id: String,
    pub sha256: String,
    pub schema_digest: String,
}
impl CheckSubject {
    fn identity(&self) -> EvaluationSubject {
        match self {
            Self::Proposal {
                proposal_digest, ..
            } => EvaluationSubject::Proposal {
                proposal_digest: proposal_digest.clone(),
            },
            Self::Artifacts {
                plan_digest,
                targets,
            } => EvaluationSubject::Artifacts {
                plan_digest: plan_digest.clone(),
                targets: targets
                    .iter()
                    .map(|(alias, target)| {
                        (
                            alias.clone(),
                            CheckTargetIdentity {
                                artifact_id: target.artifact_id.clone(),
                                sha256: target.sha256.clone(),
                                schema_digest: target.schema_digest.clone(),
                            },
                        )
                    })
                    .collect(),
            },
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckInput {
    pub protocol_version: u32,
    pub goal_id: String,
    pub goal: String,
    pub goal_spec_digest: String,
    pub check_id: String,
    pub rules_digest: String,
    pub phase: CheckInputPhase,
    pub epoch: u64,
    pub nonce: String,
    pub issued_revision: u64,
    pub subject: CheckSubject,
    pub rules: RuleSet,
    pub goal_inputs: BTreeMap<String, Value>,
    pub evidence_refs: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationReceipt {
    pub protocol_version: u32,
    pub evaluation_id: String,
    pub goal_id: String,
    pub check_id: String,
    pub phase: CheckInputPhase,
    pub checker_id: String,
    pub rules_digest: String,
    pub subject: EvaluationSubject,
    pub input_digest: String,
    pub candidate_digest: String,
    pub producer_run_id: Option<String>,
    pub verdict: EvaluationVerdict,
    pub evaluation: EvaluationCandidate,
    pub accepted_revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IssuedCheck {
    pub input: CheckInput,
    pub input_digest: String,
    pub checker_ref: String,
    pub producer_run_id: Option<String>,
}
pub(crate) struct CheckedEvaluation {
    pub candidate: EvaluationCandidate,
    pub candidate_digest: String,
}
impl FrozenRegistry {
    /// The caller constructs subject from admitted public artifacts/proposals in the same transaction.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn issue_check(
        &self,
        goal_id: &str,
        goal: &str,
        spec_digest: &str,
        goal_inputs: &Value,
        check_id: &str,
        phase: CheckInputPhase,
        checker_ref: &str,
        epoch: u64,
        revision: u64,
        subject: CheckSubject,
        evidence: BTreeSet<String>,
    ) -> Result<IssuedCheck> {
        let checker = self
            .checkers
            .get(checker_ref)
            .ok_or_else(|| invalid("/checker_ref", "unknown checker"))?;
        let rules = &self.rules[&checker.rules_ref];
        let projected = checker
            .goal_input_fields
            .iter()
            .filter_map(|k| goal_inputs.get(k).map(|value| (k.clone(), value.clone())))
            .collect();
        let input = CheckInput {
            protocol_version: 1,
            goal_id: goal_id.into(),
            goal: goal.into(),
            goal_spec_digest: spec_digest.into(),
            check_id: check_id.into(),
            rules_digest: rules.digest.clone(),
            phase,
            epoch,
            nonce: format!("check-{}", uuid::Uuid::new_v4()),
            issued_revision: revision,
            subject,
            rules: rules.value.clone(),
            goal_inputs: projected,
            evidence_refs: evidence.into_iter().collect(),
        };
        let bytes = bounded_json(&input, 4 * 1024 * 1024)?;
        self.schemas
            .contract("check-input", &crate::parse_unique(&bytes)?)?;
        Ok(IssuedCheck {
            input_digest: digest(&bytes),
            input,
            checker_ref: checker_ref.into(),
            producer_run_id: None,
        })
    }
    /// The single content acceptance path for plan/final checks and both evaluation-node purposes.
    /// Business authority and original Run identity are checked by the coordinator separately.
    pub(crate) fn check_evaluation(
        &self,
        request: &IssuedCheck,
        bytes: &[u8],
        max_bytes: usize,
        existing_evidence: &BTreeSet<String>,
    ) -> Result<CheckedEvaluation> {
        let reject = |path: &str, message: &str| Error::new("check_unavailable", path, message);
        if bytes.len() > max_bytes.min(4 * 1024 * 1024) {
            return Err(reject("/evaluation", "evaluation exceeds candidate limit"));
        }
        let value = crate::parse_unique(bytes).map_err(|e| reject(&e.path, &e.message))?;
        self.schemas
            .contract("evaluation-candidate", &value)
            .map_err(|e| reject(&e.path, &e.message))?;
        let candidate: EvaluationCandidate = serde_json::from_value(value)?;
        let required = request
            .input
            .rules
            .rules
            .iter()
            .map(|r| (&r.id, r))
            .collect::<BTreeMap<_, _>>();
        let result_ids = candidate
            .results
            .iter()
            .map(|r| &r.id)
            .collect::<BTreeSet<_>>();
        if result_ids.len() != candidate.results.len()
            || result_ids != required.keys().copied().collect::<BTreeSet<_>>()
        {
            return Err(reject(
                "/results",
                "result rule IDs must cover the request exactly, without duplicates",
            ));
        }
        let allowed = request.input.evidence_refs.iter().collect::<BTreeSet<_>>();
        for evidence in candidate.evidence_refs.iter().chain(
            candidate
                .results
                .iter()
                .flat_map(|r| r.evidence_refs.iter()),
        ) {
            if !allowed.contains(evidence) || !existing_evidence.contains(evidence) {
                return Err(reject(
                    "/evidence_refs",
                    "evidence is not authorized and present",
                ));
            }
        }
        for (index, result) in candidate.results.iter().enumerate() {
            if required[&result.id].evidence_required && result.evidence_refs.is_empty() {
                return Err(reject(
                    &format!("/results/{index}/evidence_refs"),
                    "required evidence missing",
                ));
            }
        }
        let verdict = if candidate
            .results
            .iter()
            .any(|r| r.verdict == EvaluationVerdict::Error)
        {
            EvaluationVerdict::Error
        } else if candidate
            .results
            .iter()
            .any(|r| r.verdict == EvaluationVerdict::Fail)
        {
            EvaluationVerdict::Fail
        } else {
            EvaluationVerdict::Pass
        };
        if candidate.verdict != verdict {
            return Err(reject(
                "/verdict",
                "overall verdict disagrees with rule results (error precedes fail)",
            ));
        }
        if verdict == EvaluationVerdict::Pass && !candidate.violations.is_empty() {
            return Err(reject("/violations", "pass cannot contain violations"));
        }
        Ok(CheckedEvaluation {
            candidate,
            candidate_digest: digest(bytes),
        })
    }
}
impl IssuedCheck {
    /// Receipt identity is generated exclusively from Runtime's persisted request.
    pub(crate) fn receipt(
        &self,
        checked: CheckedEvaluation,
        accepted_revision: u64,
    ) -> EvaluationReceipt {
        EvaluationReceipt {
            protocol_version: 1,
            evaluation_id: self.input.nonce.clone(),
            goal_id: self.input.goal_id.clone(),
            check_id: self.input.check_id.clone(),
            phase: self.input.phase,
            checker_id: self.checker_ref.clone(),
            rules_digest: self.input.rules_digest.clone(),
            subject: self.input.subject.identity(),
            input_digest: self.input_digest.clone(),
            candidate_digest: checked.candidate_digest,
            producer_run_id: self.producer_run_id.clone(),
            verdict: checked.candidate.verdict.clone(),
            evaluation: checked.candidate,
            accepted_revision,
        }
    }
}
