//! Persist the evidence-backed solve template after a successful CTF Run.
use super::*;
fn safe(value:&str)->String{let out=value.chars().map(|ch|if ch.is_ascii_alphanumeric() || matches!(ch,'_'|'-'){ch}else{'_'}).collect::<String>();if out.is_empty(){"template".into()}else{out}}
pub(super) fn save(state:&AppState,run_id:&str,winner:Option<&str>)->Result<PathBuf,String>{
    let run=state.store.get("runs",run_id).map_err(|e|e.to_string())?.ok_or("Solved Run not found")?;
    let graph=graph::SharedGraph::open(state,run_id)?;let events=graph.events();
    let active=graph.active_facts().into_iter().filter_map(|fact|fact["fact_seq"].as_i64()).collect::<HashSet<_>>();
    let facts=events.iter().filter(|fact|fact["kind"]=="fact_added" && fact["verified"]==true && fact["seq"].as_i64().is_some_and(|seq|active.contains(&seq))).collect::<Vec<_>>();
    let chain=|selected:Vec<&Value>|{let mut seen=HashSet::new();selected.into_iter().filter_map(|fact|fact["payload"]["fact"].as_str().map(str::trim)).filter(|text|!text.is_empty() && seen.insert(text.to_string())).take(12).map(str::to_owned).collect::<Vec<_>>()};
    let evidence_chain=chain(facts.clone());let mut steps=Vec::new();
    for intent in graph.all_intents().into_iter().filter(|intent|intent["status"]=="done" && intent["result"]=="solved") {if let Some(goal)=intent["goal"].as_str().filter(|goal|!goal.trim().is_empty()){if !steps.iter().any(|step|step==goal){steps.push(goal.to_owned());}}}
    for fact in &evidence_chain{if !steps.contains(fact){steps.push(fact.clone());}}steps.truncate(12);
    let flags=run["flags"].as_array().cloned().unwrap_or_default();let mut flag_chains=serde_json::Map::new();
    if flags.len()>1 {
        for flag in &flags {
            let Some(found)=events.iter().rev().find(|event|event["kind"]=="flag_found" && event["payload"]["flag"]==*flag)else{continue};
            let mut required=HashSet::new();let mut pending=vec![found["payload"]["intent_id"].as_str().unwrap_or("").to_owned()];let mut intents=HashSet::new();
            while let Some(intent)=pending.pop(){if intent.is_empty() || !intents.insert(intent.clone()){continue;}
                for seq in graph.intent_source_seqs(&intent){if required.insert(seq){if let Some(fact)=facts.iter().find(|fact|fact["seq"]==seq){if let Some(parent)=fact["payload"]["intent_id"].as_str(){pending.push(parent.into());}}}}
                for fact in &facts{if fact["payload"]["intent_id"]==intent{if let Some(seq)=fact["seq"].as_i64(){required.insert(seq);}}}
            }
            let selected=if required.is_empty(){facts.iter().copied().filter(|fact|fact["seq"].as_i64().unwrap_or(0)<=found["seq"].as_i64().unwrap_or(0)).collect()}else{facts.iter().copied().filter(|fact|fact["seq"].as_i64().is_some_and(|seq|required.contains(&seq))).collect()};
            if let Some(flag)=flag.as_str(){flag_chains.insert(flag.into(),json!(chain(selected)));}
        }
    }
    let text=format!("{} {}",run["name"].as_str().unwrap_or(run_id),run["description"].as_str().unwrap_or("")).to_lowercase();
    let words=regex::Regex::new(r"[a-zA-Z][a-zA-Z0-9_]{2,}").map_err(|e|e.to_string())?;let stop=["the","and","this","that","find","flag","challenge","you","your","with","for","via","get","post","http","https","www","com","are"];
    let mut keywords=Vec::new();for token in words.find_iter(&text).map(|word|word.as_str()){if !stop.contains(&token) && !keywords.iter().any(|word|word==token){keywords.push(token.to_owned());if keywords.len()==12{break;}}}
    let mut template=json!({"name":run["name"].as_str().unwrap_or(run_id),"category":run["category"].as_str().unwrap_or("misc"),"keywords":keywords,"steps":steps,"evidence_chain":evidence_chain,"success_format":run["flag_format"],"source":format!("{run_id}{}",winner.map(|winner|format!(" (solver {winner})")).unwrap_or_default()),"notes":""});
    if !flag_chains.is_empty(){template["flag_evidence_chains"]=json!(flag_chains);}
    let root=std::env::var_os("MUTEKI_KNOWLEDGE_DIR").map(PathBuf::from).unwrap_or_else(||native_chat::state_dir(state).join("knowledge"));
    fs::create_dir_all(&root).map_err(|e|e.to_string())?;
    let path=root.join(format!("{}__{}.yaml",safe(run["category"].as_str().unwrap_or("misc")),safe(run["name"].as_str().unwrap_or(run_id))));
    let bytes=serde_yaml::to_string(&template).map_err(|e|e.to_string())?;worker::write_atomic_bytes(&path,bytes.as_bytes()).map_err(|e|e.to_string())?;
    Ok(path)
}
