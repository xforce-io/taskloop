//! Application-side adapters for the pinned Inquiry domain library; no TaskLoop storage access.
use crate::fault;
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};
use taskloop::*;
use tokio_util::sync::CancellationToken;

pub struct Fetch;
#[async_trait]
impl DomainTool for Fetch {
    fn spec(&self) -> ToolSpec {
        inquiry::domain::Fetch.spec()
    }
    async fn call(&self, mut context: ToolContext, args: Value) -> Result<ToolOutput> {
        // Legacy lookup view of the already-authorized public candidates; identity/bytes unchanged.
        for artifact in &mut context.accepted {
            if artifact.reference.slot == "candidates" {
                artifact.reference.state_id = "discover".into();
            }
        }
        let mut output = inquiry::domain::Fetch.call(context, args).await?;
        for candidate in &mut output.files {
            if candidate.slot.starts_with("capture.raw.") {
                // Goal public ports are JSON. Preserve the original bytes losslessly, without
                // claiming that this wrapper is an Inquiry normalized snapshot.
                let wrapped = json!({"encoding":"base64","body_sha256":candidate.sha256(),"content":STANDARD.encode(&candidate.bytes)});
                *candidate = Candidate::json(&candidate.slot, &wrapped)?;
            }
        }
        Ok(output)
    }
}
pub struct Sources;
#[async_trait]
impl DomainTool for Sources {
    fn spec(&self) -> ToolSpec {
        inquiry::domain::ReadSource.spec()
    }
    async fn call(&self, context: ToolContext, args: Value) -> Result<ToolOutput> {
        inquiry::domain::ReadSource.call(context, args).await
    }
}
pub enum Native {
    Select,
    Fetch,
    DomainCheck,
}
impl Native {
    async fn run(&self, inv: &Invocation, tools: &Arc<dyn ToolBridge>) -> Result<()> {
        match self {
            Self::Select => {
                let urls = inv.inputs["seed_urls"]
                    .as_array()
                    .ok_or_else(|| fault("two seed URLs required"))?;
                if urls.len() != 2 {
                    return Err(fault(
                        "this bounded consumer requires exactly two user URLs",
                    ));
                }
                let items = urls.iter().enumerate().map(|(i,url)| {
                    let normalized = inquiry::net::normalize_url(url.as_str().unwrap_or("")).map_err(|e| fault(e.reason))?;
                    Ok(json!({"source_id":format!("r1s{}",i+1),"url":normalized.as_str(),"discovery_receipt_id":"user-input","subquestion":inv.inputs["question"]}))
                }).collect::<Result<Vec<_>>>()?;
                let candidates = json!({"items":items,"stop_reason":"Bounded integration uses the two explicitly supplied official URLs, without claiming search."});
                inquiry::domain::check_domain(
                    "discover",
                    1,
                    &inv.inputs,
                    &candidates,
                    &[],
                    &[],
                    &[],
                )?;
                tools
                    .call(
                        "artifact.write",
                        json!({"slot":"candidates","value":candidates}),
                    )
                    .await?;
            }
            Self::Fetch => {
                let candidates = &inv.stage_input["candidates"]["items"];
                for item in candidates
                    .as_array()
                    .ok_or_else(|| fault("candidate array missing"))?
                {
                    let result = tools
                        .call(
                            "inquiry.fetch",
                            json!({"source_id":item["source_id"],"url":item["url"]}),
                        )
                        .await?;
                    if result["result"]["usable"] != true {
                        return Err(Error::new(
                            "source_unavailable",
                            "/fetch",
                            result["result"]["reason"]
                                .as_str()
                                .unwrap_or("source unavailable"),
                        ));
                    }
                }
            }
            Self::DomainCheck => {
                let input: CheckInput = serde_json::from_value(inv.stage_input.clone())?;
                let CheckSubject::Artifacts { targets, .. } = &input.subject else {
                    return Err(fault("artifact evaluation required"));
                };
                let mut accepted = Vec::new();
                let mut values = BTreeMap::new();
                for (alias, target) in targets {
                    let reply = tools
                        .call("artifact.read", json!({"id":target.artifact_id}))
                        .await?;
                    let text = reply["text"]
                        .as_str()
                        .ok_or_else(|| fault("artifact text missing"))?;
                    let mut reference: ArtifactRef =
                        serde_json::from_value(reply["artifact"].clone())?;
                    if reference.id != target.artifact_id
                        || reference.sha256 != target.sha256
                        || format!("{:x}", Sha256::digest(text.as_bytes())) != target.sha256
                    {
                        return Err(fault("exact target identity/hash mismatch"));
                    }
                    match alias.as_str() {
                        "analysis" => {
                            reference.state_id = "analyze".into();
                            reference.slot = "analysis".into();
                        }
                        "article" => {
                            reference.state_id = "report".into();
                            reference.slot = "report".into();
                        }
                        _ => reference.state_id = "capture".into(),
                    }
                    values.insert(alias.clone(), parse_unique(text.as_bytes())?);
                    accepted.push(ArtifactData {
                        reference,
                        bytes: text.as_bytes().to_vec().into(),
                    });
                }
                let analysis = &values["analysis"];
                let article = &values["article"];
                let checked = inquiry::domain::check_domain(
                    "analyze",
                    1,
                    &json!({}),
                    analysis,
                    &accepted,
                    &[],
                    &[],
                )
                .and_then(|_| {
                    inquiry::domain::check_domain(
                        "report",
                        1,
                        &json!({}),
                        article,
                        &accepted,
                        &[],
                        &[],
                    )
                });
                let (verdict,message) = match checked {
                    Err(error) => ("fail",error.message),
                    Ok(_) if article["answer_status"] != "answered" || analysis["decision"] != "ready" => ("fail","The saved research explicitly lacks sufficient evidence; preserve the article and gaps.".into()),
                    Ok(_) => ("pass","Pinned Inquiry analysis/report checks verified exact source versions, paragraph quotes, analysis lineage and quotation limits.".into()),
                };
                let result = json!({"protocol_version":1,"verdict":verdict,"results":[{"id":"citation_integrity","verdict":verdict,"message":message,"evidence_refs":input.evidence_refs}],"evidence_refs":input.evidence_refs,"violations":[]});
                tools
                    .call(
                        "artifact.write",
                        json!({"slot":"evaluation","value":result}),
                    )
                    .await?;
            }
        }
        Ok(())
    }
}
#[async_trait]
impl Executor for Native {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::MANAGED
    }
    fn enforces_json_candidate_limit(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        inv: Invocation,
        tools: Arc<dyn ToolBridge>,
        _: CancellationToken,
    ) -> ExecutionOutcome {
        match self.run(&inv, &tools).await {
            Ok(()) => ExecutionOutcome::Completed {
                usage: json!({"application":"pinned-inquiry","native_model":false}),
            },
            Err(error) => ExecutionOutcome::Failed {
                code: error.code,
                message: error.message,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Bridge {
        data: BTreeMap<String, ArtifactData>,
        evaluation: Mutex<Option<Value>>,
    }
    #[async_trait]
    impl ToolBridge for Bridge {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![]
        }
        async fn call(&self, tool: &str, args: Value) -> Result<Value> {
            match tool {
                "artifact.read" => {
                    if args
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        != vec!["id"]
                    {
                        return Err(fault(
                            "artifact.read uses id, not the domain read_source artifact_id parameter",
                        ));
                    }
                    let artifact = self
                        .data
                        .get(args["id"].as_str().unwrap())
                        .ok_or_else(|| fault("read outside exact targets"))?;
                    Ok(
                        json!({"artifact":artifact.reference,"text":std::str::from_utf8(&artifact.bytes).unwrap()}),
                    )
                }
                "artifact.write" => {
                    assert_eq!(args["slot"], "evaluation");
                    *self.evaluation.lock().unwrap() = Some(args["value"].clone());
                    Ok(json!({"written":true}))
                }
                _ => Err(fault("undeclared tool")),
            }
        }
    }
    fn add(
        data: &mut BTreeMap<String, ArtifactData>,
        id: &str,
        slot: &str,
        value: Value,
    ) -> CheckTarget {
        let bytes = serde_json::to_vec(&value).unwrap();
        let sha = format!("{:x}", Sha256::digest(&bytes));
        data.insert(
            id.into(),
            ArtifactData {
                reference: ArtifactRef {
                    id: id.into(),
                    sha256: sha.clone(),
                    slot: slot.into(),
                    state_id: "_tl.node.producer".into(),
                    visit: 1,
                },
                bytes: bytes.into(),
            },
        );
        CheckTarget {
            artifact_id: id.into(),
            sha256: sha,
            schema_digest: "a".repeat(64),
            value,
        }
    }
    fn fixture(insufficient: bool, bad_citation: bool) -> (Invocation, Arc<Bridge>) {
        let mut data = BTreeMap::new();
        let source1 = add(
            &mut data,
            "source-1",
            "capture.snapshot.r1s1",
            json!({"fact":{"usable":true,"final_url":"https://www.sqlite.org/wal.html"},"paragraphs":["The WAL file is part of the persistent state of the database."]}),
        );
        let source2 = add(
            &mut data,
            "source-2",
            "capture.snapshot.r1s2",
            json!({"fact":{"usable":true,"final_url":"https://www.sqlite.org/fts5.html"},"paragraphs":["Keep the content and index consistent."]}),
        );
        let citation = json!({"artifact_id":"source-1","sha256":if bad_citation {"b".repeat(64)}else{source1.sha256.clone()},"paragraph":0,"quote":"persistent state"});
        let mut evidence = citation.clone();
        evidence["id"] = json!("e1");
        let analysis = add(
            &mut data,
            "analysis",
            "analysis",
            json!({"evidence":[evidence],"judgments":[{"text":"WAL is persistent state","support":["e1"],"counter":[],"limitations":[]}],"gaps":[],"decision":if insufficient{"need_input"}else{"ready"}}),
        );
        let article = add(
            &mut data,
            "article",
            "report",
            json!({"answer_status":if insufficient{"insufficient_evidence"}else{"answered"},"conclusion":"WAL is persistent state.[1]","citations":[citation],"limitations":[]}),
        );
        let targets = BTreeMap::from([
            ("source1".into(), source1),
            ("source2".into(), source2),
            ("analysis".into(), analysis),
            ("article".into(), article),
        ]);
        let input=CheckInput{protocol_version:1,goal_id:"goal-test".into(),goal:"Verify saved sources".into(),goal_spec_digest:"a".repeat(64),check_id:"domain".into(),rules_digest:"a".repeat(64),phase:CheckInputPhase::GoalFinal,epoch:1,nonce:"nonce".into(),issued_revision:1,subject:CheckSubject::Artifacts{plan_digest:"a".repeat(64),targets},rules:serde_json::from_value(json!({"protocol_version":1,"id":"domain","version":"1","rules":[{"id":"citation_integrity","instruction":"Check exact citations","pass_condition":"Exact sources and sufficient evidence","evidence_required":true}]})).unwrap(),goal_inputs:BTreeMap::new(),evidence_refs:vec!["source-1".into(),"source-2".into(),"analysis".into(),"article".into()]};
        let entry:EntryContract=serde_json::from_value(json!({"instruction":"Domain checker","executor":"domain","skills":[],"tools":["artifact.read","artifact.write"],"input_schema":"_tl.check-input@1","requires":[],"outputs":{"evaluation":{"schema":"_tl.evaluation-candidate@1","required":true}},"gate":"_tl.schema@1","outcomes":["completed"]})).unwrap();
        (
            Invocation {
                stage_input: serde_json::to_value(input).unwrap(),
                execution_id: "goal-test".into(),
                run_id: "run-test".into(),
                state_id: "_tl.check.domain".into(),
                visit: 1,
                entry,
                skills: BTreeMap::new(),
                output_schemas: BTreeMap::new(),
                inputs: json!({}),
                artifacts: data.values().map(|a| a.reference.clone()).collect(),
                signal_facts: vec![],
                work_dir: "/tmp".into(),
                timeout_ms: 30000,
                budget: None,
            },
            Arc::new(Bridge {
                data,
                evaluation: Mutex::new(None),
            }),
        )
    }
    #[tokio::test]
    async fn pinned_domain_checker_uses_the_public_read_contract_and_classifies_actual_evidence() {
        for (insufficient, bad_citation, verdict) in [
            (false, false, "pass"),
            (true, false, "fail"),
            (false, true, "fail"),
        ] {
            let (inv, bridge) = fixture(insufficient, bad_citation);
            let tools: Arc<dyn ToolBridge> = bridge.clone();
            Native::DomainCheck.run(&inv, &tools).await.unwrap();
            let result = bridge.evaluation.lock().unwrap().clone().unwrap();
            assert_eq!(result["verdict"], verdict);
            assert_eq!(result["results"][0]["id"], "citation_integrity");
        }
    }
    #[tokio::test]
    #[ignore = "requires explicit retained public Inquiry evidence; never promotes the original failed Goal"]
    async fn retained_public_research_artifacts_are_classified_without_rewriting_the_original_goal()
    {
        let root = std::path::PathBuf::from(
            std::env::var("INQUIRY_EVIDENCE_ROOT").expect("retained evidence directory"),
        );
        let snapshot: GoalSnapshot =
            serde_json::from_slice(&std::fs::read(root.join("final-snapshot.json")).unwrap())
                .unwrap();
        let client = GoalClient::open(
            root.join("store"),
            Caller {
                principal: "integration-owner".into(),
                grants: vec!["integration".into()],
            },
        )
        .unwrap();
        let plan = client.plan(&snapshot.goal_id).unwrap();
        let mut data = BTreeMap::new();
        let mut targets = BTreeMap::new();
        for alias in ["source1", "source2", "analysis", "article"] {
            let output = &plan.proposal.outputs[alias];
            let reference = &snapshot.nodes[&output.node_id].outputs[&output.port];
            let artifact = client.read_artifact(&reference.id).unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(&artifact.bytes)),
                reference.sha256
            );
            targets.insert(
                alias.into(),
                CheckTarget {
                    artifact_id: reference.id.clone(),
                    sha256: reference.sha256.clone(),
                    schema_digest: "a".repeat(64),
                    value: parse_unique(&artifact.bytes).unwrap(),
                },
            );
            data.insert(reference.id.clone(), artifact);
        }
        let (mut invocation, _) = fixture(false, false);
        let mut input: CheckInput = serde_json::from_value(invocation.stage_input).unwrap();
        input.goal = snapshot.spec.goal.clone();
        input.goal_id = snapshot.goal_id.clone();
        input.goal_spec_digest = snapshot.spec_digest.clone();
        input.subject = CheckSubject::Artifacts {
            plan_digest: plan.plan_digest,
            targets,
        };
        input.evidence_refs = data.keys().cloned().collect();
        invocation.stage_input = serde_json::to_value(input).unwrap();
        invocation.artifacts = data.values().map(|a| a.reference.clone()).collect();
        let bridge = Arc::new(Bridge {
            data,
            evaluation: Mutex::new(None),
        });
        let tools: Arc<dyn ToolBridge> = bridge.clone();
        Native::DomainCheck.run(&invocation, &tools).await.unwrap();
        let evaluation = bridge.evaluation.lock().unwrap().clone().unwrap();
        let expected = std::env::var("INQUIRY_DOMAIN_EXPECTED").unwrap_or_else(|_| "pass".into());
        assert_eq!(evaluation["verdict"], expected, "{evaluation}");
        let unchanged = client.get(&snapshot.goal_id).unwrap();
        assert_eq!(unchanged.revision, snapshot.revision);
        assert_eq!(
            unchanged.outcome, snapshot.outcome,
            "offline checking must never rewrite a business outcome"
        );
    }
}
