//! A single HTTP/1 exchange retains the bytes sent and received, including framing.
use tokio::io::{AsyncRead,AsyncWrite,AsyncWriteExt,AsyncReadExt,AsyncBufReadExt,BufReader};
use serde_json::{Value,json};
use std::{sync::Arc,time::Duration};

trait Socket:AsyncRead+AsyncWrite+Unpin+Send{}
impl<T:AsyncRead+AsyncWrite+Unpin+Send> Socket for T{}
pub(in crate::runs::src_api::domains) struct Capture {pub raw:Vec<u8>,pub complete:bool,pub error:Option<&'static str>,pub sent:usize,pub peer:Value}
struct Reader {stream:BufReader<Box<dyn Socket>>,raw:Vec<u8>,body:usize,limit:usize}
impl Reader {
    async fn line(&mut self)->Result<Vec<u8>,&'static str>{
        let mut line=Vec::new();
        loop{let available=self.stream.fill_buf().await.map_err(|_|"response_read_failed")?;if available.is_empty(){return Err("response_truncated");}
            let length=available.iter().position(|byte|*byte==b'\n').map(|at|at+1).unwrap_or(available.len());
            if line.len()+length>65536||self.raw.len()+length>self.limit+2*1024*1024{return Err("response_headers_limit");}
            line.extend_from_slice(&available[..length]);self.raw.extend_from_slice(&available[..length]);self.stream.consume(length);
            if line.last()==Some(&b'\n'){return Ok(line);}
        }
    }
    async fn exact(&mut self,mut size:usize,body:bool)->Result<(),&'static str>{
        let mut buffer=[0u8;8192];
        while size>0{let maximum=if body{self.limit.saturating_sub(self.body)}else{8192}.min(size).min(buffer.len());if maximum==0{return Err("response_body_capture_limit");}
            let count=self.stream.read(&mut buffer[..maximum]).await.map_err(|_|"response_read_failed")?;if count==0{return Err("response_truncated");}
            self.raw.extend_from_slice(&buffer[..count]);if body{self.body+=count;}size-=count;
        }Ok(())
    }
    async fn response(&mut self,method:&str)->Result<(),&'static str>{
        let mut header_total=0;let mut interim=0;
        let(status,headers)=loop{
            let start=self.line().await?;header_total+=start.len();
            let text=String::from_utf8_lossy(&start);let parts=text.trim_end_matches(['\r','\n']).splitn(3,' ').collect::<Vec<_>>();
            if parts.len()<2||!matches!(parts[0],"HTTP/1.0"|"HTTP/1.1")||parts[1].len()!=3||!parts[1].bytes().all(|byte|byte.is_ascii_digit()){return Err("response_status_invalid");}
            let status=parts[1].parse::<u16>().map_err(|_|"response_status_invalid")?;
            let mut headers=Vec::new();loop{let line=self.line().await?;header_total+=line.len();if header_total>1024*1024{return Err("response_headers_limit");}if matches!(line.as_slice(),b"\r\n"|b"\n"){break;}
                if matches!(line.first(),Some(b' '|b'\t')){return Err("response_headers_invalid");}
                let line=line.iter().map(|byte|char::from(*byte)).collect::<String>();let line=line.trim_end_matches(['\r','\n']);let(name,value)=line.split_once(':').ok_or("response_headers_invalid")?;
                if name.is_empty()||name.bytes().any(|byte|byte<=32||byte>=127){return Err("response_headers_invalid");}headers.push((name.to_ascii_lowercase(),value.trim().to_owned()));
            }
            if (100..200).contains(&status)&&status!=101{interim+=1;if interim>32{return Err("response_interim_limit");}continue;}break(status,headers)
        };
        if status==101{return Err("http_upgrade_unsupported");}
        if method.eq_ignore_ascii_case("HEAD")||matches!(status,204|304)||(100..200).contains(&status){return Ok(());}
        let lengths=headers.iter().filter(|(name,_)|name=="content-length").map(|(_,value)|value).collect::<Vec<_>>();
        let transfers=headers.iter().filter(|(name,_)|name=="transfer-encoding").flat_map(|(_,value)|value.split(',').map(|value|value.trim().to_ascii_lowercase())).collect::<Vec<_>>();
        if !transfers.is_empty(){
            if !lengths.is_empty()||transfers.last().map(String::as_str)!=Some("chunked"){return Err("response_framing_unsupported");}
            loop {let line=self.line().await?;let size=std::str::from_utf8(&line).map_err(|_|"response_chunk_invalid")?.trim().split(';').next().unwrap_or("");if size.is_empty()||!size.bytes().all(|byte|byte.is_ascii_hexdigit()){return Err("response_chunk_invalid");}let size=usize::from_str_radix(size,16).map_err(|_|"response_chunk_invalid")?;
                if size==0{let mut trailer_size=0;loop{let trailer=self.line().await?;trailer_size+=trailer.len();if trailer_size>1024*1024{return Err("response_trailers_limit");}if matches!(trailer.as_slice(),b"\r\n"|b"\n"){return Ok(());}}}
                self.exact(size,true).await?;let ending=self.line().await?;if !matches!(ending.as_slice(),b"\r\n"|b"\n"){return Err("response_chunk_invalid");}
            }
        }
        if let Some(length)=lengths.first(){if lengths.iter().any(|value|value!=length)||length.is_empty()||!length.bytes().all(|byte|byte.is_ascii_digit()){return Err("response_length_invalid");}return self.exact(length.parse().map_err(|_|"response_length_invalid")?,true).await;}
        let mut buffer=[0u8;8192];loop{let maximum=(self.limit.saturating_sub(self.body)+1).min(buffer.len());let count=self.stream.read(&mut buffer[..maximum]).await.map_err(|_|"response_read_failed")?;if count==0{return Ok(());}let keep=count.min(self.limit.saturating_sub(self.body));self.raw.extend_from_slice(&buffer[..keep]);self.body+=keep;if keep<count{return Err("response_body_capture_limit");}}
    }
}
async fn dial_tcp_or_proxy(
    host: &str,
    port: u16,
    proxy: Option<&super::super::proxypool::ResolvedUpstream>,
) -> Result<(Box<dyn Socket>, std::net::SocketAddr), &'static str> {
    let Some(upstream) = proxy else {
        let stream = tokio::net::TcpStream::connect((host, port)).await.map_err(|_| "connection_failed")?;
        let peer = stream.peer_addr().map_err(|_| "connection_failed")?;
        return Ok((Box::new(stream), peer));
    };
    let tcp = tokio::net::TcpStream::connect((upstream.host.as_str(), upstream.port))
        .await
        .map_err(|_| "upstream_proxy_connect_failed")?;
    let peer = tcp.peer_addr().map_err(|_| "connection_failed")?;
    let mut tcp: Box<dyn Socket> = if upstream.scheme == "https" {
        use tokio_rustls::rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder_with_provider(Arc::new(tokio_rustls::rustls::crypto::ring::default_provider())).with_safe_default_protocol_versions().map_err(|_| "tls_configuration_failed")?.with_root_certificates(roots).with_no_client_auth();
        let name = ServerName::try_from(upstream.host.clone()).map_err(|_| "tls_server_name_invalid")?;
        Box::new(tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(|_| "upstream_proxy_tls_failed")?)
    } else { Box::new(tcp) };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    match upstream.scheme.as_str() {
        "socks5" | "socks5h" => {
            let has_auth = !upstream.username.is_empty() || !upstream.password.is_empty();
            let greeting = if has_auth {
                [0x05u8, 0x02, 0x00, 0x02]
            } else {
                [0x05u8, 0x01, 0x00, 0x00]
            };
            let greeting_slice = if has_auth { &greeting[..4] } else { &greeting[..3] };
            tcp.write_all(greeting_slice)
                .await
                .map_err(|_| "upstream_socks_write_failed")?;
            let mut choice = [0u8; 2];
            tcp.read_exact(&mut choice)
                .await
                .map_err(|_| "upstream_socks_read_failed")?;
            if choice[0] != 0x05 {
                return Err("upstream_socks_protocol_error");
            }
            if choice[1] == 0x02 {
                let u = upstream.username.as_bytes();
                let p = upstream.password.as_bytes();
                if u.len() > 255 || p.len() > 255 {
                    return Err("upstream_socks_auth_too_long");
                }
                let mut auth_req = Vec::with_capacity(3 + u.len() + p.len());
                auth_req.push(0x01);
                auth_req.push(u.len() as u8);
                auth_req.extend_from_slice(u);
                auth_req.push(p.len() as u8);
                auth_req.extend_from_slice(p);
                tcp.write_all(&auth_req)
                    .await
                    .map_err(|_| "upstream_socks_auth_write_failed")?;
                let mut auth_resp = [0u8; 2];
                tcp.read_exact(&mut auth_resp)
                    .await
                    .map_err(|_| "upstream_socks_auth_read_failed")?;
                if auth_resp[1] != 0x00 {
                    return Err("upstream_socks_auth_rejected");
                }
            } else if choice[1] != 0x00 {
                return Err("upstream_socks_method_rejected");
            }
            let mut req = Vec::with_capacity(64);
            req.extend_from_slice(&[0x05, 0x01, 0x00]);
            if let Ok(ip) = host.parse::<std::net::IpAddr>() {
                match ip {
                    std::net::IpAddr::V4(v4) => {
                        req.push(0x01);
                        req.extend_from_slice(&v4.octets());
                    }
                    std::net::IpAddr::V6(v6) => {
                        req.push(0x04);
                        req.extend_from_slice(&v6.octets());
                    }
                }
            } else {
                let hb = host.as_bytes();
                if hb.len() > 255 {
                    return Err("upstream_socks_host_too_long");
                }
                req.push(0x03);
                req.push(hb.len() as u8);
                req.extend_from_slice(hb);
            }
            req.extend_from_slice(&port.to_be_bytes());
            tcp.write_all(&req)
                .await
                .map_err(|_| "upstream_socks_connect_write_failed")?;
            let mut hdr = [0u8; 4];
            tcp.read_exact(&mut hdr)
                .await
                .map_err(|_| "upstream_socks_connect_read_failed")?;
            if hdr[0] != 0x05 || hdr[1] != 0x00 {
                return Err("upstream_socks_connect_rejected");
            }
            match hdr[3] {
                0x01 => {
                    let mut rest = [0u8; 6];
                    tcp.read_exact(&mut rest)
                        .await
                        .map_err(|_| "upstream_socks_connect_tail_failed")?;
                }
                0x03 => {
                    let mut len = [0u8; 1];
                    tcp.read_exact(&mut len)
                        .await
                        .map_err(|_| "upstream_socks_connect_tail_failed")?;
                    let mut rest = vec![0u8; (len[0] as usize) + 2];
                    tcp.read_exact(&mut rest)
                        .await
                        .map_err(|_| "upstream_socks_connect_tail_failed")?;
                }
                0x04 => {
                    let mut rest = [0u8; 18];
                    tcp.read_exact(&mut rest)
                        .await
                        .map_err(|_| "upstream_socks_connect_tail_failed")?;
                }
                _ => return Err("upstream_socks_bad_atyp"),
            }
            Ok((tcp, peer))
        }
        _ => {
            let authority = if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            let mut connect_req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
            if !upstream.username.is_empty() || !upstream.password.is_empty() {
                use base64::Engine;
                let token = base64::engine::general_purpose::STANDARD
                    .encode(format!("{}:{}", upstream.username, upstream.password));
                connect_req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
            }
            connect_req.push_str("\r\n");
            tcp.write_all(connect_req.as_bytes())
                .await
                .map_err(|_| "upstream_http_connect_write_failed")?;
            let mut head = Vec::with_capacity(512);
            let mut byte = [0u8; 1];
            loop {
                if tcp
                    .read_exact(&mut byte)
                    .await
                    .map_err(|_| "upstream_http_connect_read_failed")?
                    == 0
                {
                    return Err("upstream_http_connect_eof");
                }
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
                    break;
                }
                if head.len() > 16384 {
                    return Err("upstream_http_connect_headers_too_large");
                }
            }
            let status_line = head.split(|b| *b == b'\n').next().unwrap_or(&[]);
            let status_text = std::str::from_utf8(status_line).unwrap_or("");
            let code = status_text
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(0);
            if !(200..300).contains(&code) {
                return Err("upstream_http_connect_rejected");
            }
            Ok((tcp, peer))
        }
    }
}

pub(in crate::runs::src_api::domains) async fn exchange_via_proxy(url:&reqwest::Url,method:&str,request:&[u8],timeout:Duration,limit:usize,proxy:Option<&super::super::proxypool::ResolvedUpstream>)->Capture{
    let deadline=tokio::time::Instant::now()+timeout;
    let mut capture=Capture{raw:Vec::new(),complete:false,error:None,sent:0,peer:Value::Null};
    let connect=async{
        let host=url.host_str().ok_or("connection_failed")?;let port=url.port_or_known_default().ok_or("connection_failed")?;
        let (tcp, peer)=dial_tcp_or_proxy(host, port, proxy).await?;
        capture.peer=json!({"address":peer.ip().to_string(),"port":peer.port()});
        let socket:Box<dyn Socket>=if url.scheme()=="https"{
            use tokio_rustls::rustls::{ClientConfig,RootCertStore,pki_types::ServerName};
            let roots=RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let provider=Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
            let config=ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().map_err(|_|"tls_configuration_failed")?.with_root_certificates(roots).with_no_client_auth();
            let name=ServerName::try_from(host.to_owned()).map_err(|_|"tls_server_name_invalid")?;
            Box::new(tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name,tcp).await.map_err(|_|"tls_connection_failed")?)
        }else{Box::new(tcp)};Ok::<_,&'static str>(socket)
    };
    let socket=match tokio::time::timeout(timeout.min(Duration::from_secs(10)),connect).await{Ok(Ok(socket))=>socket,Ok(Err(error))=>{capture.error=Some(error);return capture;},Err(_)=>{capture.error=Some("connection_timeout");return capture;}};
    let mut reader=Reader{stream:BufReader::new(socket),raw:Vec::new(),body:0,limit};
    let outcome=tokio::time::timeout_at(deadline,async{
        while capture.sent<request.len(){let count=reader.stream.get_mut().write(&request[capture.sent..]).await.map_err(|_|"request_write_failed")?;if count==0{return Err("request_write_failed");}capture.sent+=count;}
        reader.stream.get_mut().flush().await.map_err(|_|"request_write_failed")?;reader.response(method).await
    }).await;
    capture.raw=reader.raw;match outcome{Ok(Ok(()))=>capture.complete=true,Ok(Err(error))=>capture.error=Some(error),Err(_)=>capture.error=Some("exchange_timeout")};capture
}
