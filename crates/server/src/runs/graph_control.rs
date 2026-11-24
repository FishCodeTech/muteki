//! Decision resolution and its exact continuation share the graph transaction.
use super::*;
impl SharedGraph {
    pub fn open_decisions(&self)->Result<Vec<Value>,String>{
        let mut stmt=self.conn.prepare("SELECT request_id,worker,need,intent_id,route_hash,branch_id,kind,need_kind FROM hitl_requests WHERE status='open' ORDER BY created_seq,request_id").map_err(|e|e.to_string())?;
        stmt.query_map([],|row|Ok(json!({"request_id":row.get::<_,String>(0)?,"worker":row.get::<_,String>(1)?,"need":row.get::<_,String>(2)?,"intent_id":row.get::<_,String>(3)?,"route_hash":row.get::<_,String>(4)?,"branch_id":row.get::<_,String>(5)?,"kind":row.get::<_,String>(6)?,"need_kind":row.get::<_,String>(7)?}))).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())
    }
    pub fn settle_decisions(&self,command_id:&str,actor:&str,requests:&[Value],resolution:&str,dismiss:bool,continuation:Option<&str>)->Result<Value,String>{
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS decision_commands(command_id TEXT PRIMARY KEY,request_json TEXT NOT NULL,result_json TEXT NOT NULL);BEGIN IMMEDIATE").map_err(|e|e.to_string())?;
        let request=json!({"actor":actor,"requests":requests,"resolution":resolution,"dismiss":dismiss,"continuation":continuation});
        let result=(||{
            if let Some((old,result))=self.conn.query_row("SELECT request_json,result_json FROM decision_commands WHERE command_id=?1",[command_id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).optional().map_err(|e|e.to_string())?{
                if serde_json::from_str::<Value>(&old).ok().as_ref()!=Some(&request){return Err("Decision command identity conflict".into());}
                return serde_json::from_str::<Value>(&result).map_err(|e|e.to_string());
            }
            let at=now_secs();let mut ids=Vec::new();
            for selected in requests {
                let id=selected["request_id"].as_str().ok_or("Decision request ID missing")?;
                let status=self.conn.query_row("SELECT status FROM hitl_requests WHERE request_id=?1",[id],|row|row.get::<_,String>(0)).optional().map_err(|e|e.to_string())?;
                if status.as_deref()!=Some("open"){return Err(format!("Decision {id} is no longer open"));}
                let payload=json!({"request_id":id,"resolution":resolution,"resolution_kind":if dismiss{"dismissed"}else{"answered"},"source_command_id":command_id,"continuation_intent_id":continuation});
                self.conn.execute("INSERT INTO events(ts,kind,actor,payload,verified,confidence,dedupe_key)VALUES(?1,?2,?3,?4,0,1.0,?5)",params![at,EV_HITL_RESOLVED,actor,payload.to_string(),format!("decision:{command_id}:{id}")]).map_err(|e|e.to_string())?;
                let seq=self.conn.last_insert_rowid();
                self.conn.execute("UPDATE hitl_requests SET status='closed',resolution=?1,resolved_by=?2,updated_seq=?3,updated_ts=?4 WHERE request_id=?5 AND status='open'",params![resolution,actor,seq,at,id]).map_err(|e|e.to_string())?;
                if dismiss {
                    self.conn.execute("INSERT INTO events(ts,kind,actor,payload,verified,confidence,dedupe_key)VALUES(?1,?2,?3,?4,0,1.0,?5)",params![at,EV_DEAD_END,actor,json!({"reason":format!("Operator dismissed the ask «{}» without supplying it; do not re-raise",selected["need"].as_str().unwrap_or("")),"request_id":id,"source_command_id":command_id}).to_string(),format!("dismiss:{command_id}:{id}")]).map_err(|e|e.to_string())?;
                }else if let Some(continuation)=continuation {
                    let goal=format!("Continue the task after the operator answered decision {id}: {}. Use the reserved operator context for the reply. Original intent: {}.",selected["need"].as_str().unwrap_or(""),selected["intent_id"].as_str().unwrap_or(""));
                    self.conn.execute("INSERT INTO events(ts,kind,actor,payload,verified,confidence,dedupe_key)VALUES(?1,?2,?3,?4,0,1.0,?5)",params![at,EV_INTENT_PROPOSED,actor,json!({"intent_id":continuation,"goal":goal,"source_command_id":command_id,"decision_request_id":id}).to_string(),format!("decision-continuation:{command_id}")]).map_err(|e|e.to_string())?;
                    let seq=self.conn.last_insert_rowid();
                    self.conn.execute("INSERT INTO intents(intent_id,goal,worker_class,status,dispatch_state,route_hash,branch_id,priority,requested_priority,priority_reason,created_seq)VALUES(?1,?2,'code','open','active',?3,?4,'operator','operator','operator_decision',?5)",params![continuation,goal,selected["route_hash"].as_str().unwrap_or(""),selected["branch_id"].as_str().unwrap_or(""),seq]).map_err(|e|e.to_string())?;
                }
                ids.push(id.to_owned());
            }
            let result=json!({"request_ids":ids,"resolution_kind":if dismiss{"dismissed"}else{"answered"},"continuation_intent_id":continuation});
            self.conn.execute("INSERT INTO decision_commands(command_id,request_json,result_json)VALUES(?1,?2,?3)",params![command_id,request.to_string(),result.to_string()]).map_err(|e|e.to_string())?;Ok(result)
        })();
        match result{Ok(value)=>{self.conn.execute_batch("COMMIT").map_err(|e|e.to_string())?;Ok(value)},Err(error)=>{let _=self.conn.execute_batch("ROLLBACK");Err(error)}}
    }
}

const LEASES:[(&str,&str,&str);5]=[("intents","intent_id","worker"),("pocs","poc_id","owner"),("activity_locks","activity_key","owner"),("lane_locks","lane_key","owner"),("resource_locks","resource_key","holder_worker_id")];
impl SharedGraph {
    pub fn suspend_control_leases(&self,suspension:&str,workers:&[String],run_wide:bool)->Result<usize,String>{
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS lease_freeze_values(suspension_id TEXT NOT NULL,table_name TEXT NOT NULL,row_id TEXT NOT NULL,owner TEXT NOT NULL,remaining REAL NOT NULL,PRIMARY KEY(suspension_id,table_name,row_id));BEGIN IMMEDIATE").map_err(|e|e.to_string())?;
        let result=(||{let at=now_secs();let mut affected=0;
            for(table,key,owner)in LEASES {
                let mut stmt=self.conn.prepare(&format!("SELECT {key},{owner},lease_expires_at FROM {table} WHERE lease_expires_at>?1 AND lease_suspended_remaining_s IS NULL")).map_err(|e|e.to_string())?;
                let leases=stmt.query_map([at],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?.unwrap_or_default(),row.get::<_,f64>(2)?))).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;drop(stmt);
                for(id,holder,deadline)in leases {if !run_wide&&!workers.contains(&holder){continue;}
                    self.conn.execute("INSERT INTO lease_freeze_values(suspension_id,table_name,row_id,owner,remaining)VALUES(?1,?2,?3,?4,?5)",params![suspension,table,id,holder,(deadline-at).max(0.)]).map_err(|e|e.to_string())?;
                    affected += self.conn.execute(
                        &format!("UPDATE {table} SET lease_suspended_remaining_s=?1 WHERE {key}=?2 AND {owner}=?3"),
                        params![deadline - at, id, holder],
                    ).map_err(|error| error.to_string())?;
                }
            }Ok(affected)
        })();match result{Ok(value)=>{self.conn.execute_batch("COMMIT").map_err(|e|e.to_string())?;Ok(value)},Err(error)=>{let _=self.conn.execute_batch("ROLLBACK");Err(error)}}
    }
    pub fn resume_control_leases(&self,suspension:&str)->Result<usize,String>{
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS lease_freeze_values(suspension_id TEXT NOT NULL,table_name TEXT NOT NULL,row_id TEXT NOT NULL,owner TEXT NOT NULL,remaining REAL NOT NULL,PRIMARY KEY(suspension_id,table_name,row_id));BEGIN IMMEDIATE").map_err(|e|e.to_string())?;
        let result=(||{let at=now_secs();let mut stmt=self.conn.prepare("SELECT table_name,row_id,owner,remaining FROM lease_freeze_values WHERE suspension_id=?1").map_err(|e|e.to_string())?;
            let rows=stmt.query_map([suspension],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,f64>(3)?))).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;drop(stmt);let mut affected=0;
            for(table,id,holder,remaining)in rows{let Some((_,key,owner))=LEASES.iter().find(|(name,_,_)|*name==table)else{return Err("Invalid lease suspension table".into())};
                affected+=self.conn.execute(&format!("UPDATE {table} SET lease_expires_at=?1,lease_suspended_remaining_s=NULL WHERE {key}=?2 AND {owner}=?3 AND lease_suspended_remaining_s IS NOT NULL"),params![at+remaining.max(0.),id,holder]).map_err(|e|e.to_string())?;
            }
            self.conn.execute("DELETE FROM lease_freeze_values WHERE suspension_id=?1",[suspension]).map_err(|e|e.to_string())?;Ok(affected)
        })();match result{Ok(value)=>{self.conn.execute_batch("COMMIT").map_err(|e|e.to_string())?;Ok(value)},Err(error)=>{let _=self.conn.execute_batch("ROLLBACK");Err(error)}}
    }
}
