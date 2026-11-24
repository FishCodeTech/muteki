//! Claim and retirement publish events only with their corresponding state change.
use super::*;
impl SharedGraph {
    pub fn claim_intent_checked(&self,worker:&str,intent:&str,lease:f64)->Result<bool,String>{
        if worker.is_empty() || !lease.is_finite() || lease<=0.{return Err("Invalid claim owner or lease".into());}
        self.conn.execute_batch("SAVEPOINT claim_intent").map_err(|e|e.to_string())?;
        let result=(||{
            let at=now_secs();let expiry=at+lease.max(30.);
            let changed=self.conn.execute("UPDATE intents SET status='claimed',worker=?1,claimed_ts=?2,lease_expires_at=?3 WHERE intent_id=?4 AND status='open' AND dispatch_state='active'
                AND NOT EXISTS(SELECT 1 FROM intent_dependencies d LEFT JOIN intents p ON p.intent_id=d.depends_on_intent_id WHERE d.intent_id=intents.intent_id AND ((p.intent_id IS NULL AND d.depends_on_intent_id GLOB '*[^0-9]*') OR (p.intent_id IS NOT NULL AND (p.status<>'done' OR p.dispatch_state<>'closed'))))
                AND NOT EXISTS(SELECT 1 FROM json_each(intents.required_pocs_json) r LEFT JOIN pocs p ON p.poc_id=r.value WHERE p.poc_id IS NULL OR p.status NOT IN('available','directional','wip'))",params![worker,at,expiry,intent]).map_err(|e|e.to_string())?;
            if changed==0{return Ok(false);}
            let payload=json!({"intent_id":intent,"worker":worker,"lease_expires_at":expiry});
            self.conn.execute("INSERT INTO events(ts,kind,actor,payload,verified,confidence)VALUES(?1,?2,?3,?4,1,1.0)",params![at,EV_INTENT_CLAIMED,worker,payload.to_string()]).map_err(|e|e.to_string())?;let seq=self.conn.last_insert_rowid();
            self.conn.execute("UPDATE intents SET claimed_seq=?1 WHERE intent_id=?2 AND worker=?3",params![seq,intent,worker]).map_err(|e|e.to_string())?;Ok(true)
        })();
        match result{Ok(result)=>{self.conn.execute_batch("RELEASE claim_intent").map_err(|e|e.to_string())?;Ok(result)},Err(error)=>{let _=self.conn.execute_batch("ROLLBACK TO claim_intent;RELEASE claim_intent");Err(error)}}
    }
    pub fn compact_graph_checked(&self,actor:&str,summary:&str)->Result<(i64,Vec<String>),String>{
        self.conn.execute_batch("SAVEPOINT compact_graph").map_err(|e|e.to_string())?;
        let result=(||{
            let cutoff=self.conn.query_row("SELECT COALESCE(MAX(seq),0) FROM events",[],|row|row.get::<_,i64>(0)).map_err(|e|e.to_string())?;let at=now_secs();let id=format!("compact-{}",uuid::Uuid::new_v4().simple());
            let retired={let mut stmt=self.conn.prepare("SELECT i.intent_id FROM intents i WHERE i.status<>'claimed' AND NOT EXISTS(SELECT 1 FROM intent_products p WHERE p.intent_id=i.intent_id) AND((i.status='done' AND i.dispatch_state='closed') OR i.dispatch_state='resume') ORDER BY i.created_seq,i.intent_id").map_err(|e|e.to_string())?;stmt.query_map([],|row|row.get::<_,String>(0)).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?};
            let payload=json!({"compact_id":id,"before_seq":cutoff,"cutoff_seq":cutoff,"summary":summary,"retired_intent_ids":retired});
            self.conn.execute("INSERT INTO events(ts,kind,actor,payload,verified,confidence)VALUES(?1,?2,?3,?4,1,1.0)",params![at,EV_GRAPH_COMPACTED,actor,payload.to_string()]).map_err(|e|e.to_string())?;let seq=self.conn.last_insert_rowid();
            self.conn.execute("INSERT INTO compact_epochs(before_seq,summary_seq,summary_text,actor,created_ts)VALUES(?1,?2,?3,?4,?5)",params![cutoff,seq,summary,actor,at]).map_err(|e|e.to_string())?;
            for intent in &retired{
                self.conn.execute("UPDATE intents SET dispatch_state='retired',compact_id=?1 WHERE intent_id=?2",params![seq,intent]).map_err(|e|e.to_string())?;
                let payload=json!({"intent_id":intent,"dispatch_state":"retired","compact_id":id});self.conn.execute("INSERT INTO events(ts,kind,actor,payload,verified,confidence)VALUES(?1,?2,?3,?4,1,1.0)",params![at,EV_INTENT_STATE_CHANGED,actor,payload.to_string()]).map_err(|e|e.to_string())?;
            }Ok((seq,retired))
        })();
        match result{Ok(result)=>{self.conn.execute_batch("RELEASE compact_graph").map_err(|e|e.to_string())?;Ok(result)},Err(error)=>{let _=self.conn.execute_batch("ROLLBACK TO compact_graph;RELEASE compact_graph");Err(error)}}
    }
}
