//! Per-process guardian and persistent ownership ledger for native engines.
//! The protocol child is contained by a group-leader front process and an
//! independent watchdog; killing either the host or front stops descendants.
use std::{path::{Path,PathBuf},process::{Command,Stdio},time::{Duration,Instant}};
use serde::{Serialize,Deserialize};
use serde_json::json;

#[derive(Debug,Clone,Serialize,Deserialize,PartialEq,Eq)]
struct Identity {pid:u32,pgid:u32,uid:u32,birth:String,executable:String}
#[derive(Debug,Serialize,Deserialize)]
struct Record {version:u32,owner:Identity,leader:Identity,member:Identity,command_digest:String}

#[cfg(target_os="macos")]
fn identity(pid:u32)->std::io::Result<Option<Identity>> {
    #[repr(C)]struct BsdInfo {prefix:[u32;12],comm:[u8;16],name:[u8;32],suffix:[u32;6],sec:u64,usec:u64}
    unsafe extern "C"{fn proc_pidinfo(pid:i32,flavor:i32,arg:u64,buffer:*mut std::ffi::c_void,size:i32)->i32;fn proc_pidpath(pid:i32,buffer:*mut std::ffi::c_void,size:u32)->i32;}
    let mut info=std::mem::MaybeUninit::<BsdInfo>::zeroed();
    let n=unsafe{proc_pidinfo(pid as i32,3,0,info.as_mut_ptr().cast(),std::mem::size_of::<BsdInfo>() as i32)};
    if n==0 && std::io::Error::last_os_error().raw_os_error()==Some(libc::ESRCH){return Ok(None);}
    if n!=std::mem::size_of::<BsdInfo>() as i32{return Err(std::io::Error::other("libproc process identity unavailable"));}
    let info=unsafe{info.assume_init()};if info.prefix[1]==5{return Ok(None);}
    let mut executable=[0u8;4096];let n=unsafe{proc_pidpath(pid as i32,executable.as_mut_ptr().cast(),executable.len() as u32)};
    if n<=0{return Err(std::io::Error::other(format!("libproc executable identity unavailable for pid {pid}: {}. If the application bundle was replaced while running, restart Muteki before starting another agent.",std::io::Error::last_os_error())));}
    let end=executable.iter().position(|c|*c==0).unwrap_or(n as usize);
    Ok(Some(Identity{pid,pgid:info.suffix[1],uid:info.prefix[5],birth:format!("{}:{}",info.sec,info.usec),executable:String::from_utf8_lossy(&executable[..end]).into_owned()}))
}
#[cfg(target_os="linux")]
fn identity(pid:u32)->std::io::Result<Option<Identity>> {
    use std::os::unix::fs::MetadataExt;
    let root=PathBuf::from(format!("/proc/{pid}"));
    let stat=match std::fs::read_to_string(root.join("stat")){Ok(v)=>v,Err(e)if e.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(e)=>return Err(e)};
    let after=stat.rsplit_once(')').ok_or_else(||std::io::Error::other("Malformed process stat"))?.1;
    let fields=after.split_whitespace().collect::<Vec<_>>();if fields.first()==Some(&"Z"){return Ok(None);}
    let group=fields.get(2).and_then(|v|v.parse().ok()).ok_or_else(||std::io::Error::other("Missing process group"))?;
    let start=fields.get(19).ok_or_else(||std::io::Error::other("Missing process birth identity"))?;
    Ok(Some(Identity{pid,pgid:group,uid:std::fs::metadata(&root)?.uid(),birth:format!("{}:{start}",std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()),executable:std::fs::read_link(root.join("exe"))?.to_string_lossy().into_owned()}))
}
#[cfg(not(any(target_os="linux",target_os="macos")))]
fn identity(_:u32)->std::io::Result<Option<Identity>>{Err(std::io::Error::other("Native process supervision is unavailable on this OS"))}
fn alive(expected:&Identity)->std::io::Result<bool>{Ok(identity(expected.pid)?.as_ref()==Some(expected))}
/// A supervised CLI reports its own PID, while the owned child is the guardian.
#[cfg(unix)]
pub(crate) fn verified_member(leader:u32,pid:u32)->std::io::Result<bool>{
    let(Some(parent),Some(member))=(identity(leader)?,identity(pid)?)else{return Ok(false)};
    Ok(parent.pgid==leader&&member.pgid==leader&&parent.uid==member.uid&&parent.uid==unsafe{libc::geteuid()})
}
#[cfg(not(unix))]
pub(crate) fn verified_member(_:u32,_:u32)->std::io::Result<bool>{Ok(false)}

/// Verify socket ownership before sending a private runtime credential.
pub(crate) fn owns_tcp_listener(leader: u32, port: u16) -> std::io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        // Runtime guardian runs fixed system utilities for process-group containment.
        #[allow(clippy::disallowed_methods)]
        let output = Command::new("/usr/sbin/lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
            .stdin(Stdio::null()).output()?;
        if !output.status.success() || output.stdout.len() > 65536 { return Ok(false); }
        let owners = String::from_utf8(output.stdout)
            .map_err(std::io::Error::other)?;
        let mut found = false;
        for line in owners.lines() {
            let pid = line.parse::<u32>().map_err(std::io::Error::other)?;
            if !verified_member(leader, pid)? { return Ok(false); }
            found = true;
        }
        return Ok(found);
    }
    #[cfg(target_os = "linux")]
    {
        let mut sockets = std::collections::HashSet::new();
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let content = match std::fs::read_to_string(table) {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for row in content.lines().skip(1) {
                let fields: Vec<_> = row.split_whitespace().collect();
                if fields.len() < 10 || fields[3] != "0A" { continue; }
                let Some((_, encoded)) = fields[1].rsplit_once(':') else { continue; };
                if u16::from_str_radix(encoded, 16).ok() == Some(port) {
                    sockets.insert(format!("socket:[{}]", fields[9]));
                }
            }
        }
        if sockets.is_empty() { return Ok(false); }
        for (member, _) in group_members(leader)? {
            if !verified_member(leader, member.pid)? { continue; }
            for entry in std::fs::read_dir(format!("/proc/{}/fd", member.pid))? {
                match std::fs::read_link(entry?.path()) {
                    Ok(target) => { sockets.remove(target.to_string_lossy().as_ref()); },
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                    Err(error) => return Err(error),
                }
            }
        }
        return Ok(sockets.is_empty());
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { let _ = (leader, port); Ok(false) }
}
fn private_dir(path:&Path)->std::io::Result<()>{std::fs::create_dir_all(path)?;if std::fs::symlink_metadata(path)?.file_type().is_symlink(){return Err(std::io::Error::other("Process ledger directory is a link"));}#[cfg(unix)]{use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700))?;}Ok(())}
fn write(path:&Path,record:&Record)->std::io::Result<()>{
    use std::io::Write;let mut temp=tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    #[cfg(unix)]{use std::os::unix::fs::PermissionsExt;temp.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;}
    serde_json::to_writer(&mut temp,record)?;temp.flush()?;temp.as_file().sync_all()?;temp.persist(path).map_err(|e|e.error)?;std::fs::File::open(path.parent().unwrap())?.sync_all()
}
#[cfg(unix)]
fn terminate(record:&Record)->std::io::Result<bool>{
    if record.member.pid<=1 || record.leader.pgid<=1 || record.member.uid!=unsafe{libc::geteuid()} || record.member.pgid!=record.leader.pgid || record.leader.pgid==unsafe{libc::getpgrp()} as u32 || !alive(&record.member)? {return Ok(false);}
    unsafe{libc::kill(-(record.leader.pgid as i32),libc::SIGCONT);libc::kill(-(record.leader.pgid as i32),libc::SIGTERM);}
    std::thread::sleep(Duration::from_secs(1));
    if alive(&record.member)?{unsafe{libc::kill(-(record.leader.pgid as i32),libc::SIGKILL);}}
    Ok(true)
}
#[cfg(not(unix))]
fn terminate(_: &Record)->std::io::Result<bool>{Ok(false)}

pub fn configure(root:&Path)->anyhow::Result<()> {
    let runtime=root.canonicalize()?.join("runtime");private_dir(&runtime)?;let dir=runtime.join("processes");private_dir(&dir)?;
    let owner=identity(std::process::id())?.ok_or_else(||anyhow::anyhow!("Server process identity unavailable"))?;
    let exe=std::env::current_exe()?;
    agent::configure_process_supervisor(exe,vec!["--runtime-guardian".into(),dir.into_os_string(),serde_json::to_string(&owner)?.into(),"--".into()]);
    Ok(())
}
/// Called before CLI parsing. Internal guardian modes never initialize product state.
pub fn entry()->anyhow::Result<bool>{
    let args=std::env::args_os().collect::<Vec<_>>();
    match args.get(1).and_then(|a|a.to_str()) {
        Some("--runtime-guardian")=>{
            let dir=PathBuf::from(args.get(2).ok_or_else(||anyhow::anyhow!("Missing ledger directory"))?);
            let owner:Identity=serde_json::from_str(&args.get(3).ok_or_else(||anyhow::anyhow!("Missing owner"))?.to_string_lossy())?;
            if !alive(&owner)?{return Ok(true);}
            let leader=identity(std::process::id())?.ok_or_else(||anyhow::anyhow!("Guardian identity unavailable"))?;
            if leader.pgid!=leader.pid || leader.pgid==owner.pgid{anyhow::bail!("Guardian must lead its own process group");}
            let path=dir.join(format!("{}.json",uuid::Uuid::new_v4()));
            let command=args.get(5..).filter(|v|!v.is_empty()).ok_or_else(||anyhow::anyhow!("Missing guarded command"))?;
            use sha2::{Digest,Sha256};let digest=format!("{:x}",Sha256::digest(command.iter().map(|v|v.to_string_lossy()).collect::<Vec<_>>().join("\0").as_bytes()));
            // Pre-init guardian supervisor spawns its own watchdog and already-resolved target.
            #[allow(clippy::disallowed_methods)]
            let mut watchdog=Command::new(std::env::current_exe()?).arg("--runtime-watchdog").arg(&path).arg(serde_json::to_string(&owner)?).arg(serde_json::to_string(&leader)?).arg(digest).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
            let deadline=Instant::now()+Duration::from_secs(5);
            while !path.is_file(){if watchdog.try_wait()?.is_some() || Instant::now()>deadline{anyhow::bail!("Process watchdog did not register its identity");}std::thread::sleep(Duration::from_millis(10));}
            if !alive(&owner)?{return Ok(true);}
            #[allow(clippy::disallowed_methods)]
            let status=Command::new(&command[0]).args(&command[1..]).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Some("--runtime-watchdog")=>{
            let path=PathBuf::from(&args[2]);let owner:Identity=serde_json::from_str(&args[3].to_string_lossy())?;let leader:Identity=serde_json::from_str(&args[4].to_string_lossy())?;
            let member=identity(std::process::id())?.ok_or_else(||anyhow::anyhow!("Watchdog identity unavailable"))?;
            if member.pgid!=leader.pgid || member.uid!=owner.uid{anyhow::bail!("Watchdog containment identity mismatch");}
            #[cfg(unix)]unsafe{libc::signal(libc::SIGTERM,libc::SIG_IGN);}
            let record=Record{version:1,owner,leader,member,command_digest:args[5].to_string_lossy().into_owned()};write(&path,&record)?;
            loop {
                if !alive(&record.owner)? || !alive(&record.leader)? {
                    // This live watchdog anchors the group even after its leader exits.
                    #[cfg(unix)]unsafe{libc::kill(-(record.leader.pgid as i32),libc::SIGCONT);libc::kill(-(record.leader.pgid as i32),libc::SIGTERM);}
                    std::thread::sleep(Duration::from_secs(1));let _=std::fs::remove_file(&path);
                    #[cfg(unix)]unsafe{libc::kill(-(record.leader.pgid as i32),libc::SIGKILL);}
                    return Ok(true);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        _=>Ok(false)
    }
}
pub fn reap(root:&Path)->anyhow::Result<serde_json::Value>{
    let dir=root.join("runtime/processes");if !dir.exists(){return Ok(json!({"stopped":0,"dropped":0,"active":0}));}
    let mut stopped=0;let mut dropped=0;let mut active=0;
    for entry in std::fs::read_dir(&dir)? {
        let entry=entry?;if entry.path().extension().and_then(|v|v.to_str())!=Some("json"){continue;}
        let kind=match entry.file_type(){Ok(kind)=>kind,Err(error)if error.kind()==std::io::ErrorKind::NotFound=>continue,Err(error)=>return Err(error.into())};
        if kind.is_symlink() || !kind.is_file(){anyhow::bail!("Unexpected process ledger entry");}
        let Some(record)=read_record(&entry.path())? else{continue};
        if record.version!=1{anyhow::bail!("Unknown process ledger version");}
        if alive(&record.owner)?{active+=1;continue;}
        if terminate(&record)?{stopped+=1;}else{dropped+=1;}
        match std::fs::remove_file(entry.path()){Ok(())=>{},Err(e)if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}
    }
    Ok(json!({"stopped":stopped,"dropped":dropped,"active":active}))
}

fn read_record(path:&Path)->std::io::Result<Option<Record>>{
    use std::io::Read;let mut options=std::fs::OpenOptions::new();options.read(true);
    #[cfg(unix)]{use std::os::unix::fs::OpenOptionsExt;options.custom_flags(libc::O_NOFOLLOW);}
    let file=match options.open(path){Ok(file)=>file,Err(error)if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(error)=>return Err(error)};let meta=file.metadata()?;if !meta.is_file()||meta.len()>65536{return Err(std::io::Error::other("Invalid process ledger file"));}
    #[cfg(unix)]{use std::os::unix::fs::MetadataExt;if meta.uid()!=unsafe{libc::geteuid()}{return Err(std::io::Error::other("Process ledger owner mismatch"));}}
    let mut bytes=Vec::new();file.take(65537).read_to_end(&mut bytes)?;serde_json::from_slice(&bytes).map(Some).map_err(std::io::Error::other)
}
#[cfg(unix)]
fn group_members(group:u32)->std::io::Result<Vec<(Identity,bool)>>{
    #[allow(clippy::disallowed_methods)]
    let output=Command::new("/bin/ps").args(["-axo","pid=,pgid=,state="]).stdin(Stdio::null()).output()?;
    if !output.status.success()||output.stdout.len()>8*1024*1024{return Err(std::io::Error::other("Process inventory failed"));}
    let mut members=Vec::new();
    for row in String::from_utf8_lossy(&output.stdout).lines(){let columns=row.split_whitespace().collect::<Vec<_>>();if columns.len()!=3{continue;}
        let(Some(pid),Some(pgid))=(columns[0].parse::<u32>().ok(),columns[1].parse::<u32>().ok())else{continue};
        if pgid==group && let Some(identity)=identity(pid)? && identity.pgid==group{members.push((identity,columns[2].starts_with('T')));}
    }Ok(members)
}
/// Stop only this service's verified group members, keeping its watchdog live.
#[cfg(unix)]
pub fn set_frozen(root:&Path,leader:u32,frozen:bool)->std::io::Result<bool>{
    let own=identity(std::process::id())?.ok_or_else(||std::io::Error::other("Server identity unavailable"))?;
    let mut selected=None;
    for entry in std::fs::read_dir(root.join("runtime/processes"))?{let entry=entry?;if entry.path().extension().and_then(|v|v.to_str())!=Some("json"){continue;}let Some(record)=read_record(&entry.path())? else{continue};if record.leader.pid==leader && record.owner==own{selected=Some(record);break;}}
    let Some(record)=selected else{return Err(std::io::Error::other("No owned process ledger for this runtime"));};
    if leader<=1 || record.leader.pgid==own.pgid || !alive(&record.member)? || !alive(&record.leader)?{return Err(std::io::Error::other("Runtime process ownership changed"));}
    let deadline=Instant::now()+Duration::from_secs(3);
    loop {
        if !alive(&record.member)? || !alive(&record.leader)?{return Err(std::io::Error::other("Runtime exited during process control"));}
        let members=group_members(record.leader.pgid)?;let mut settled=true;let mut affected=false;
        for(member,stopped)in members{
            if member.pid==record.member.pid{continue;}
            if member.uid!=own.uid{return Err(std::io::Error::other("Runtime process group has a foreign owner"));}
            affected=true;if stopped==frozen{continue;}settled=false;
            if !alive(&member)?{continue;}
            if unsafe{libc::kill(member.pid as i32,if frozen{libc::SIGSTOP}else{libc::SIGCONT})}!=0{return Err(std::io::Error::last_os_error());}
        }
        if settled{return Ok(affected);}
        if Instant::now()>=deadline{return Err(std::io::Error::other("Runtime did not confirm its stopped/running state"));}
        std::thread::sleep(Duration::from_millis(25));
    }
}
#[cfg(not(unix))]
pub fn set_frozen(_: &Path,_:u32,_:bool)->std::io::Result<bool>{Err(std::io::Error::other("Process freezing is unavailable on this platform"))}
