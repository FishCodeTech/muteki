//! Worker evidence, report/summary and owner-fenced conclusion commit together.
use super::*;
impl SharedGraph {
    pub(crate) fn intent_products(&self) -> Result<HashMap<String, Vec<i64>>, String> {
        let mut query = self.conn.prepare("SELECT p.intent_id,p.fact_seq FROM intent_products p JOIN intents i ON i.intent_id=p.intent_id ORDER BY i.created_seq,p.fact_seq").map_err(|error|error.to_string())?;
        let rows=query.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?))).map_err(|error|error.to_string())?;
        let mut products=HashMap::<String,Vec<i64>>::new();
        for row in rows {let (intent,fact)=row.map_err(|error|error.to_string())?;products.entry(intent).or_default().push(fact);}
        Ok(products)
    }
    pub fn worker_result_receipt(&self,run:&str,worker:&str,intent:&str,epoch:u64,checkpoint:&str)->Result<Option<Value>,String>{
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS worker_result_receipts(identity_json TEXT PRIMARY KEY,request_json TEXT NOT NULL,response_json TEXT NOT NULL)").map_err(|e|e.to_string())?;
        let key=json!([run,worker,intent,epoch,checkpoint]).to_string();
        self.conn.query_row("SELECT response_json FROM worker_result_receipts WHERE identity_json=?1",[key],|row|row.get::<_,String>(0)).optional().map_err(|e|e.to_string())?.map(|raw|{let mut value:Value=serde_json::from_str(&raw).map_err(|e|e.to_string())?;value["deduplicated"]=json!(true);Ok(value)}).transpose()
    }
    pub fn worker_result_receipt_for_request(&self,run:&str,worker:&str,intent:&str,epoch:u64,checkpoint:&str,digest:&str)->Result<Option<Value>,String>{
        let receipt=self.worker_result_receipt(run,worker,intent,epoch,checkpoint)?;
        if let Some(receipt)=&receipt {
            let saved=receipt["request_digest"].as_str().ok_or("step.checkpoint.identity_unverified: legacy checkpoint has no original request digest; inspect its original receipt")?;
            if saved!=digest{return Err("step.checkpoint.identity_conflict: checkpoint was already committed with different input".into());}
        }
        Ok(receipt)
    }
    /// Older receipts retain the admitted normalized request and committed
    /// staging rows. Compare those original effects before pinning a retry's
    /// input digest; an upgrade must not invalidate an identical checkpoint.
    pub fn migrate_worker_result_request(&self,run:&str,worker:&str,intent:&str,epoch:u64,checkpoint:&str,input:&Value,digest:&str,finish:bool,solved:bool)->Result<(),String>{
        self.with_immediate_tx(||{
            let key=json!([run,worker,intent,epoch,checkpoint]).to_string();
            self.worker_result_receipt(run,worker,intent,epoch,checkpoint)?;
            let prior=self.conn.query_row("SELECT request_json,response_json FROM worker_result_receipts WHERE identity_json=?1",[&key],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).optional().map_err(|e|e.to_string())?;
            let Some((request,raw))=prior else{return Ok(())};
            let mut receipt:Value=serde_json::from_str(&raw).map_err(|e|e.to_string())?;
            if receipt["request_digest"].is_string(){return Ok(())}
            let saved:Value=serde_json::from_str(&request).map_err(|e|e.to_string())?;
            self.result_schema()?;
            let mut query=self.conn.prepare("SELECT kind,payload FROM worker_result_staging WHERE worker=?1 AND intent_id=?2 AND target_epoch=?3 AND committed=1 ORDER BY rowid").map_err(|e|e.to_string())?;
            let staged=query.query_map(params![worker,intent,epoch],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).map_err(|e|e.to_string())?.map(|row|{let(kind,raw)=row.map_err(|e|e.to_string())?;Ok((kind,serde_json::from_str::<Value>(&raw).map_err(|e|e.to_string())?))}).collect::<Result<Vec<_>,String>>()?;
            let committed_solved=if let Some(seq)=receipt["concluded_seq"].as_i64(){
                let raw=self.conn.query_row("SELECT payload FROM events WHERE seq=?1",[seq],|row|row.get::<_,String>(0)).optional().map_err(|e|e.to_string())?;
                raw.and_then(|raw|serde_json::from_str::<Value>(&raw).ok()).is_some_and(|event|event["result"]=="solved")
            }else{saved["status"]=="solved"};
            let matches=saved["request_digest"].as_str().map(|saved|saved==digest).unwrap_or_else(||legacy_request_matches(&saved,input,&staged,solved));
            if receipt["finish_worker"]!=finish||committed_solved!=solved||!matches{return Err("step.checkpoint.identity_conflict: checkpoint was already committed with different input".into());}
            receipt["request_digest"]=json!(digest);receipt["request_digest_origin"]=json!("legacy_normalized_request_and_staging");
            self.conn.execute("UPDATE worker_result_receipts SET response_json=?1 WHERE identity_json=?2 AND response_json=?3",params![receipt.to_string(),key,raw]).map_err(|e|e.to_string())?;Ok(())
        })
    }
    fn result_schema(&self)->Result<(),String>{
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS worker_result_staging(worker TEXT NOT NULL,intent_id TEXT NOT NULL,target_epoch INTEGER NOT NULL,request_id TEXT NOT NULL,kind TEXT NOT NULL,payload TEXT NOT NULL,committed INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(worker,intent_id,target_epoch,request_id));").map_err(|e|e.to_string())
    }
    pub fn stage_worker_result(&self,worker:&str,intent:&str,epoch:u64,request:&str,kind:&str,payload:&Value)->Result<(),String>{
        self.with_immediate_tx(||{
            self.result_schema()?;
            let owner=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM intents WHERE intent_id=?1 AND worker=?2 AND status='claimed')",params![intent,worker],|row|row.get::<_,bool>(0)).map_err(|e|e.to_string())?;
            if !owner{return Err("Worker staging no longer owns its Step".into());}
            let raw=payload.to_string();
            if let Some((saved_kind,saved))=self.conn.query_row("SELECT kind,payload FROM worker_result_staging WHERE worker=?1 AND intent_id=?2 AND target_epoch=?3 AND request_id=?4",params![worker,intent,epoch,request],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).optional().map_err(|e|e.to_string())?{
                return if saved_kind==kind && saved==raw{Ok(())}else{Err("Worker staging request identity was reused with different content".into())};
            }
            self.conn.execute("INSERT INTO worker_result_staging(worker,intent_id,target_epoch,request_id,kind,payload)VALUES(?1,?2,?3,?4,?5,?6)",params![worker,intent,epoch,request,kind,raw]).map_err(|e|e.to_string())?;Ok(())
        })
    }
    pub fn staged_request(&self,worker:&str,intent:&str,epoch:u64,request:&str)->Result<Option<(String,Value)>,String>{
        self.result_schema()?;
        self.conn.query_row("SELECT kind,payload FROM worker_result_staging WHERE worker=?1 AND intent_id=?2 AND target_epoch=?3 AND request_id=?4",params![worker,intent,epoch,request],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).optional().map_err(|e|e.to_string())?.map(|(kind,raw)|serde_json::from_str(&raw).map(|payload|(kind,payload)).map_err(|e|e.to_string())).transpose()
    }
    pub fn staged_worker_result(&self,worker:&str,intent:&str,epoch:u64)->Result<Vec<(String,Value)>,String>{
        self.result_schema()?;
        let mut stmt=self.conn.prepare("SELECT kind,payload FROM worker_result_staging WHERE worker=?1 AND intent_id=?2 AND target_epoch=?3 AND committed=0 ORDER BY rowid").map_err(|e|e.to_string())?;
        let rows=stmt.query_map(params![worker,intent,epoch],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
        // Python keeps one Fact draft; a new Fact also clears its report draft.
        // Retain request rows for idempotent ingress receipts, but publish only the latest draft.
        let latest_fact = rows.iter().rposition(|(kind, _)| kind == "observations");
        let latest_report = rows.iter().rposition(|(kind, _)| kind == "vulnerability_report");
        let filtered:Vec<(String,Value)>=rows.into_iter().enumerate().filter(|(index, (kind, _))| {
            match kind.as_str() {
                "observations" => Some(*index) == latest_fact,
                "vulnerability_report" => Some(*index) == latest_report && latest_fact.is_none_or(|fact| *index > fact),
                _ => true,
            }
        }).map(|(_, (kind,raw))|serde_json::from_str(&raw).map(|value|(kind,value)).map_err(|e|e.to_string())).collect::<Result<_,_>>()?;
        let mut result:Vec<(String,Value)>=Vec::new();
        for (kind,payload) in filtered {
            let prior=if kind=="dead_ends"{result.iter().position(|(old_kind,old)|old_kind=="dead_ends"&&(old["reason"]==payload["reason"]||old["tested_scope"]==payload["tested_scope"]&&old["observed_result"]==payload["observed_result"]))}else if kind=="need_input"{result.iter().position(|(old_kind,_)|old_kind=="need_input")}else{None};
            if let Some(index)=prior{result[index]=(kind,payload);}else{result.push((kind,payload));}
        }
        Ok(result)
    }

    pub fn conclude_intent_checked(&self,worker:&str,intent:&str,result:&str,reason:&str)->Result<i64,String>{
        self.with_immediate_tx(||{
            let row=self.conn.query_row("SELECT status,worker,concluded_seq,result FROM intents WHERE intent_id=?1",[intent],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,Option<i64>>(2)?,row.get::<_,Option<String>>(3)?))).optional().map_err(|e|e.to_string())?.ok_or("Step does not exist")?;
            if row.1.as_deref()!=Some(worker){return Err("Step result owner differs from its claimed Worker".into());}
            if row.0=="done" && row.2.is_some(){return Ok(row.2.unwrap());}
            if row.0!="claimed"{return Err("Step is no longer claimed".into());}
            let at=now_secs();let status=if result=="blocked"{"blocked"}else if result=="superseded"{"superseded"}else{"done"};
            let payload=json!({"intent_id":intent,"worker":worker,"result":result,"reason":reason});
            self.conn.execute("INSERT INTO events(ts,kind,actor,payload,verified,confidence)VALUES(?1,?2,?3,?4,1,1.0)",params![at,EV_INTENT_CONCLUDED,worker,payload.to_string()]).map_err(|e|e.to_string())?;let seq=self.conn.last_insert_rowid();
            if self.conn.execute("UPDATE intents SET status=?1,dispatch_state='closed',concluded_seq=?2,result=?3 WHERE intent_id=?4 AND worker=?5 AND status='claimed'",params![status,seq,result,intent,worker]).map_err(|e|e.to_string())?!=1{return Err("Step ownership changed during result settlement".into());}
            let facts=self.conn.query_row("SELECT COUNT(*) FROM intent_products WHERE intent_id=?1",[intent],|row|row.get::<_,i64>(0)).map_err(|e|e.to_string())?;
            let observations=self.conn.query_row("SELECT COUNT(*) FROM observations WHERE intent_id=?1 AND actor=?2",params![intent,worker],|row|row.get::<_,i64>(0)).map_err(|e|e.to_string())?;
            self.conn.execute("INSERT INTO value_receipts(intent_id,worker,conclude_result,observations_added,verified_facts_added,realized_value,concluded_seq,updated_ts)VALUES(?1,?2,?3,?4,?5,?6,?7,?8)ON CONFLICT(intent_id)DO UPDATE SET worker=excluded.worker,conclude_result=excluded.conclude_result,observations_added=excluded.observations_added,verified_facts_added=excluded.verified_facts_added,realized_value=excluded.realized_value,concluded_seq=excluded.concluded_seq,updated_ts=excluded.updated_ts",params![intent,worker,result,observations,facts,if result=="solved"{"terminal"}else if facts>0{"fact_gain"}else if observations>0{"observation_only"}else{"none"},seq,at]).map_err(|e|e.to_string())?;
            self.resolve_verifier_intent(worker,intent,result,reason)?;
            Ok(seq)
        })
    }
    pub fn commit_worker_result(&self,state:&AppState,run_id:&str,worker:&str,intent:&str,epoch:u64,checkpoint:&str,payload:&Value,finish:bool,solved:bool)->Result<Value,String>{
        self.with_immediate_tx(||{
            self.result_schema()?;
            self.conn.execute_batch("CREATE TABLE IF NOT EXISTS worker_result_receipts(identity_json TEXT PRIMARY KEY,request_json TEXT NOT NULL,response_json TEXT NOT NULL)").map_err(|e|e.to_string())?;
            let key=json!([run_id,worker,intent,epoch,checkpoint]).to_string();
            let request_digest=payload["request_digest"].as_str().ok_or("Worker result request digest missing")?;
            if let Some(receipt)=self.worker_result_receipt_for_request(run_id,worker,intent,epoch,checkpoint,request_digest)?{return Ok(receipt);}
            let owner=self.conn.query_row("SELECT worker,status FROM intents WHERE intent_id=?1",[intent],|row|Ok((row.get::<_,Option<String>>(0)?,row.get::<_,String>(1)?))).optional().map_err(|e|e.to_string())?.ok_or("Worker Step does not exist")?;
            if owner.0.as_deref()!=Some(worker) || owner.1!="claimed"{return Err("Worker result does not own the current Step".into());}
            let mut observations=Vec::new();let mut facts=Vec::new();let mut dead_ends=Vec::new();let mut pocs=Vec::new();let at=now_secs();
            for observation in payload["observations"].as_array().into_iter().flatten(){
                let text=observation["text"].as_str().filter(|text|!text.trim().is_empty()).ok_or("Worker observation text is required")?;
                let mut provenance=observation["provenance"].as_object().cloned().unwrap_or_default();provenance.insert("target_epoch".into(),json!(epoch));
                for field in ["subject","predicate","scope","canonical_key","mode"]{if let Some(value)=observation.get(field){provenance.insert(field.into(),value.clone());}}
                if let Some(value)=observation.get("object"){provenance.insert("object_value".into(),value.clone());}
                let artifact=provenance.get("artifact_id").and_then(Value::as_str).map(str::to_owned);let provenance=json!(provenance);
                let(obs,fact)=self.add_evidence_checked(worker,observation["title"].as_str().unwrap_or(""),text,artifact.as_deref(),observation["claim_verified"]==true,if observation["claim_verified"]==true{1.}else{0.4},Some(intent),None,Some(&provenance))?;
                observations.push(obs);if let Some(fact)=fact{facts.push(fact);}
            }
            let mut superseded_facts=Vec::new();
            let mut superseded_events=Vec::new();
            for old in payload["supersedes"].as_array().into_iter().flatten() {
                let old=old.as_i64().ok_or("Invalid superseded Fact identity")?;
                let replacement=*facts.last().ok_or("supersedes requires a verified replacement Fact")?;
                if old==replacement {continue;}
                let seq=self.pentest_decide_supersede_fact(worker,old,payload["result_detail"].as_str().unwrap_or(""),Some(replacement))?;
                superseded_facts.push(old);if seq>0{superseded_events.push(seq);}
            }
            for dead in payload["dead_ends"].as_array().into_iter().flatten(){let reason=dead["reason"].as_str().filter(|v|!v.trim().is_empty()).ok_or("Dead end reason missing")?;
                let body=json!({"reason":reason,"tested_scope":dead["tested_scope"],"observed_result":dead["observed_result"],"intent_id":intent,"target_epoch":epoch,"route_hash":dead["route_hash"],"coverage_key":dead["coverage_key"]});
                self.conn.execute("INSERT INTO events(ts,kind,actor,payload,verified,confidence)VALUES(?1,?2,?3,?4,1,1.0)",params![at,EV_DEAD_END,worker,body.to_string()]).map_err(|e|e.to_string())?;dead_ends.push(self.conn.last_insert_rowid());
            }
            for poc in payload["pocs"].as_array().into_iter().flatten(){let id=poc["poc_id"].as_str().filter(|v|!v.is_empty()).map(str::to_owned).unwrap_or_else(||format!("poc-{}",uuid::Uuid::new_v4().simple()));
                let mut body=poc.clone();body["poc_id"]=json!(id);body["intent_id"]=json!(intent);body["target_epoch"]=json!(epoch);
                self.conn.execute("INSERT INTO events(ts,kind,actor,payload,artifact_id,verified,confidence)VALUES(?1,?2,?3,?4,?5,1,1.0)",params![at,EV_POC_SAVED,worker,body.to_string(),poc["artifact_id"].as_str()]).map_err(|e|e.to_string())?;let seq=self.conn.last_insert_rowid();
                self.conn.execute("INSERT INTO pocs(poc_id,name,path,entry_command,verified_artifact_id,status,created_by,created_seq,updated_seq,note)VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8,?9)ON CONFLICT(poc_id)DO UPDATE SET name=excluded.name,path=excluded.path,entry_command=excluded.entry_command,verified_artifact_id=excluded.verified_artifact_id,status=excluded.status,updated_seq=excluded.updated_seq,note=excluded.note",params![id,poc["name"].as_str().unwrap_or("PoC"),poc["path"].as_str().unwrap_or(""),poc["entry_command"].as_str().unwrap_or(""),poc["artifact_id"].as_str(),poc["status"].as_str().unwrap_or("available"),worker,seq,poc["note"].as_str().unwrap_or("")]).map_err(|e|e.to_string())?;pocs.push(seq);
            }
            let mut stmt=self.conn.prepare("SELECT obs_seq,admitted_fact_seq FROM observations WHERE actor=?1 AND intent_id=?2 AND target_epoch=?3 ORDER BY obs_seq").map_err(|e|e.to_string())?;
            for row in stmt.query_map(params![worker,intent,epoch],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,Option<i64>>(1)?))).map_err(|e|e.to_string())?{let(obs,fact)=row.map_err(|e|e.to_string())?;observations.push(obs);if let Some(fact)=fact{facts.push(fact);}}
            observations.sort_unstable();observations.dedup();facts.sort_unstable();facts.dedup();
            let report_result=if let Some(report)=payload.get("vulnerability_report").filter(|report|!report.is_null()){
                Some(super::super::pentest::submit_report_in_graph(state,run_id,worker,report,self)?)
            }else{None};
            let mut summary=payload.clone();summary["report_result"]=json!(report_result);summary["worker"]=json!(worker);summary["intent_id"]=json!(intent);summary["target_epoch"]=json!(epoch);summary["checkpoint_id"]=json!(checkpoint);summary["checkpoint"]=json!(!finish);summary["finish_worker"]=json!(finish);summary["observation_seqs"]=json!(observations);summary["fact_seqs"]=json!(facts);summary["dead_end_seqs"]=json!(dead_ends);summary["poc_seqs"]=json!(pocs);summary["superseded_fact_seqs"]=json!(superseded_facts);summary["superseded_event_seqs"]=json!(superseded_events);
            self.conn.execute("INSERT INTO events(ts,kind,actor,payload,verified,confidence)VALUES(?1,?2,?3,?4,1,1.0)",params![at,EV_WORKER_RESULT_COMMITTED,worker,summary.to_string()]).map_err(|e|e.to_string())?;let seq=self.conn.last_insert_rowid();
            let conclude=if finish{Some(self.conclude_intent_checked(worker,intent,if solved{"solved"}else{payload["status"].as_str().unwrap_or("done")},payload["result_detail"].as_str().or_else(||payload["summary"].as_str()).unwrap_or("worker_result_committed"))?)}else{self.conn.execute("UPDATE intents SET lease_expires_at=?1 WHERE intent_id=?2 AND worker=?3 AND status='claimed'",params![at+600.,intent,worker]).map_err(|e|e.to_string())?;None};
            self.conn.execute("UPDATE worker_result_staging SET committed=1 WHERE worker=?1 AND intent_id=?2 AND target_epoch=?3 AND committed=0",params![worker,intent,epoch]).map_err(|e|e.to_string())?;
            let receipt=json!({"intent_id":intent,"request_digest":request_digest,"commit_seq":seq,"concluded_seq":conclude,"checkpoint":!finish,"finish_worker":finish,"observation_seqs":observations,"fact_seqs":facts,"dead_end_seqs":dead_ends,"poc_seqs":pocs,"superseded_fact_seqs":superseded_facts,"superseded_event_seqs":superseded_events,"handoff_missing":payload["handoff_missing"]==true,"deduplicated":false});
            self.conn.execute("INSERT INTO worker_result_receipts(identity_json,request_json,response_json)VALUES(?1,?2,?3)",params![key,payload.to_string(),receipt.to_string()]).map_err(|e|e.to_string())?;Ok(receipt)
        })
    }
}

fn legacy_effect_value(kind:&str,value:&Value)->Value{
    let mut value=value.clone();
    if let Some(fields)=value.as_object_mut(){
        if kind=="observations"{fields.remove("provenance");fields.remove("mode");}
        if kind=="pocs"{
            if fields.get("status").and_then(Value::as_str).is_some_and(|status|!matches!(status,"available"|"wip"|"directional"|"spent"|"quarantined")){fields.insert("status".into(),json!("available"));}
            if let Some(id)=fields.get("image_ref").and_then(|image|image.get("artifact_id")).cloned(){fields.insert("artifact_id".into(),id);}
        }
    }
    value
}
fn legacy_effect_matches(kind:&str,input:&Value,saved:&Value)->bool{
    let mut expected=legacy_effect_value(kind,input);
    if kind=="observations"&&expected.is_object()&&saved["mode"]=="pentest"&&saved["claim_verified"]==false&&saved["admission_error"].is_string(){
        expected["claim_verified"]=json!(false);expected["admission_error"]=saved["admission_error"].clone();
    }
    expected==legacy_effect_value(kind,saved)
}
fn legacy_request_matches(saved:&Value,input:&Value,staged:&[(String,Value)],solved:bool)->bool{
    let has_fact=saved["observations"].as_array().into_iter().flatten().enumerate().any(|(index,row)|{
        row["claim_verified"]==true || row["mode"]=="pentest"&&row["admission_error"].is_string()&&(
            input["observations"].get(index).is_some_and(|value|value["claim_verified"]==true)
            ||staged.iter().any(|(kind,draft)|input["handoff_missing"]!=true&&kind=="observations"&&draft["claim_verified"]==true&&{
                let mut draft=draft.clone();if let(Some(title),Some(text))=(draft["title"].as_str(),draft["text"].as_str()){draft["text"]=json!(format!("{title}\n{text}"));}legacy_effect_matches(kind,&draft,row)
            }))
    });
    let mut expected=input.clone();if let Some(fields)=expected.as_object_mut(){fields.remove("request_digest");}
    if expected["status"]=="done"{expected["status"]=json!(if solved{"solved"}else if !has_fact&&saved["dead_ends"].as_array().is_some_and(|rows|!rows.is_empty()){"dead_end"}else{"explored"});}
    let keys=saved.as_object().into_iter().flat_map(|v|v.keys()).chain(expected.as_object().into_iter().flat_map(|v|v.keys())).collect::<std::collections::BTreeSet<_>>();
    for key in keys{
        if matches!(key.as_str(),"request_digest"|"artifact_refs"){continue;}
        if key=="result_detail"&&input["status"]=="done"&&!has_fact{continue;}
        if matches!(key.as_str(),"observations"|"dead_ends"|"pocs"|"artifacts"){
            let Some(old)=saved[key].as_array()else{return false};let Some(new)=expected[key].as_array()else{return false};
            if new.len()>old.len()||!new.iter().zip(old).all(|(a,b)|legacy_effect_matches(key,a,b)){return false;}
            for extra in &old[new.len()..]{
                if !staged.iter().any(|(kind,draft)|kind==key&&{
                    let mut draft=draft.clone();
                    if kind=="observations"{if input["handoff_missing"]==true{draft["claim_verified"]=json!(false);}if let(Some(title),Some(text))=(draft["title"].as_str(),draft["text"].as_str()){draft["text"]=json!(format!("{title}\n{text}"));}}
                    legacy_effect_matches(key,&draft,extra)
                }){return false;}
            }
        }else if saved.get(key)!=expected.get(key){
            let staged_override=staged.iter().any(|(kind,draft)|{
                if matches!(key.as_str(),"vulnerability_report"|"need_input"){kind==key&&*draft==saved[key]}
                else if key=="supersedes"{kind=="observations"&&draft["supersedes"]==saved[key]}
                else if key=="result_detail"{kind=="observations"&&draft["title"]==saved[key]}
                else{false}
            });
            if !staged_override{return false;}
        }
    }
    true
}
