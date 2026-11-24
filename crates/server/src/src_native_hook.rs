//! The packaged service binary also serves as Claude's fail-closed command hook.
pub async fn entry()->anyhow::Result<bool>{
    if std::env::args().nth(1).as_deref()!=Some("src-native-hook"){return Ok(false)}
    let result=async{
        use std::io::Read;
        let mut bytes=Vec::new();std::io::stdin().take(32*1024*1024+1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len()<=32*1024*1024,"SRC tool hook input exceeds capture limit");
        let input:serde_json::Value=serde_json::from_slice(&bytes)?;
        let endpoint=std::env::var("MUTEKI_SRC_HOOK_URL")?;let url=reqwest::Url::parse(&endpoint)?;
        anyhow::ensure!(url.scheme()=="http"&&url.host_str()==Some("127.0.0.1")&&url.path()=="/hook","SRC hook endpoint is not private loopback");
        let client=crate::network_proxy::direct_client_builder().redirect(reqwest::redirect::Policy::none()).timeout(std::time::Duration::from_secs(950)).build()?;
        let response=client.post(url).bearer_auth(std::env::var("MUTEKI_SRC_HOOK_TOKEN")?).json(&input).send().await?.error_for_status()?;
        let answer:serde_json::Value=response.json().await?;
        if answer.get("error").is_some(){anyhow::bail!("SRC execution authorization failed: {}",answer["error"]);}
        println!("{}",serde_json::to_string(&answer)?);Ok::<(),anyhow::Error>(())
    }.await;
    if let Err(error)=result{eprintln!("SRC execution policy hook blocked dispatch: {error}");std::process::exit(2);}
    Ok(true)
}
