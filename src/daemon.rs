use crate::{config::{self,Account,Settings},core,dns,nat_retry,protocol::{self,State,SEP},routes::{self,Tun},websocket::Ws};
use serde_json::{json,Value};
use std::{collections::VecDeque,fs,io::{self,BufRead,BufReader,Read,Write},os::{fd::AsRawFd,unix::{fs::PermissionsExt,net::UnixListener,process::CommandExt}},path::{Path,PathBuf},process::{Child,Command,Stdio},sync::{atomic::{AtomicBool,AtomicU32,AtomicU64,AtomicUsize,Ordering},mpsc,Arc},thread,time::{Duration,Instant}};
static EXIT:AtomicBool=AtomicBool::new(false);
extern "C" fn signal(_:i32){EXIT.store(true,Ordering::Relaxed)}
type Request=(Value,mpsc::Sender<Value>);
const PORT:u16=33472;
const CONNECT_TIMEOUT:Duration=Duration::from_secs(90);
fn connection_config_changed(old:&Settings,new:&Settings)->bool {
 old.role!=new.role||old.client.code!=new.client.code||old.client.peer_id!=new.client.peer_id||old.client.password!=new.client.password
 ||((old.role=="server"||new.role=="server")&&(old.server.conpwd!=new.server.conpwd||old.server.webpwd!=new.server.webpwd||old.server.lanip!=new.server.lanip))
}
fn clear_account_file(data:&Path)->io::Result<()> {
 let path=data.join("account.json");
 // If unlink fails, a successfully replaced empty file still retains no password.
 // If writing fails (for example a full disk), successful unlink is sufficient.
 let wipe=config::save(&path,&Account::default());
 match fs::remove_file(&path) {
  Ok(())=>{},Err(e)if e.kind()==io::ErrorKind::NotFound=>{},
  Err(e)=>return Err(wipe.err().unwrap_or(e)),
 }
 fs::File::open(data)?.sync_all()
}
fn prepare_core(data:&Path,module:&Path)->io::Result<PathBuf> {
 let executable=core::verified_path(data)?;
 prepare_identity(data,module)?;
 fs::set_permissions(&executable,fs::Permissions::from_mode(0o700))?;
 Ok(executable)
}
fn prepare_identity(data:&Path,module:&Path)->io::Result<()> {
 let root=data.join("core");let private=root.join("data");
 for path in [&root,&private]{fs::create_dir_all(path)?;fs::set_permissions(path,fs::Permissions::from_mode(0o700))?;}
 // The official Go core resolves its config relative to os.Executable(), not
 // its working directory. Run original bytes from the persistent private root.
 let path=private.join("config");
 let legacy=module.join("bin/data/config");
 let existing=if path.exists(){Some(fs::read(&path)?)}else if legacy.exists(){Some(fs::read(&legacy)?)}else{None};
 // A fresh core must create its own defaults, including encrypted MAC data.
 // Its verified default Auto_login is false. An incomplete synthetic config
 // would prevent initialization and make PCID restoration fail on next start.
 let Some(bytes)=existing else{return Ok(())};
 let mut conf:Value=serde_json::from_slice(&bytes).map_err(io::Error::other)?;
 let object=conf.as_object_mut().ok_or_else(||io::Error::other("核心身份配置格式有误，原文件已保留"))?;
 // Preserve the official encrypted MAC and account PCID records. Only the
 // Rust manager schedules login; the core must not also auto-login.
 object.insert("Auto_login".into(),json!(false));
 config::save(&path,&conf)?;
 Ok(())
}
fn reap_stale_core(data:&std::path::Path,module:&std::path::Path)->io::Result<()> {
 let pid_file=data.join("run/core.pid");
 let pid=fs::read_to_string(&pid_file).ok().and_then(|p|p.trim().parse::<i32>().ok()).filter(|&p|p>1&&p!=std::process::id() as i32);
 if let Some(pid)=pid {
  let executable=data.join("core/natpierce");let executable=executable.canonicalize().unwrap_or(executable);
  let legacy=module.join("bin/natpierce");let legacy=legacy.canonicalize().unwrap_or(legacy);
  let cwd=data.join("core");let cwd=cwd.canonicalize().unwrap_or(cwd);
  let owns=||fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|p|crate::matches_executable(&p,&executable)||crate::matches_executable(&p,&legacy))
   &&fs::read_link(format!("/proc/{pid}/cwd")).is_ok_and(|p|p==cwd);
  if owns() {
   if unsafe{libc::kill(pid,libc::SIGTERM)}!=0{let error=io::Error::last_os_error();if error.raw_os_error()!=Some(libc::ESRCH){return Err(error)}}
   for _ in 0..20 {if !owns(){break}thread::sleep(Duration::from_millis(50));}
   if owns()&&unsafe{libc::kill(pid,libc::SIGKILL)}!=0{let error=io::Error::last_os_error();if error.raw_os_error()!=Some(libc::ESRCH){return Err(error)}}
  }
 }
 let _=fs::remove_file(pid_file);
 Ok(())
}
struct Manager {
 data:PathBuf,module:PathBuf,conf:Settings,account:Account,state:State,tun:Tun,core_installer:core::Installer,core_revision:u64,
 core:Option<Child>,core_pid:Arc<AtomicU32>,nat_retries:Arc<AtomicU64>,nat_retry_running:bool,ws:Option<Ws>,queue:VecDeque<String>,last_send:Instant,
 login_pending:Option<Instant>,connect_pending:Option<Instant>,login_retry:Instant,connect_retry:Instant,
 login_attempts:u32,connection_attempts:u32,generation:u64,started:Instant,core_retry:Instant,last_poll:Instant,
 last_route:Instant,last_probe:Instant,probe_rx:mpsc::Receiver<(u64,bool)>,probe_tx:mpsc::Sender<(u64,bool)>,probe_busy:bool,
 probe_epoch:u64,data_ok:bool,probe_verified:bool,probe_failures:u32,last_event:String,renamed:bool,server_pending:Option<Instant>,vpn_configured:bool,
}
impl Manager {
 fn new(data:PathBuf,module:PathBuf,conf:Settings,account:Account)->Self {
  let now=Instant::now();let(probe_tx,probe_rx)=mpsc::channel();
  let core_installer=core::Installer::new(data.clone());
  Self{conf,account,data,module,state:State::default(),tun:Tun::default(),core_installer,core_revision:0,core:None,core_pid:Arc::new(AtomicU32::new(0)),nat_retries:Arc::new(AtomicU64::new(0)),nat_retry_running:false,ws:None,queue:VecDeque::new(),last_send:now,login_pending:None,connect_pending:None,login_retry:now,connect_retry:now,login_attempts:0,connection_attempts:0,generation:0,started:now,core_retry:now,last_poll:now,last_route:now,last_probe:now,probe_rx,probe_tx,probe_busy:false,probe_epoch:0,data_ok:false,probe_verified:false,probe_failures:0,last_event:String::new(),renamed:false,server_pending:None,vpn_configured:false}
 }
 fn event(&mut self,text:&str){ self.last_event=text.into(); if let Ok(mut f)=fs::OpenOptions::new().create(true).append(true).open(self.data.join("logs/manager.log")){let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();let _=writeln!(f,"{now} {text}");} }
 fn snapshot(&self)->Value {
  let mut s=serde_json::to_value(&self.state).unwrap();
  s["daemon"]=json!(true);s["core_running"]=json!(self.core.is_some());s["control_connected"]=json!(self.ws.is_some());
  s["tun"]=json!(self.tun);s["uptime"]=json!(self.started.elapsed().as_secs());s["last_event"]=json!(self.last_event);
  s["data_ok"]=json!(self.data_ok);s["probe_failures"]=json!(self.probe_failures);s["generation"]=json!(self.generation);
  s["login_attempts"]=json!(self.login_attempts);s["connection_attempts"]=json!(self.connection_attempts);
  s["nat_retry_running"]=json!(self.nat_retry_running);s["nat_retries"]=json!(self.nat_retries.load(Ordering::Relaxed));
  if self.login_pending.is_some() && !self.state.logged_in{s["phase"]=json!("logging_in")}
  if self.connect_pending.is_some() && !self.state.client_connected{s["phase"]=json!("connecting")}
  let info=self.core_installer.status();
  if self.core.is_none()&&!info.verified{s["phase"]=json!(if info.phase=="error"{"core_error"}else{"core_missing"});s["error"]=json!(info.error);}
  json!({"ok":true,"core":info,"state":s,"settings":self.conf.public()})
 }
 fn persist(&self)->io::Result<()> {config::save(&self.data.join("settings.json"),&self.conf)}
 fn commit_conf(&mut self,conf:Settings)->Result<(),String> {
  conf.validate()?;
  config::save(&self.data.join("settings.json"),&conf).map_err(|e|format!("设置保存失败：{e}"))?;
  self.conf=conf;Ok(())
 }
 fn invalidate_probe(&mut self) {
  self.probe_epoch=self.probe_epoch.wrapping_add(1);self.data_ok=false;self.probe_verified=false;self.probe_failures=0;self.probe_busy=false;
  self.last_probe=Instant::now().checked_sub(Duration::from_secs(16)).unwrap_or_else(Instant::now);
 }
 /// Only a result for the current connection may affect its health or trigger recovery.
 fn record_probe(&mut self,epoch:u64,ok:bool)->bool {
  if epoch!=self.probe_epoch{return false}
  self.probe_busy=false;
  if !self.state.client_connected||!self.tun.routed{self.data_ok=false;return false}
  self.data_ok=ok;
  if ok{self.probe_verified=true;self.probe_failures=0}else{self.probe_failures=self.probe_failures.saturating_add(1)}
  // The default ICMP target can legitimately ignore ping. It may tear down a
  // link only after responding on this connection, or with an explicit TCP probe.
  self.probe_failures>=3&&self.conf.role=="client"&&self.conf.auto_connect&&(self.probe_verified||self.conf.probe_port!=0)
 }
 fn restart_core(&mut self) {
  self.stop_core();let now=Instant::now();self.core_retry=now;self.login_retry=now;self.connect_retry=now;
 }
 fn stop_core(&mut self) {
  self.core_pid.store(0,Ordering::Release);
  self.clear_control();
  if let Some(mut c)=self.core.take(){let _=c.kill();let _=c.wait();}
  let _=fs::remove_file(self.data.join("run/core.pid"));
  self.state=State::default();self.tun=Tun::default();self.login_pending=None;self.connect_pending=None;self.renamed=false;self.server_pending=None;self.invalidate_probe();routes::cleanup();
 }
 fn clear_control(&mut self) {
  self.ws=None;self.queue.clear();self.vpn_configured=false;
 }
 fn spawn_core(&mut self)->io::Result<()> {
  let executable=prepare_core(&self.data,&self.module)?;
  let log=fs::OpenOptions::new().create(true).append(true).open(self.data.join("logs/core.log"))?;
  let parent_pid=unsafe{libc::getpid()};
  let mut command=Command::new(executable);
  command.args(["-p",&PORT.to_string()]).current_dir(self.data.join("core")).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
  // The kernel terminates this core if its manager is killed, including SIGKILL.
  // Checking the parent after prctl closes the death-before-registration race.
  unsafe{command.pre_exec(move||{
   if libc::prctl(libc::PR_SET_PDEATHSIG,libc::SIGKILL as libc::c_ulong,0 as libc::c_ulong,0 as libc::c_ulong,0 as libc::c_ulong)!=0{return Err(io::Error::last_os_error())}
   if libc::getppid()!=parent_pid{libc::_exit(1)}
   Ok(())
  });}
  let mut child=command.spawn()?;
  if let Err(error)=config::save(&self.data.join("run/core.pid"),&child.id()){let _=child.kill();let _=child.wait();return Err(error)}
  let _=fs::write(format!("/proc/{}/oom_score_adj",child.id()),"-1000");
  let pid=child.id();self.core=Some(child);self.core_pid.store(pid,Ordering::Release);self.generation+=1;self.event("核心已启动");Ok(())
 }
 fn enqueue(&mut self,parts:&[&str]){self.queue.push_back(parts.join(SEP))}
 fn enable_vpn(&mut self) {
  // The core restores this setting from its own account configuration. Select
  // virtual-network mode explicitly, once per core, before either role starts.
  // Re-sending the mode command can stop an existing service or connection.
  if !self.vpn_configured{self.enqueue(&["VPN","1"]);self.vpn_configured=true;}
 }
 fn login(&mut self){
  if self.account.user.is_empty()||self.ws.is_none()||self.login_pending.is_some(){return}
  let (u,p)=(self.account.user.clone(),self.account.password.clone());self.enqueue(&["login",&u,&p,"0","0"]);
  self.login_pending=Some(Instant::now());self.login_attempts+=1;self.state.error.clear();self.event("正在登录");
 }
 fn try_connect(&mut self){
  if self.connect_pending.is_some()||self.state.client_connected||self.state.server_running||!self.state.logged_in{return}
  if !self.conf.client.peer_id.is_empty(){let id=self.conf.client.peer_id.clone();self.enable_vpn();self.enqueue(&["conpc",&id]);}
  else if !self.conf.client.code.is_empty(){let(code,pwd)=(self.conf.client.code.clone(),self.conf.client.password.clone());self.enable_vpn();self.enqueue(&["concode",&code,&pwd]);}
  else{return}
  self.connect_pending=Some(Instant::now());self.connection_attempts+=1;self.state.error.clear();self.event("正在连接");
 }
 fn connection_expired(&self,now:Instant)->bool {
  self.connect_pending.is_some_and(|started|now.saturating_duration_since(started)>=CONNECT_TIMEOUT)
 }
 fn handle(&mut self,v:Value)->Result<Value,String> {
  let action=v["action"].as_str().ok_or("请求中缺少操作名称")?;
  let text=|name:&str|v[name].as_str().unwrap_or("").to_owned();
  match action {
   "status"=>{},
   "core-status"=>{self.core_installer.refresh();},
   "core-download"=>{
    self.core_installer.refresh();
    let info=self.core_installer.request_download().map_err(|e|format!("无法下载核心：{e}"))?;
    self.core_retry=Instant::now();
    self.event(if info.downloading{"正在下载核心，完成后自动校验和安装"}else{"核心已安装，文件校验通过"});
   },
   "core-check-update"=>{
    self.core_installer.refresh();
    let info=self.core_installer.request_check_update().map_err(|e|format!("无法检查核心更新：{e}"))?;
    self.event(if info.checking_update{"正在检查核心更新"}else{"核心操作正在进行，请稍候"});
   },
   "core-update"=>{
    self.core_installer.refresh();
    let info=self.core_installer.request_update().map_err(|e|format!("无法更新核心：{e}"))?;
    self.event(if info.downloading{"正在更新核心"}else{"已是推荐版本，或已有任务在运行"});
   },
   "login"=>{
    let (user,password)=(text("user").trim().to_owned(),text("password"));
    if user.is_empty()||password.is_empty()||!config::field(&user,64)||!config::field(&password,128){return Err("请输入有效账号和密码".into())}
    let account=Account{user,password};config::save(&self.data.join("account.json"),&account).map_err(|e|format!("账号保存失败：{e}"))?;
    self.account=account;self.restart_core();self.login_attempts=0;self.event("账号已保存");
   },
   "logout"=>{
    let cleared=clear_account_file(&self.data);
    self.account=Account::default();self.restart_core();
    let mut conf=self.conf.clone();conf.role="idle".into();conf.auto_connect=false;let committed=self.commit_conf(conf);
    cleared.map_err(|e|format!("已停止连接，但账号信息删除失败：{e}"))?;committed?;
    self.event("已退出登录，并清除保存的账号");
   },
   "connect"|"connect-peer"=>{
    let mut conf=self.conf.clone();conf.role="client".into();conf.auto_connect=true;
    if action=="connect"{let code=text("code").trim().to_owned();let password=text("password");if !password.is_empty()||code!=self.conf.client.code||!self.conf.client.peer_id.is_empty(){conf.client.password=password}conf.client.code=code;conf.client.peer_id.clear();if conf.client.code.is_empty(){return Err("请输入识别码".into())}}
    else {conf.client.peer_id=text("id");conf.client.code.clear();if conf.client.peer_id.is_empty(){return Err("设备 ID 无效".into())}}
    self.commit_conf(conf)?;
    // A fresh core/control channel prevents queued commands or late replies for
    // the previous target from being attributed to this connection request.
    self.restart_core();self.connection_attempts=0;self.event("连接设置已保存");
   },
   "disconnect"=>{let mut conf=self.conf.clone();conf.role="idle".into();conf.auto_connect=false;self.commit_conf(conf)?;self.restart_core();self.event("已断开连接，自动重连已关闭");},
   "server-start"=>{
    let mut conf=self.conf.clone();conf.role="server".into();
    for key in ["conpwd","webpwd","lanip"]{let val=text(key);if !val.is_empty(){match key{"conpwd"=>conf.server.conpwd=val,"webpwd"=>conf.server.webpwd=val,_=>conf.server.lanip=val}}}
    conf.validate()?;if conf.server.conpwd.is_empty()||conf.server.webpwd.is_empty(){return Err("请设置连接密码和 Web 访问密码".into())}
    self.commit_conf(conf)?;self.restart_core();self.event("服务端设置已保存");
   },
   "server-stop"=>{let mut conf=self.conf.clone();conf.role="idle".into();self.commit_conf(conf)?;self.restart_core();self.event("正在停止服务端");},
   "settings-save"=>{
    let mut merged=serde_json::to_value(&self.conf).unwrap();merge(&mut merged,&v["settings"]);
    let conf:Settings=serde_json::from_value(merged).map_err(|_|"设置格式有误")?;
    let restart=connection_config_changed(&self.conf,&conf);
    let probe_changed=self.conf.probe_ip!=conf.probe_ip||self.conf.probe_port!=conf.probe_port;
    self.commit_conf(conf)?;if restart{self.restart_core()}else if probe_changed{self.invalidate_probe()}
    self.renamed=false;self.event("设置已保存");
   },
   "restart"=>{self.restart_core();self.event("正在重启核心");},
   "stop"=>{fs::write(self.data.join("stop"),"").map_err(|e|e.to_string())?;self.invalidate_probe();EXIT.store(true,Ordering::Relaxed);self.event("正在停止后台服务");},
   "doctor"=>{let mut out=self.snapshot();out["diagnostics"]=json!({"tun_device":std::path::Path::new("/dev/net/tun").exists(),"control_connected":self.ws.is_some(),"core_running":self.core.is_some(),"logged_in":self.state.logged_in,"route":self.tun.routed,"data_verified":self.data_ok,"probe_ip":self.conf.probe_ip,"probe_port":self.conf.probe_port,"dns_loopback":Command::new("ss").args(["-lnu"]).output().ok().is_some_and(|o|String::from_utf8_lossy(&o.stdout).contains("127.0.0.1:53"))});return Ok(out)},
   "logs"=>{let kind=if text("kind")=="core"{"core"}else{"manager"};let data=fs::read_to_string(self.data.join(format!("logs/{kind}.log"))).unwrap_or_default();let lines:Vec<_>=data.lines().rev().take(100).collect();return Ok(json!({"ok":true,"text":lines.into_iter().rev().collect::<Vec<_>>().join("\n")}))},
   _=>return Err("未知操作".into())
  }
  Ok(self.snapshot())
 }
 fn tick(&mut self){
  let now=Instant::now();
  let revision=self.core_installer.revision();
  if revision!=self.core_revision{self.core_revision=revision;self.restart_core();self.event("核心已更新，正在恢复登录和连接");}
  if self.core.as_mut().is_some_and(|c|c.try_wait().ok().flatten().is_some()) {self.stop_core();self.core_retry=now+Duration::from_secs(3);self.login_retry=self.core_retry;self.state.error="核心已退出，正在重启".into();self.event("核心已退出，正在重启");}
  if self.core.is_none()&&self.core_installer.status().verified&&now>=self.core_retry{if let Err(e)=self.spawn_core(){self.core_installer.refresh();self.state.error=format!("核心启动失败：{e}");self.core_retry=now+Duration::from_secs(10)}else{self.core_retry=now+Duration::from_secs(1)}}
  if self.ws.is_none()&&self.core.is_some()&&now>=self.core_retry{match Ws::connect(PORT){Ok(ws)=>{self.ws=Some(ws);self.event("已连接核心");self.login_retry=now+Duration::from_secs(1)},Err(_)=>self.core_retry=now+Duration::from_secs(2)}}
  let incoming=match self.ws.as_mut(){Some(w)=>w.poll(),None=>Ok(Vec::new())};
  match incoming {
   Ok(messages)=>for message in messages {
    let old=self.state.logged_in;let serving=self.state.server_running;let connected=self.state.client_connected;let ip=self.state.local_ip.clone();let kind=message.split(SEP).next().unwrap_or("");
    protocol::apply_message(&message,&mut self.state);
    if connected!=self.state.client_connected||ip!=self.state.local_ip{self.invalidate_probe();}
    if old&&!self.state.logged_in {
     let error=self.state.error.clone();self.restart_core();self.core_retry=now+Duration::from_secs(3);self.login_retry=self.core_retry;self.state.error=error;
     self.event("登录连接已断开，正在重新登录");break;
    }
    if self.state.logged_in&&!old{self.login_pending=None;self.login_attempts=0;self.event("登录成功");self.enqueue(&["pclist",""])}
    if self.state.server_running{self.server_pending=None;}
    if serving&&!self.state.server_running&&self.conf.role=="server" {
     self.restart_core();self.core_retry=now+Duration::from_secs(3);self.login_retry=self.core_retry;
     self.state.error="服务端已停止，正在重新启动".into();self.event("服务端已停止，正在重新启动");break;
    }
    if self.state.client_connected{if self.connect_pending.take().is_some(){self.event("连接已建立，正在检查数据是否连通")}self.connect_retry=now+Duration::from_secs(30);}
    if matches!(kind,"info"|"tips"|"0")&&self.login_pending.is_some()&&!self.state.logged_in&&!self.state.error.is_empty(){self.login_pending=None;self.login_retry=now+Duration::from_secs((self.login_attempts as u64*5).clamp(5,60));self.event("登录失败，稍后重试");}
    if message.starts_with("conerr")||message.starts_with("conpc<$!$>no"){self.connect_pending=None;self.connect_retry=now+Duration::from_secs(30);self.event("连接被拒绝，30 秒后重试");}
    if kind=="discon" {self.connect_pending=None;self.connect_retry=now+Duration::from_secs(3);self.event("连接已断开，等待重连");}
   },
   Err(_)=>{self.stop_core();self.core_retry=now+Duration::from_secs(3);self.login_retry=self.core_retry;self.state.error="与核心的连接已断开，正在重试".into();self.event("与核心的连接已断开，正在重启核心");}
  }
  if self.login_pending.is_some_and(|t|t.elapsed()>Duration::from_secs(30)){self.restart_core();self.core_retry=now+Duration::from_secs(10);self.login_retry=self.core_retry;self.state.error="登录超时，等待重试".into();self.event("登录超时，正在重启核心");}
  if self.server_pending.is_some_and(|t|t.elapsed()>Duration::from_secs(30)) {
   self.restart_core();self.core_retry=now+Duration::from_secs(10);self.login_retry=self.core_retry;
   self.state.error="服务端启动超时，等待重试".into();self.event("服务端启动超时，正在重启核心");
  }
  if self.ws.is_some()&&!self.state.logged_in&&now>=self.login_retry{self.login()}
  if self.state.logged_in&&self.ws.is_some(){
   if !self.renamed&&!self.conf.pcname.is_empty(){let n=self.conf.pcname.clone();self.enqueue(&["newpcname",&n,""]);self.renamed=true;}
   if now.duration_since(self.last_poll)>Duration::from_secs(10){self.enqueue(&["pclist",""]);self.last_poll=now;}
   if self.conf.role=="server"&&!self.state.server_running&&self.server_pending.is_none() {
    let(c,w,ip)=(self.conf.server.conpwd.clone(),self.conf.server.webpwd.clone(),self.conf.server.lanip.clone());
    if !c.is_empty()&&!w.is_empty(){self.enable_vpn();self.enqueue(&["startServer",&c,"0",&w,&ip]);self.server_pending=Some(now);self.event("正在启动服务端");}
   }
   if self.conf.role=="client"&&self.conf.auto_connect&&!self.state.server_running&&now>=self.connect_retry {self.try_connect()}
  }
  if self.connection_expired(now){self.restart_core();self.connect_retry=now+Duration::from_secs(30);self.state.error="连接超时，30 秒后重试".into();self.event("连接超时，正在重启核心");}
  if self.ws.is_some()&&now.duration_since(self.last_send)>Duration::from_millis(500){if let Some(msg)=self.queue.pop_front(){let failed=self.ws.as_mut().unwrap().send(&msg).is_err();self.last_send=now;if failed{self.restart_core();self.core_retry=now+Duration::from_secs(3);self.login_retry=self.core_retry;self.state.error="向核心发送命令失败，正在重试".into();self.event("命令发送失败，正在重启核心");}}}
  if now.duration_since(self.last_route)>Duration::from_secs(2){let tun=routes::reconcile();if self.tun.ip!=tun.ip||self.tun.routed!=tun.routed{self.invalidate_probe();}self.tun=tun;self.last_route=now;}
  for _ in 0..16 {
   let Ok((epoch,ok))=self.probe_rx.try_recv()else{break};
   if self.record_probe(epoch,ok){self.restart_core();self.core_retry=now+Duration::from_secs(3);self.login_retry=self.core_retry;self.connect_retry=self.core_retry;self.state.error="连续三次连接检查失败，正在重连".into();self.event("连续三次连接检查失败，正在重连");break;}
  }
  if self.state.client_connected&&self.tun.routed&&!self.probe_busy&&now.duration_since(self.last_probe)>Duration::from_secs(15){
   self.probe_busy=true;self.last_probe=now;let(ip,port,tx,epoch)=(self.conf.probe_ip.clone(),self.conf.probe_port,self.probe_tx.clone(),self.probe_epoch);
   if thread::Builder::new().name("np-tunnel-probe".into()).spawn(move||{let ok=if port!=0{format!("{ip}:{port}").parse().ok().is_some_and(|address|std::net::TcpStream::connect_timeout(&address,Duration::from_secs(3)).is_ok())}else{Command::new("ping").args(["-c","1","-W","2",&ip]).output().is_ok_and(|o|o.status.success())};let _=tx.send((epoch,ok));}).is_err(){self.probe_busy=false;self.event("连接检查启动失败");}
  }
 }
}
fn merge(dst:&mut Value,src:&Value){if let(Some(a),Some(b))=(dst.as_object_mut(),src.as_object()){for(k,v)in b{if let Some(d)=a.get_mut(k){if d.is_object()&&v.is_object(){merge(d,v)}else{*d=v.clone()}}}}}
pub fn run(data:PathBuf,module:PathBuf)->io::Result<()> {
 unsafe{libc::umask(0o077);}
 for p in [&data,&data.join("run"),&data.join("logs")]{fs::create_dir_all(p)?;fs::set_permissions(p,fs::Permissions::from_mode(0o700))?;}
 let lock=fs::OpenOptions::new().create(true).write(true).open(data.join("run/daemon.lock"))?;
 if unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0{return Err(io::Error::other("already running"))}
 reap_stale_core(&data,&module)?;
 unsafe{libc::signal(libc::SIGTERM,signal as *const () as libc::sighandler_t);libc::signal(libc::SIGINT,signal as *const () as libc::sighandler_t);}
 let _=fs::write("/proc/self/oom_score_adj","-1000");fs::write(data.join("run/daemon.pid"),std::process::id().to_string())?;
 let socket=data.join("run/control.sock");let _=fs::remove_file(&socket);let listener=UnixListener::bind(&socket)?;fs::set_permissions(&socket,fs::Permissions::from_mode(0o600))?;
 let(tx,rx)=mpsc::channel::<Request>();let clients=Arc::new(AtomicUsize::new(0));
 thread::spawn(move||for incoming in listener.incoming(){let Ok(stream)=incoming else{break};if clients.fetch_add(1,Ordering::AcqRel)>=16{clients.fetch_sub(1,Ordering::AcqRel);continue}let count=clients.clone();let tx=tx.clone();thread::spawn(move||{
  let _=stream.set_read_timeout(Some(Duration::from_secs(1)));let _=stream.set_write_timeout(Some(Duration::from_secs(1)));
  let mut reader=BufReader::new(stream);let mut bytes=Vec::new();
  let payload=reader.by_ref().take(16385).read_until(b'\n',&mut bytes).ok().filter(|_|bytes.len()<=16384).and_then(|_|serde_json::from_slice::<Value>(&bytes).ok());
  if let Some(v)=payload{let(rt,rr)=mpsc::channel();if tx.send((v,rt)).is_ok(){if let Ok(result)=rr.recv_timeout(Duration::from_secs(2)){let _=writeln!(reader.get_mut(),"{result}");}}}
  count.fetch_sub(1,Ordering::AcqRel);
 });});
 let conf=config::read(&data.join("settings.json"));let account=config::read(&data.join("account.json"));
 let mut m=Manager::new(data,module,conf,account);
 m.conf.validate().map_err(io::Error::other)?;m.persist()?;routes::protect_control(true);
 match nat_retry::start(m.core_pid.clone()){Ok(count)=>{m.nat_retries=count;m.nat_retry_running=true;},Err(e)=>m.event(&format!("NAT 探测重试启动失败：{e}"))}
 if let Err(e)=dns::start(){m.event(&format!("DNS 转发启动失败：{e}"))}
 m.event("后台服务已启动");let mut saved=Instant::now();
 while !EXIT.load(Ordering::Relaxed)&&!m.data.join("stop").exists()&&!m.data.join("safe_mode").exists()&&!m.module.join("disable").exists()&&!m.module.join("remove").exists(){
  for(v,reply)in rx.try_iter().take(16){let result=m.handle(v).unwrap_or_else(|e|json!({"ok":false,"error":e}));let _=reply.send(result);}
  m.tick();if saved.elapsed()>Duration::from_secs(2){let _=config::save(&m.data.join("run/state.json"),&m.snapshot());let _=fs::set_permissions(m.data.join("core/data/config"),fs::Permissions::from_mode(0o600));saved=Instant::now();}
  thread::sleep(Duration::from_millis(100));
 }
 m.stop_core();routes::protect_control(false);let _=fs::remove_file(socket);let _=fs::remove_file(m.data.join("run/daemon.pid"));Ok(())
}

#[cfg(test)]
mod tests {
 use super::*;
 static NEXT:AtomicUsize=AtomicUsize::new(0);
 struct TestDir(PathBuf);
 impl TestDir {
  fn new()->Self {
   let path=std::env::temp_dir().join(format!("np-manager-test-{}-{}",std::process::id(),NEXT.fetch_add(1,Ordering::Relaxed)));
   fs::create_dir_all(&path).unwrap();Self(path)
  }
 }
 impl Drop for TestDir {
  fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}
 }
 fn manager(data:PathBuf)->Manager {Manager::new(data,PathBuf::from("/unused-test-module"),Settings::default(),Account::default())}
 fn connected_manager()->Manager {
  let mut m=manager(PathBuf::from("/unused-test-data"));m.conf.role="client".into();m.state.logged_in=true;m.state.client_connected=true;m.tun.routed=true;m
 }

 #[test]
 fn client_selects_virtual_network_before_code_or_peer_connection() {
  for by_peer in [false,true] {
   let mut m=manager(PathBuf::from("/unused-test-data"));m.state.logged_in=true;
   if by_peer{m.conf.client.peer_id="fixture-peer".into();}else{m.conf.client.code="87654321".into();m.conf.client.password="fixture-password".into();}
   m.try_connect();
   let expected=if by_peer{["conpc","fixture-peer"].join(SEP)}else{["concode","87654321","fixture-password"].join(SEP)};
   assert_eq!(m.queue,VecDeque::from([["VPN","1"].join(SEP),expected]));
   assert!(m.connect_pending.is_some());assert_eq!(m.connection_attempts,1);
   m.try_connect();assert_eq!(m.queue.len(),2);assert_eq!(m.connection_attempts,1);
  }
 }

 #[test]
 fn retry_preserves_mode_but_new_core_selects_it_again() {
  let mut m=manager(PathBuf::from("/unused-test-data"));m.state.logged_in=true;m.conf.client.code="87654321".into();
  m.try_connect();m.queue.clear();m.connect_pending=None;
  m.try_connect();assert_eq!(m.queue,VecDeque::from([["concode","87654321",""].join(SEP)]));
  m.clear_control();m.connect_pending=None;
  m.try_connect();assert_eq!(m.queue.front(),Some(&["VPN","1"].join(SEP)));assert_eq!(m.queue.len(),2);
 }

 #[test]
 fn existing_session_server_or_missing_target_cannot_change_mode() {
  for condition in ["connected","server","logged-out","no-target"] {
   let mut m=manager(PathBuf::from("/unused-test-data"));m.state.logged_in=true;m.conf.client.code="87654321".into();
   match condition {
    "connected"=>m.state.client_connected=true,
    "server"=>m.state.server_running=true,
    "logged-out"=>m.state.logged_in=false,
    _=>m.conf.client.code.clear(),
   }
   m.try_connect();assert!(m.queue.is_empty());assert!(!m.vpn_configured);assert!(m.connect_pending.is_none());
  }
 }

 #[test]
 fn connection_waits_for_full_deadline_and_cancellation_clears_it() {
  let mut m=manager(PathBuf::from("/unused-test-data"));let started=Instant::now();m.connect_pending=Some(started);
  assert!(!m.connection_expired(started+Duration::from_secs(30)));
  assert!(!m.connection_expired(started+Duration::from_secs(89)));
  assert!(m.connection_expired(started+Duration::from_secs(90)));
  m.connect_pending=None;assert!(!m.connection_expired(started+Duration::from_secs(120)));
 }

 #[test]
 fn official_identity_is_migrated_and_retained_without_core_auto_login() {
  let dir=TestDir::new();let module=dir.0.join("module");let data=dir.0.join("runtime");
  fs::create_dir_all(module.join("bin/data")).unwrap();
  prepare_identity(&data,&module).unwrap();assert!(!data.join("core/data/config").exists());
  let identity=json!({"Auto_login":true,"Global_pwd":"encrypted-mac-fixture","Users":[{"User":"fixture","Client_pwd":"encrypted-pcid-fixture"}]});
  config::save(&module.join("bin/data/config"),&identity).unwrap();
  prepare_identity(&data,&module).unwrap();
  let path=data.join("core/data/config");let first:Value=config::read(&path);
  assert_eq!(first["Auto_login"],false);assert_eq!(first["Users"],identity["Users"]);assert_eq!(first["Global_pwd"],identity["Global_pwd"]);
  fs::remove_file(module.join("bin/data/config")).unwrap();prepare_identity(&data,&module).unwrap();
  assert_eq!(config::read::<Value>(&path),first);assert_eq!(fs::metadata(&path).unwrap().permissions().mode()&0o777,0o600);
  prepare_identity(&data,&module).unwrap();assert_eq!(config::read::<Value>(&path),first);
  fs::write(&path,b"broken identity config").unwrap();assert!(prepare_identity(&data,&module).is_err());assert_eq!(fs::read(path).unwrap(),b"broken identity config");
 }

 #[test]
 fn settings_write_failure_keeps_live_configuration_and_generation() {
  let dir=TestDir::new();let mut m=manager(dir.0.join("absent-parent"));
  m.probe_epoch=7;m.generation=9;m.conf.pcname="original".into();
  let result=m.handle(json!({"action":"settings-save","settings":{"role":"client","client":{"code":"87654321"},"pcname":"changed"}}));
  assert!(result.is_err());assert_eq!(m.conf.role,"idle");assert_eq!(m.conf.pcname,"original");assert!(m.conf.client.code.is_empty());assert_eq!(m.generation,9);assert_eq!(m.probe_epoch,7);
 }

 #[test]
 fn target_and_role_changes_require_fresh_core_but_name_changes_do_not() {
  let original=Settings::default();let mut changed=original.clone();changed.pcname="new name".into();changed.autostart=false;
  assert!(!connection_config_changed(&original,&changed));
  changed=original.clone();changed.role="client".into();assert!(connection_config_changed(&original,&changed));
  changed=original.clone();changed.client.code="87654321".into();assert!(connection_config_changed(&original,&changed));
  changed=original.clone();changed.client.peer_id="7".into();assert!(connection_config_changed(&original,&changed));
  changed=original.clone();changed.client.password="changed target password".into();assert!(connection_config_changed(&original,&changed));
  let mut server=original;server.role="server".into();changed=server.clone();changed.server.lanip="192.168.5.56".into();assert!(connection_config_changed(&server,&changed));
 }

 #[test]
 fn late_probe_cannot_complete_a_new_probe_or_change_connection_health() {
  let mut m=connected_manager();let old=m.probe_epoch;m.invalidate_probe();m.probe_busy=true;
  assert!(!m.record_probe(old,true));assert!(m.probe_busy);assert!(!m.data_ok);assert_eq!(m.probe_failures,0);
  assert!(!m.record_probe(m.probe_epoch,true));assert!(!m.probe_busy);assert!(m.data_ok);
 }

 #[test]
 fn default_ping_cannot_tear_down_an_unverified_link_but_tcp_can() {
  let mut m=connected_manager();let epoch=m.probe_epoch;
  for _ in 0..3 {assert!(!m.record_probe(epoch,false));}
  assert!(!m.record_probe(epoch,true));
  assert!(!m.record_probe(epoch,false));assert!(!m.record_probe(epoch,false));assert!(m.record_probe(epoch,false));
  m.invalidate_probe();m.conf.probe_port=80;let epoch=m.probe_epoch;
  assert!(!m.record_probe(epoch,false));assert!(!m.record_probe(epoch,false));assert!(m.record_probe(epoch,false));
 }

 #[test]
 fn logout_removes_saved_credentials_and_temporary_account_file() {
  let dir=TestDir::new();config::save(&dir.0.join("account.json"),&Account{user:"fixture".into(),password:"fixture-secret".into()}).unwrap();
  clear_account_file(&dir.0).unwrap();assert!(!dir.0.join("account.json").exists());assert!(!dir.0.join("account.tmp").exists());
  clear_account_file(&dir.0).unwrap();assert!(fs::read_dir(&dir.0).unwrap().next().is_none());
 }
}
