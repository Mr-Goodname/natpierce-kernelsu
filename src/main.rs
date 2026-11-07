mod config;
mod core;
mod daemon;
mod dns;
mod nat_retry;
mod protocol;
mod routes;
mod websocket;
use base64::{Engine,engine::general_purpose::STANDARD};
use serde_json::{json,Value};
use std::{fs,io::{self,Read,Write},os::unix::net::UnixStream,path::{Path,PathBuf},thread,time::{Duration,Instant}};
fn root()->PathBuf { std::env::var_os("NP_DATA").map(PathBuf::from).unwrap_or_else(||"/data/adb/natpierce-rs".into()) }
fn module()->PathBuf {std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().to_owned()}
fn print(v:Value){println!("{}",v)}
fn request(v:Value)->io::Result<Value> {
    let mut s=UnixStream::connect(root().join("run/control.sock"))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;s.set_write_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(serde_json::to_string(&v)?.as_bytes())?;s.write_all(b"\n")?;
    let mut buf=Vec::new();
    loop {let mut b=[0;4096];let n=s.read(&mut b)?;if n==0{break}buf.extend_from_slice(&b[..n]);if buf.contains(&b'\n'){break}if buf.len()>1048576{return Err(io::Error::other("response too large"))}}
    Ok(serde_json::from_slice(&buf)?)
}
fn start()->io::Result<()> {
    let data=root();std::fs::create_dir_all(&data)?;
    match std::fs::remove_file(data.join("stop")){Ok(())=>{},Err(e)if e.kind()==io::ErrorKind::NotFound=>{},Err(e)=>return Err(e)}
    std::process::Command::new("sh").arg(module().join("bin/watchdog.sh")).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
    Ok(())
}
fn matches_executable(actual:&Path,expected:&Path)->bool {
    let actual=actual.to_string_lossy();let actual=actual.strip_suffix(" (deleted)").unwrap_or(&actual);
    Path::new(actual)==expected
}
fn owned_pid(data:&Path,kind:&str)->Option<i32> {
    let pid=fs::read_to_string(data.join(format!("run/{kind}.pid"))).ok()?.trim().parse::<i32>().ok()?;
    if pid<=1||pid==std::process::id()as i32{return None}
    let expected=if kind=="core"{data.join("core/natpierce")}else{std::env::current_exe().ok()?};
    let expected=expected.canonicalize().unwrap_or(expected);
    let actual=fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let legacy=module().join("bin/natpierce");let legacy=legacy.canonicalize().unwrap_or(legacy);
    if !matches_executable(&actual,&expected)&&!(kind=="core"&&matches_executable(&actual,&legacy)){return None}
    if kind=="core"{
        let cwd=fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
        if cwd!=data.join("core").canonicalize().unwrap_or_else(|_|data.join("core")){return None}
    }else{
        let cmdline=fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        if cmdline.split(|&b|b==0).nth(1)!=Some(b"daemon".as_slice()){return None}
    }
    Some(pid)
}
fn terminate_owned(data:&Path,kind:&str)->io::Result<()> {
    if let Some(pid)=owned_pid(data,kind){
        if unsafe{libc::kill(pid,libc::SIGTERM)}!=0{let error=io::Error::last_os_error();if error.raw_os_error()!=Some(libc::ESRCH){return Err(error)}}
        let deadline=Instant::now()+Duration::from_millis(700);
        while owned_pid(data,kind)==Some(pid)&&Instant::now()<deadline{thread::sleep(Duration::from_millis(25));}
        if owned_pid(data,kind)==Some(pid)&&unsafe{libc::kill(pid,libc::SIGKILL)}!=0{let error=io::Error::last_os_error();if error.raw_os_error()!=Some(libc::ESRCH){return Err(error)}}
        let deadline=Instant::now()+Duration::from_millis(300);
        while owned_pid(data,kind)==Some(pid)&&Instant::now()<deadline{thread::sleep(Duration::from_millis(25));}
        if owned_pid(data,kind)==Some(pid){return Err(io::Error::other("owned process has not exited"))}
    }
    Ok(())
}
fn cleanup_stopped(data:&Path)->io::Result<Value> {
    if owned_pid(data,"daemon").is_some()||owned_pid(data,"core").is_some(){return Err(io::Error::other("服务尚未退出，暂不清理运行中的网络规则"))}
    let clean=routes::cleanup();routes::protect_control(false);
    if !clean{return Err(io::Error::other("服务已停止，但部分原生 NAT 规则未能清理"))}
    Ok(json!({"ok":true,"core":core::inspect(data),"state":{"daemon":false,"core_running":false,"logged_in":false,"phase":"stopped"},"settings":config::read::<config::Settings>(&data.join("settings.json")).public()}))
}
fn stop()->io::Result<Value> {
    let data=root();fs::create_dir_all(&data)?;
    // The supervisor observes this even when its daemon/control socket is gone.
    let mut marker=fs::File::create(data.join("stop"))?;marker.write_all(b"stopped\n")?;marker.sync_all()?;
    let _=request(json!({"action":"stop"}));
    terminate_owned(&data,"daemon")?;terminate_owned(&data,"core")?;
    cleanup_stopped(&data)
}
fn main(){
    let args:Vec<_>=std::env::args().skip(1).collect();
    if args.first().map(String::as_str)==Some("daemon") {if let Err(e)=daemon::run(root(),module()){eprintln!("daemon: {e}");std::process::exit(1)}return}
    if args.first().map(String::as_str)==Some("start"){print(match start(){Ok(_)=>json!({"ok":true,"message":"正在启动"}),Err(e)=>json!({"ok":false,"error":e.to_string()})});return}
    let payload=if let Some(pos)=args.iter().position(|a|a=="--b64") {
        args.get(pos+1).and_then(|s|STANDARD.decode(s).ok()).and_then(|b|serde_json::from_slice::<Value>(&b).ok())
    } else {Some(json!({"action":args.first().map(String::as_str).unwrap_or("status")}))};
    let Some(v)=payload else{print(json!({"ok":false,"error":"请求格式无效"}));return};
    if v["action"]=="start"{print(match start(){Ok(_)=>json!({"ok":true,"message":"正在启动"}),Err(e)=>json!({"ok":false,"error":e.to_string()})});return}
    if v["action"]=="stop"{print(match stop(){Ok(out)=>out,Err(e)=>json!({"ok":false,"error":e.to_string()})});return}
    if v["action"]=="cleanup"{print(match cleanup_stopped(&root()){Ok(out)=>out,Err(e)=>json!({"ok":false,"error":e.to_string()})});return}
    match request(v.clone()) {
        Ok(out)=>print(out),
        Err(e)=>{
            let conf:config::Settings=config::read(&root().join("settings.json"));
            let error=if matches!(v["action"].as_str(),Some("core-download"|"core-check-update"|"core-update")){"请先启动管理服务，再管理官方核心".to_owned()}else{format!("管理服务未响应：{e}")};
            print(json!({"ok":v["action"]=="status"||v["action"]=="core-status","error":error,"core":core::inspect(&root()),"settings":conf.public(),"state":{"daemon":false,"core_running":false,"logged_in":false,"phase":"stopped"}}));
        }
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    #[test]
    fn executable_identity_accepts_only_the_same_module_path(){
        let expected=Path::new("/data/adb/modules/natpierce_rs/bin/npctl");
        assert!(matches_executable(expected,expected));
        assert!(matches_executable(Path::new("/data/adb/modules/natpierce_rs/bin/npctl (deleted)"),expected));
        assert!(!matches_executable(Path::new("/data/local/tmp/npctl"),expected));
        assert!(!matches_executable(Path::new("/data/adb/modules/another/bin/npctl"),expected));
    }
}
