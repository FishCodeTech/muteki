//! A raw HTTP listener keeps the exact request bytes outside normalized app routing.
use super::*;
use std::{collections::HashMap,sync::{Arc,OnceLock},time::Duration};
use tokio::{io::{AsyncReadExt,AsyncWriteExt},net::{TcpListener,TcpStream},sync::Mutex};
struct Receiver{url:String,host:String,port:u16,task:tokio::task::AbortHandle}
static RECEIVERS:OnceLock<Mutex<HashMap<String,Receiver>>>=OnceLock::new();
pub(super) async fn start(state:Arc<AppState>,host:&str,port:u16)->Api<Value>{
    let key=format!("{}:{}",crate::native_chat::state_dir(&state).display(),state.chat.epoch());
    let mut rows=RECEIVERS.get_or_init(||Mutex::new(HashMap::new())).lock().await;
    if let Some(receiver)=rows.get(&key).filter(|receiver|!receiver.task.is_finished()){return Ok(json!({"state":"ready","url":receiver.url,"base_url":receiver.url,"host":receiver.host,"port":receiver.port,"capture":"raw-http","protocols":["http"],"external_reachability":"unverified"}));}
    let listener=TcpListener::bind((host,port)).await.map_err(|error|Error::new(StatusCode::SERVICE_UNAVAILABLE,"capability_unavailable",format!("Local OAST receiver bind failed: {error}")))?;
    let actual=listener.local_addr().map_err(crate::runs::src_api::db)?;let address=if actual.ip().is_ipv6(){format!("[{}]",actual.ip())}else{actual.ip().to_string()};let url=format!("http://{address}:{}",actual.port());
    let weak=Arc::downgrade(&state);let task=tokio::spawn(async move{
        let slots=Arc::new(tokio::sync::Semaphore::new(128));let mut tick=tokio::time::interval(Duration::from_secs(1));
        loop{tokio::select!{
            _=tick.tick()=>{if weak.strong_count()==0{break;}},
            accepted=listener.accept()=>{let Ok((stream,peer))=accepted else{break};let Some(state)=weak.upgrade()else{break};let Ok(slot)=slots.clone().try_acquire_owned()else{drop(stream);continue};tokio::spawn(async move{let _slot=slot;let _=serve(state,stream,&peer.to_string()).await;});}
        }}
    }).abort_handle();
    if let Err(error)=state.store.put("system","oast_receiver_url",&json!(url)){task.abort();return Err(crate::runs::src_api::db(error));}
    let result=json!({"state":"ready","url":url,"base_url":url,"host":actual.ip().to_string(),"port":actual.port(),"capture":"raw-http","protocols":["http"],"external_reachability":"unverified"});
    rows.insert(key,Receiver{url,host:actual.ip().to_string(),port:actual.port(),task});Ok(result)
}
async fn read_request(stream:&mut TcpStream)->Result<(Vec<u8>,String,String,HeaderMap),String>{
    let mut raw=Vec::new();let mut chunk=[0u8;8192];let header_end;
    loop{
        if let Some(end)=raw.windows(4).position(|bytes|bytes==b"\r\n\r\n"){header_end=end+4;break;}
        if raw.len()>65536{return Err("HTTP header exceeds 64 KiB".into());}
        let n=stream.read(&mut chunk).await.map_err(|e|e.to_string())?;if n==0{return Err("HTTP request ended before its headers".into());}raw.extend_from_slice(&chunk[..n]);
    }
    if header_end>65536{return Err("HTTP header exceeds 64 KiB".into());}
    let(method,path,headers,length,chunked)={
        let mut fields=[httparse::EMPTY_HEADER;256];let mut request=httparse::Request::new(&mut fields);
        if !request.parse(&raw[..header_end]).map_err(|e|e.to_string())?.is_complete(){return Err("HTTP header is incomplete".into());}
        let mut headers=HeaderMap::new();let mut length=None;let mut transfer=Vec::new();
        for field in request.headers.iter(){
            if field.name.eq_ignore_ascii_case("transfer-encoding"){transfer.extend(std::str::from_utf8(field.value).map_err(|_|"Invalid Transfer-Encoding")?.split(',').map(|value|value.trim().to_ascii_lowercase()));}
            if field.name.eq_ignore_ascii_case("content-length"){
                let value=std::str::from_utf8(field.value).map_err(|e|e.to_string())?.parse::<usize>().map_err(|e|e.to_string())?;
                if length.replace(value).is_some(){return Err("Duplicate Content-Length".into());}
            }
            let name=axum::http::HeaderName::from_bytes(field.name.as_bytes()).map_err(|e|e.to_string())?;let value=axum::http::HeaderValue::from_bytes(field.value).map_err(|e|e.to_string())?;headers.append(name,value);
        }
        if !transfer.is_empty()&&(length.is_some()||(transfer.len()!=1||transfer[0]!="chunked")){return Err("Unsupported or ambiguous HTTP body framing".into())}
        (request.method.ok_or("HTTP method missing")?.to_owned(),request.path.ok_or("HTTP target missing")?.to_owned(),headers,length.unwrap_or(0),!transfer.is_empty())
    };
    if length>32*1024*1024{return Err("Callback body exceeds 32 MiB".into());}
    if headers.get("expect").is_some_and(|value|value.as_bytes().eq_ignore_ascii_case(b"100-continue")){stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await.map_err(|error|error.to_string())?;}
    async fn fill(stream:&mut TcpStream,raw:&mut Vec<u8>,total:usize)->Result<(),String>{if total>34*1024*1024{return Err("HTTP capture exceeds limit".into())}let mut chunk=[0u8;8192];while raw.len()<total{let cap=(total-raw.len()).min(chunk.len());let n=stream.read(&mut chunk[..cap]).await.map_err(|error|error.to_string())?;if n==0{return Err("HTTP body ended early".into())}raw.extend_from_slice(&chunk[..n]);}Ok(())}
    async fn line_end(stream:&mut TcpStream,raw:&mut Vec<u8>,start:usize)->Result<usize,String>{loop{if let Some(index)=raw.get(start..).unwrap_or(&[]).windows(2).position(|bytes|bytes==b"\r\n"){if index>8192{return Err("HTTP chunk line exceeds limit".into())}return Ok(start+index)}if raw.len().saturating_sub(start)>8192{return Err("HTTP chunk line exceeds limit".into())}let required=raw.len()+1;fill(stream,raw,required).await?;}}
    let total=if chunked{
        let mut offset=header_end;let mut body_size=0usize;
        loop{let end=line_end(stream,&mut raw,offset).await?;let text=std::str::from_utf8(&raw[offset..end]).map_err(|_|"Invalid chunk size")?.split(';').next().unwrap_or("");let size=usize::from_str_radix(text,16).map_err(|_|"Invalid chunk size")?;offset=end+2;
            if size==0{let trailer_start=offset;loop{let end=line_end(stream,&mut raw,offset).await?;if end-trailer_start>65536{return Err("HTTP trailers exceed limit".into())}if end==offset{break}if !raw[offset..end].contains(&b':'){return Err("Malformed HTTP trailer".into())}offset=end+2;}break offset+2;}
            body_size=body_size.checked_add(size).ok_or("Chunk length overflow")?;if body_size>32*1024*1024{return Err("Callback body exceeds 32 MiB".into())}
            let end=offset.checked_add(size).and_then(|value|value.checked_add(2)).ok_or("Chunk length overflow")?;fill(stream,&mut raw,end).await?;if &raw[end-2..end]!=b"\r\n"{return Err("Invalid chunk terminator".into())}offset=end;
        }
    }else{let total=header_end+length;fill(stream,&mut raw,total).await?;total};
    raw.truncate(total);Ok((raw,method,path,headers))
}
async fn serve(state:Arc<AppState>,mut stream:TcpStream,peer:&str)->Result<(),String>{
    let request=tokio::time::timeout(Duration::from_secs(45),read_request(&mut stream)).await;
    let(status,body)=match request{
        Ok(Ok((raw,method,path,headers)))if path=="/health"=>{let _=(raw,method,headers);("200 OK",json!({"state":"ready","capture":"raw-http"}))},
        Ok(Ok((raw,method,path,headers)))=>{
            let token=path.strip_prefix('/').unwrap_or("").split(['/', '?']).next().unwrap_or("");
            match super::callback_raw(&state,token,&method,&path,&headers,&raw,peer,"raw_wire"){
                Ok(())=>("200 OK",json!({"accepted":true})),Err(error)if error.status==StatusCode::NOT_FOUND=>("404 Not Found",json!({"error":error.code})),Err(error)=>("400 Bad Request",json!({"error":error.code}))
            }
        },_ =>("400 Bad Request",json!({"error":"invalid_request"}))
    };
    let body=body.to_string();let reply=format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
    tokio::time::timeout(Duration::from_secs(10),stream.write_all(reply.as_bytes())).await.map_err(|_|"Callback response timed out".to_owned())?.map_err(|e|e.to_string())?;let _=stream.shutdown().await;Ok(())
}
