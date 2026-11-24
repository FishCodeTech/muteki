//! Observations, admitted Facts and their links share one savepoint.
use super::*;
use unicode_normalization::UnicodeNormalization;
use unicode_casefold::UnicodeCaseFold;
fn normalized(text:&str)->String{
    let normalized:String=text.nfkc().collect();
    let prefix=regex::Regex::new(r"(?i)^\[[a-z0-9 _.-]{1,40}\]\s*").unwrap();
    let text=prefix.replace(&normalized,"");
    let text:String=text.chars().map(|ch|match ch{'“'|'”'=>'"','‘'|'’'=>'\'','—'|'–'=>'-','：'=>':','，'=>',','。'=>'.',ch=>ch}).collect();
    canonical_fact_whitespace(&text).trim_matches(|ch:char|ch.is_whitespace() || ".,;:!".contains(ch)).case_fold().collect()
}
fn identity(fact:&str,provenance:&Value)->Value {
    let epoch=provenance.get("target_epoch").cloned().unwrap_or_else(||json!("legacy"));
    let mode=provenance["mode"].as_str().unwrap_or("pentest");
    let text=if mode=="ctf"{fact.trim().lines().map(canonical_fact_whitespace).collect::<Vec<_>>().join("\n")}else{normalized(fact)};
    let key=if mode=="ctf"{json!(["text",text])} else if let Some(key)=provenance["canonical_key"].as_str().filter(|v|!v.trim().is_empty()){json!(["canonical",normalized(key)])}
        else if provenance["subject"].as_str().is_some_and(|v|!v.trim().is_empty()) && provenance["predicate"].as_str().is_some_and(|v|!v.trim().is_empty()){json!(["structured",normalized(provenance["subject"].as_str().unwrap()),normalized(provenance["predicate"].as_str().unwrap()),provenance["object_value"],provenance["scope"]])}
        else{json!(["text",text])};
    json!([mode,epoch,key])
}
impl SharedGraph {
    pub fn add_evidence_checked(&self,actor:&str,title:&str,fact:&str,artifact_id:Option<&str>,verified:bool,confidence:f64,intent_id:Option<&str>,route_hash:Option<&str>,provenance:Option<&Value>)->Result<(i64,Option<i64>),String>{
        if actor.trim().is_empty() || fact.trim().is_empty() || !confidence.is_finite() || !(0.0..=1.0).contains(&confidence){return Err("Invalid evidence actor, content or confidence".into());}
        self.conn.execute_batch("SAVEPOINT evidence_commit").map_err(|e|e.to_string())?;
        let result=(||{
            self.conn.execute_batch("CREATE TABLE IF NOT EXISTS observation_identities(identity_json TEXT PRIMARY KEY,obs_seq INTEGER NOT NULL UNIQUE);CREATE TABLE IF NOT EXISTS fact_identities(identity_json TEXT PRIMARY KEY,fact_seq INTEGER NOT NULL)").map_err(|e|e.to_string())?;
            let provenance=provenance.cloned().unwrap_or_else(||json!({}));let iid=intent_id.unwrap_or("");let at=now_secs();let route=normalize_route_hash(route_hash.unwrap_or(""));let fact_identity=identity(fact,&provenance);
            let observation_identity=json!([actor,iid,fact_identity,verified,provenance["tool_event_seq"],provenance["artifact_sha256"],provenance["artifact_refs"],provenance["enables"]]).to_string();
            if let Some((seq,admitted))=self.conn.query_row("SELECT o.obs_seq,o.admitted_fact_seq FROM observation_identities k JOIN observations o USING(obs_seq) WHERE k.identity_json=?1",[&observation_identity],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,Option<i64>>(1)?))).optional().map_err(|e|e.to_string())?{return Ok((seq,admitted));}
            let summary=if title.trim().is_empty(){fact.trim()}else{title.trim()};
            let payload=json!({"source":title,"summary":summary,"detail":fact.trim(),"fact":fact.trim(),"intent_id":iid,"route_hash":route,"artifact_id":artifact_id,"target_epoch":provenance["target_epoch"],"claimed_verified":verified,"admitted":verified,"evidence_provenance":provenance});
            self.conn.execute("INSERT INTO events(ts,kind,actor,payload,artifact_id,verified,confidence,dedupe_key)VALUES(?1,?2,?3,?4,?5,?6,?7,NULL)",params![at,EV_OBSERVATION_ADDED,actor,payload.to_string(),artifact_id,verified,confidence]).map_err(|e|e.to_string())?;let observation=self.conn.last_insert_rowid();
            self.conn.execute("INSERT INTO observations(obs_seq,actor,summary,detail,artifact_id,evidence_json,confidence,intent_id,route_hash,target_epoch,status,created_ts,updated_ts)VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)",params![observation,actor,summary,fact.trim(),artifact_id,payload.to_string(),confidence,iid,route,provenance["target_epoch"].as_u64().unwrap_or(1),if verified{"admitted"}else{"recorded"},at]).map_err(|e|e.to_string())?;
            self.conn.execute("INSERT INTO observation_identities(identity_json,obs_seq)VALUES(?1,?2)",params![observation_identity,observation]).map_err(|e|e.to_string())?;
            if !verified{return Ok((observation,None));}
            let canonical=fact_identity.to_string();
            let mut prior=self.conn.prepare("SELECT e.seq,e.payload FROM events e LEFT JOIN fact_states s ON s.fact_seq=e.seq WHERE e.kind=?1 AND e.verified=1 AND COALESCE(s.status,'active') IN('active','verified','revalidated') ORDER BY e.seq").map_err(|e|e.to_string())?;
            let prior=prior.query_map([EV_FACT_ADDED],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?))).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
            for(seq,raw)in prior{let row:Value=serde_json::from_str(&raw).map_err(|e|e.to_string())?;let mut old=row["evidence_provenance"].clone();if !old.is_object(){old=json!({});}if old["target_epoch"].is_null(){old["target_epoch"]=row["target_epoch"].clone();}if old["mode"].is_null(){old["mode"]=provenance["mode"].clone();}
                let text=row["fact"].as_str().or_else(||row["detail"].as_str()).unwrap_or("");
                self.conn.execute("INSERT OR IGNORE INTO fact_identities(identity_json,fact_seq)VALUES(?1,?2)",params![identity(text,&old).to_string(),seq]).map_err(|e|e.to_string())?;
            }

            let existing=self.conn.query_row("SELECT k.fact_seq FROM fact_identities k JOIN events e ON e.seq=k.fact_seq LEFT JOIN fact_states s ON s.fact_seq=e.seq WHERE k.identity_json=?1 AND COALESCE(s.status,'active') IN('active','verified','revalidated')",[&canonical],|row|row.get::<_,i64>(0)).optional().map_err(|e|e.to_string())?;
            let admitted=if let Some(seq)=existing{seq}else{
                let payload=json!({"fact":fact.trim(),"summary":summary,"obs_seq":observation,"intent_id":iid,"route_hash":route,"source_solver":actor,"verifier":actor,"target_epoch":provenance["target_epoch"],"canonical_identity":fact_identity,"evidence_provenance":provenance});
                self.conn.execute("INSERT INTO events(ts,kind,actor,payload,artifact_id,verified,confidence,dedupe_key)VALUES(?1,?2,?3,?4,?5,1,?6,NULL)",params![at,EV_FACT_ADDED,actor,payload.to_string(),artifact_id,confidence]).map_err(|e|e.to_string())?;let seq=self.conn.last_insert_rowid();
                self.conn.execute("INSERT INTO fact_states(fact_seq,status,updated_by,updated_seq,updated_ts)VALUES(?1,'active',?2,?1,?3)",params![seq,actor,at]).map_err(|e|e.to_string())?;
                self.conn.execute("INSERT INTO fact_identities(identity_json,fact_seq)VALUES(?1,?2)ON CONFLICT(identity_json)DO UPDATE SET fact_seq=excluded.fact_seq",params![canonical,seq]).map_err(|e|e.to_string())?;seq
            };
            self.conn.execute("UPDATE observations SET admitted_fact_seq=?1,status='admitted' WHERE obs_seq=?2",params![admitted,observation]).map_err(|e|e.to_string())?;
            self.conn.execute("INSERT INTO fact_evidence(fact_seq,obs_seq,relation,created_seq)VALUES(?1,?2,'supports',?2)",params![admitted,observation]).map_err(|e|e.to_string())?;
            if !iid.is_empty(){self.conn.execute("INSERT OR IGNORE INTO intent_products(intent_id,fact_seq)VALUES(?1,?2)",params![iid,admitted]).map_err(|e|e.to_string())?;}
            Ok((observation,Some(admitted)))
        })();
        match result{Ok(result)=>{self.conn.execute_batch("RELEASE evidence_commit").map_err(|e|e.to_string())?;Ok(result)},Err(error)=>{let _=self.conn.execute_batch("ROLLBACK TO evidence_commit;RELEASE evidence_commit");Err(error)}}
    }
}
