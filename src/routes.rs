use serde::{Deserialize,Serialize};
use std::{collections::BTreeMap,fs,io::{self,Read},path::PathBuf,process::Command,sync::OnceLock};
pub const TABLE:&str="17861";
const PRIORITY:&str="8997";
const SUBNET:&str="10.6.22.0/24";
const CONTROL_OWNER:&str="natpierce_rs-control";
static NAT_BASELINE:OnceLock<Vec<Vec<String>>>=OnceLock::new();
static FORWARD_BASELINE:OnceLock<Option<Vec<Vec<String>>>>=OnceLock::new();
const FORWARD_BASELINE_BYTES:u64=65536;
const FORWARD_BASELINE_RULES:usize=256;
#[derive(Deserialize,Serialize)]
#[serde(deny_unknown_fields)]
struct ForwardBaseline {schema:u8,rules:Vec<Vec<String>>}
#[derive(Clone, Default, Serialize)]
pub struct Tun { pub present:bool, pub ip:String, pub routed:bool, pub rx_bytes:u64, pub tx_bytes:u64, pub detail:String }
fn cmd(args:&[&str])->bool { Command::new("ip").args(args).output().is_ok_and(|x|x.status.success()) }
fn output(args:&[&str])->String { Command::new("ip").args(args).output().ok().map(|x|String::from_utf8_lossy(&x.stdout).into_owned()).unwrap_or_default() }
fn baseline_path()->PathBuf {std::env::var_os("NP_DATA").map(PathBuf::from).unwrap_or_else(||"/data/adb/natpierce-rs".into()).join("run/network-baseline.json")}
fn forward_baseline_path()->PathBuf {baseline_path().with_file_name("forward-baseline.json")}
// The Linux core creates precisely this untagged rule. Preserve any identical
// pre-existing rules; never touch extra matches, comments, or other subnets.
fn native_nat_rule(line:&str)->Option<Vec<String>> {
    let words:Vec<_>=line.split_whitespace().collect();
    if words.len()!=8||words[0]!="-A"||words[1]!="POSTROUTING"||words[2]!="-s"||words[3]!=SUBNET||words[4]!="-o"||words[6]!="-j"||words[7]!="MASQUERADE"{return None}
    if words[5].is_empty()||words[5].len()>15||!words[5].bytes().all(|b|b.is_ascii_alphanumeric()||b"_.:-".contains(&b)){return None}
    Some(words.into_iter().map(str::to_owned).collect())
}
fn native_nat_rules()->Option<Vec<Vec<String>>> {
    let out=Command::new("iptables").args(["-t","nat","-S","POSTROUTING"]).output().ok()?;
    if !out.status.success(){return None}
    Some(String::from_utf8_lossy(&out.stdout).lines().filter_map(native_nat_rule).collect())
}
fn excess_rules(current:&[Vec<String>],baseline:&[Vec<String>])->Vec<Vec<String>> {
    let mut kept=BTreeMap::<Vec<String>,usize>::new();for rule in baseline{*kept.entry(rule.clone()).or_default()+=1}
    current.iter().filter_map(|rule|{let count=kept.entry(rule.clone()).or_default();if *count>0{*count-=1;None}else{Some(rule.clone())}}).collect()
}
fn prepare_native_nat() {
    if NAT_BASELINE.get().is_some(){return}
    let path=baseline_path();
    // A previous manager may have died before cleaning the Linux core's rules.
    // Resolve that delta before taking the new baseline, preserving its record
    // if removal failed. Never infer an empty baseline from an iptables error.
    if path.exists()&&!cleanup_native_nat(){
        if let Ok(bytes)=fs::read(&path){if let Ok(rules)=serde_json::from_slice::<Vec<Vec<String>>>(&bytes){let _=NAT_BASELINE.set(rules);}}
        return
    }
    let Some(rules)=native_nat_rules()else{return};
    if let Some(parent)=path.parent(){let _=fs::create_dir_all(parent);}
    let _=crate::config::save(&path,&rules);let _=NAT_BASELINE.set(rules);
}
fn cleanup_native_nat()->bool {
    let baseline=if let Some(rules)=NAT_BASELINE.get(){Some(rules.clone())}else{
        fs::read(baseline_path()).ok().and_then(|b|serde_json::from_slice::<Vec<Vec<String>>>(&b).ok()).filter(|rules|rules.iter().all(|r|native_nat_rule(&r.join(" ")).as_ref()==Some(r)))
    };
    let Some(baseline)=baseline else{return true};
    let Some(current)=native_nat_rules()else{return false};
    let excess=excess_rules(&current,&baseline);let mut success=excess.len()<=64;
    for mut rule in excess.into_iter().take(64){rule[0]="-D".into();success&=Command::new("iptables").args(["-t","nat"]).args(&rule).output().is_ok_and(|out|out.status.success());}
    success
}
fn uplink_interface(name:&str)->bool {
    !name.is_empty()&&name.len()<=15&&name!="natpierce"&&name!="lo"
        &&name.as_bytes()[0].is_ascii_alphanumeric()
        &&name.bytes().all(|b|b.is_ascii_alphanumeric()||b"_.:-".contains(&b))
}
// The core's filter rules are untagged too. Only these complete forms can
// belong to it; rules with another tunnel or additional matches stay intact.
fn native_forward_rule(line:&str)->Option<Vec<String>> {
    let words:Vec<_>=line.split_whitespace().collect();
    if words.len()!=8&&words.len()!=12{return None}
    if words[0]!="-A"||words[1]!="FORWARD"||words[2]!="-i"||words[4]!="-o"{return None}
    let outbound=words.len()==8&&words[3]=="natpierce"&&uplink_interface(words[5])
        &&words[6]=="-j"&&words[7]=="ACCEPT";
    let inbound=words.len()==12&&uplink_interface(words[3])&&words[5]=="natpierce"
        &&words[6]=="-m"&&words[7]=="conntrack"&&words[8]=="--ctstate"
        &&["RELATED,ESTABLISHED","ESTABLISHED,RELATED"].contains(&words[9])
        &&words[10]=="-j"&&words[11]=="ACCEPT";
    if !outbound&&!inbound{return None}
    Some(words.into_iter().map(str::to_owned).collect())
}
fn native_forward_rules()->Option<Vec<Vec<String>>> {
    let out=Command::new("iptables").args(["-t","filter","-S","FORWARD"]).output().ok()?;
    if !out.status.success(){return None}
    Some(String::from_utf8_lossy(&out.stdout).lines().filter_map(native_forward_rule).collect())
}
fn decode_forward_baseline(bytes:&[u8])->Option<Vec<Vec<String>>> {
    if bytes.len()>FORWARD_BASELINE_BYTES as usize{return None}
    let record:ForwardBaseline=serde_json::from_slice(bytes).ok()?;
    if record.schema!=1||record.rules.len()>FORWARD_BASELINE_RULES
        ||!record.rules.iter().all(|r|native_forward_rule(&r.join(" ")).as_ref()==Some(r)){return None}
    Some(record.rules)
}
fn load_forward_baseline()->io::Result<Option<Vec<Vec<String>>>> {
    let file=match fs::File::open(forward_baseline_path()){
        Ok(file)=>file,Err(e) if e.kind()==io::ErrorKind::NotFound=>return Ok(None),Err(e)=>return Err(e)
    };
    let mut bytes=Vec::new();file.take(FORWARD_BASELINE_BYTES+1).read_to_end(&mut bytes)?;
    decode_forward_baseline(&bytes).map(Some).ok_or_else(||io::Error::new(io::ErrorKind::InvalidData,"invalid FORWARD baseline"))
}
fn remove_forward_delta(baseline:&[Vec<String>])->bool {
    let Some(current)=native_forward_rules()else{return false};
    let excess=excess_rules(&current,baseline);let mut success=excess.len()<=64;
    for mut rule in excess.into_iter().take(64){rule[0]="-D".into();success&=Command::new("iptables").args(["-t","filter"]).args(&rule).output().is_ok_and(|out|out.status.success());}
    success
}
fn prepare_native_forward() {
    if FORWARD_BASELINE.get().is_some(){return}
    match load_forward_baseline(){
        Ok(Some(rules)) if !remove_forward_delta(&rules)=>{let _=FORWARD_BASELINE.set(Some(rules));return},
        Ok(_)=>{},
        Err(_)=>{let _=FORWARD_BASELINE.set(None);return}
    }
    let Some(rules)=native_forward_rules().filter(|rules|rules.len()<=FORWARD_BASELINE_RULES)else{let _=FORWARD_BASELINE.set(None);return};
    let path=forward_baseline_path();
    let saved=path.parent().is_some_and(|parent|fs::create_dir_all(parent).is_ok())
        &&crate::config::save(&path,&ForwardBaseline{schema:1,rules:rules.clone()}).is_ok();
    // A failed read or save must never become a trusted empty baseline.
    let _=FORWARD_BASELINE.set(if saved{Some(rules)}else{None});
}
fn cleanup_native_forward()->bool {
    let baseline=match FORWARD_BASELINE.get(){
        Some(Some(rules))=>rules.clone(),Some(None)=>return false,
        None=>match load_forward_baseline(){Ok(Some(rules))=>rules,Ok(None)=>return true,Err(_)=>return false}
    };
    remove_forward_delta(&baseline)
}
pub fn inspect()->Tun {
    let s=output(&["-o","-4","addr","show","dev","natpierce"]);
    let mut t=Tun::default();
    let words:Vec<_>=s.split_whitespace().collect();
    if let Some(pos)=words.iter().position(|w|*w=="inet") {
        t.ip=words.get(pos+1).unwrap_or(&"").split('/').next().unwrap_or("").into();
        t.present=t.ip.parse::<std::net::Ipv4Addr>().is_ok();
    }
    t.rx_bytes=fs::read_to_string("/sys/class/net/natpierce/statistics/rx_bytes").ok().and_then(|v|v.trim().parse().ok()).unwrap_or(0);
    t.tx_bytes=fs::read_to_string("/sys/class/net/natpierce/statistics/tx_bytes").ok().and_then(|v|v.trim().parse().ok()).unwrap_or(0);
    t.routed=t.present && output(&["rule","show"]).lines().any(|l|l.starts_with("8997:")&&l.contains(SUBNET)&&l.contains(TABLE)) && output(&["route","show","table",TABLE]).contains("dev natpierce");
    t.detail=if t.routed {"策略路由已安装"}else if t.present {"TUN 已创建，策略路由未就绪"}else{"官方核心尚未创建 TUN"}.into();
    t
}
pub fn reconcile()->Tun {
    let mut t=inspect();
    if t.present && !t.routed {
        if !output(&["rule","show"]).lines().any(|l|l.starts_with("8997:")&&l.contains(SUBNET)&&l.contains(TABLE)) {
            cmd(&["rule","add","priority",PRIORITY,"to",SUBNET,"lookup",TABLE]);
        }
        cmd(&["route","replace",SUBNET,"dev","natpierce","src",&t.ip,"table",TABLE]);
        t=inspect();
    }
    t
}
pub fn cleanup()->bool {
    for _ in 0..4 { if !cmd(&["rule","del","priority",PRIORITY,"to",SUBNET,"lookup",TABLE]){break} }
    // This table may also contain a route installed by the user. Delete only
    // the module's subnet route, with its known device.
    cmd(&["route","del",SUBNET,"dev","natpierce","table",TABLE]);
    let nat=cleanup_native_nat();let forward=cleanup_native_forward();nat&&forward
}
pub fn protect_control(add:bool) {
    if add{prepare_native_nat();prepare_native_forward();}
    // Official core listens on all interfaces. Only root CLI controls it.
    let op=if add{"-A"}else{"-D"};
    for program in ["iptables","ip6tables"] {
      for tail in [vec!["INPUT","!","-i","lo","-p","tcp","--dport","33472","-m","comment","--comment",CONTROL_OWNER,"-j","REJECT"],vec!["OUTPUT","-p","tcp","--dport","33472","-m","owner","!","--uid-owner","0","-m","comment","--comment",CONTROL_OWNER,"-j","REJECT"]] {
        let exists=Command::new(program).arg("-C").args(&tail).output().is_ok_and(|o|o.status.success());
        if add && exists || !add && !exists {continue}
        let _=Command::new(program).arg(op).args(&tail).output();
      }
    }
    if !add{
        if cleanup_native_nat(){let _=fs::remove_file(baseline_path());}
        if cleanup_native_forward(){let _=fs::remove_file(forward_baseline_path());}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_native_exact_rule_is_owned(){
        assert!(native_nat_rule("-A POSTROUTING -s 10.6.22.0/24 -o wlan0 -j MASQUERADE").is_some());
        for line in ["-A POSTROUTING -s 192.168.1.0/24 -o wlan0 -j MASQUERADE","-A POSTROUTING -s 10.6.22.0/24 -o wlan0 -m comment --comment another-module -j MASQUERADE","-A POSTROUTING -s 10.6.22.0/24 -o wlan0 -j SNAT","-A POSTROUTING -s 10.6.22.0/24 -o wlan0 -p tcp -j MASQUERADE"]{assert!(native_nat_rule(line).is_none());}
    }
    #[test]
    fn stopping_preserves_duplicate_rules_from_before_start(){
        let wifi=native_nat_rule("-A POSTROUTING -s 10.6.22.0/24 -o wlan0 -j MASQUERADE").unwrap();
        let mobile=native_nat_rule("-A POSTROUTING -s 10.6.22.0/24 -o rmnet_data0 -j MASQUERADE").unwrap();
        let before=vec![wifi.clone(),wifi.clone()];let after=vec![wifi.clone(),mobile.clone(),wifi.clone(),wifi.clone()];
        assert_eq!(excess_rules(&after,&before),vec![mobile,wifi]);
        assert!(excess_rules(&before,&before).is_empty());
    }
    #[test]
    fn only_complete_native_forward_rules_are_owned(){
        for line in ["-A FORWARD -i natpierce -o rmnet_data1 -j ACCEPT","-A FORWARD -i wlan0 -o natpierce -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT","-A FORWARD -i eth0 -o natpierce -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT"]{assert!(native_forward_rule(line).is_some());}
        for line in [
            "-A FORWARD -i tun0 -o rmnet_data1 -j ACCEPT",
            "-A FORWARD -i rmnet_data1 -o tun0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT",
            "-A FORWARD -i natpierce -o natpierce -j ACCEPT",
            "-A FORWARD -i natpierce -o lo -j ACCEPT",
            "-A FORWARD -i natpierce -o wlan+ -j ACCEPT",
            "-A FORWARD -i natpierce -o -bad -j ACCEPT",
            "-A FORWARD -i natpierce -o this_interface_is_too_long -j ACCEPT",
            "-A FORWARD -i natpierce -o wlan0 -s 10.6.22.0/24 -j ACCEPT",
            "-A FORWARD -i natpierce -o wlan0 -m comment --comment another-module -j ACCEPT",
            "-A FORWARD -i natpierce -o wlan0 -p tcp -j ACCEPT",
            "-A FORWARD -i natpierce -o wlan0 -j DROP",
            "-A FORWARD -i wlan0 -o natpierce -j ACCEPT",
            "-A FORWARD -i wlan0 -o natpierce -m state --ctstate RELATED,ESTABLISHED -j ACCEPT",
            "-A FORWARD -i wlan0 -o natpierce -m conntrack --ctstate NEW,RELATED,ESTABLISHED -j ACCEPT"
        ]{assert!(native_forward_rule(line).is_none(),"{line}");}
    }
    #[test]
    fn forward_cleanup_preserves_existing_duplicates(){
        let out=native_forward_rule("-A FORWARD -i natpierce -o rmnet_data1 -j ACCEPT").unwrap();
        let back=native_forward_rule("-A FORWARD -i rmnet_data1 -o natpierce -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT").unwrap();
        let baseline=vec![out.clone(),out.clone(),back.clone()];
        let current=vec![out.clone(),back.clone(),out.clone(),back.clone(),out.clone()];
        assert_eq!(excess_rules(&current,&baseline),vec![back,out]);
        assert!(excess_rules(&baseline,&baseline).is_empty());
    }
    #[test]
    fn forward_records_reject_unknown_or_unbounded_content(){
        let rule=native_forward_rule("-A FORWARD -i natpierce -o rmnet_data1 -j ACCEPT").unwrap();
        let record=serde_json::to_vec(&ForwardBaseline{schema:1,rules:vec![rule.clone()]}).unwrap();
        assert_eq!(decode_forward_baseline(&record),Some(vec![rule.clone()]));
        for invalid in [
            serde_json::json!({"schema":2,"rules":[rule.clone()]}),
            serde_json::json!({"schema":1,"rules":[],"extra":true}),
            serde_json::json!({"schema":1,"rules":[["-A","FORWARD","-j","ACCEPT"]]}),
            serde_json::json!({"schema":1,"rules":[["-A","FORWARD","-i","natpierce -p tcp","-o","wlan0","-j","ACCEPT"]]}),
            serde_json::json!({"schema":1,"rules":vec![rule;FORWARD_BASELINE_RULES+1]})
        ]{assert!(decode_forward_baseline(&serde_json::to_vec(&invalid).unwrap()).is_none());}
        assert!(decode_forward_baseline(b"[]").is_none());
        assert!(decode_forward_baseline(b"{broken").is_none());
        assert!(decode_forward_baseline(&vec![b' ';FORWARD_BASELINE_BYTES as usize+1]).is_none());
        assert_eq!(decode_forward_baseline(b"{\"schema\":1,\"rules\":[]}"),Some(vec![]));
    }
}
