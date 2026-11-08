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
                        .call("artifact.read", json!({"artifact_id":target.artifact_id}))
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
