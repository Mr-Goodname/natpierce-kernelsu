use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, io, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::Path};

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Account { pub user: String, #[serde(alias="pass")] pub password: String }
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub autostart: bool,
    pub role: String,
    pub auto_connect: bool,
    pub pcname: String,
    pub client: Client,
    pub server: Server,
    pub probe_ip: String,
    pub probe_port: u16,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Client { pub code: String, pub password: String, pub peer_id: String }
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Server { pub conpwd: String, pub webpwd: String, pub lanip: String }
impl Default for Server { fn default() -> Self { Self { conpwd: String::new(), webpwd: String::new(), lanip: String::new() } } }
impl Default for Settings {
    fn default() -> Self { Self { autostart:true, role:"idle".into(), auto_connect:true, pcname:"Android-KernelSU".into(), client:Client::default(), server:Server::default(), probe_ip:"10.6.22.1".into(), probe_port:0 } }
}
pub fn read<T: serde::de::DeserializeOwned + Default>(path:&Path) -> T {
    fs::read(path).ok().and_then(|b|serde_json::from_slice(&b).ok()).unwrap_or_default()
}
pub fn save<T: Serialize>(path:&Path,value:&T)->io::Result<()> {
    use std::io::Write;
    let tmp=path.with_extension("tmp");
    let mut f=fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
    fs::set_permissions(&tmp,fs::Permissions::from_mode(0o600))?;
    f.write_all(&serde_json::to_vec(value)?)?; f.sync_all()?; fs::rename(tmp,path)?;
    if let Some(parent)=path.parent(){ fs::File::open(parent)?.sync_all()?; }
    Ok(())
}
pub fn field(s:&str,max:usize)->bool { s.len()<=max && !s.contains("<$!$>") && !s.chars().any(|c|c.is_control()) }
impl Settings {
    pub fn public(&self)->Value { json!({"autostart":self.autostart,"role":self.role,"auto_connect":self.auto_connect,"pcname":self.pcname,"client":{"code":self.client.code,"peer_id":self.client.peer_id,"has_password":!self.client.password.is_empty()},"server":{"lanip":self.server.lanip,"has_password":!self.server.conpwd.is_empty()},"probe_ip":self.probe_ip,"probe_port":self.probe_port}) }
    pub fn validate(&self)->Result<(),String> {
        if !["idle","client","server"].contains(&self.role.as_str()){return Err("角色无效".into())}
        if !field(&self.pcname,64)||!field(&self.client.password,128)||!field(&self.server.conpwd,128)||!field(&self.server.webpwd,128){return Err("字段包含非法字符或过长".into())}
        if !self.client.code.is_empty() && (self.client.code.len()!=8 || !self.client.code.bytes().all(|b|b.is_ascii_digit())) {return Err("识别码应为 8 位数字".into())}
        if !self.client.peer_id.is_empty() && !self.client.peer_id.bytes().all(|b|b.is_ascii_digit()){return Err("设备 ID 无效".into())}
        if !self.server.lanip.is_empty() && self.server.lanip.parse::<std::net::Ipv4Addr>().is_err(){return Err("服务端地址应为 IPv4 地址".into())}
        let ip=self.probe_ip.parse::<std::net::Ipv4Addr>().map_err(|_|"探测地址应为隧道 IPv4 地址")?;
        if ip.octets()[..3]!=[10,6,22]{return Err("探测地址应在皎月连隧道 10.6.22.0/24 内".into())}
        Ok(())
    }
}
