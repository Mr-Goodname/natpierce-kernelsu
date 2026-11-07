use serde::Serialize;
use std::{collections::BTreeMap,fs,path::PathBuf,process::Command,sync::OnceLock};
pub const TABLE:&str="17861";
const PRIORITY:&str="8997";
const SUBNET:&str="10.6.22.0/24";
const CONTROL_OWNER:&str="natpierce_rs-control";
static NAT_BASELINE:OnceLock<Vec<Vec<String>>>=OnceLock::new();
#[derive(Clone, Default, Serialize)]
pub struct Tun { pub present:bool, pub ip:String, pub routed:bool, pub rx_bytes:u64, pub tx_bytes:u64, pub detail:String }
fn cmd(args:&[&str])->bool { Command::new("ip").args(args).output().is_ok_and(|x|x.status.success()) }
fn output(args:&[&str])->String { Command::new("ip").args(args).output().ok().map(|x|String::from_utf8_lossy(&x.stdout).into_owned()).unwrap_or_default() }
fn baseline_path()->PathBuf {std::env::var_os("NP_DATA").map(PathBuf::from).unwrap_or_else(||"/data/adb/natpierce-rs".into()).join("run/network-baseline.json")}
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
    cleanup_native_nat()
}
pub fn protect_control(add:bool) {
    if add{prepare_native_nat();}
    // Official core listens on all interfaces. Only root CLI controls it.
    let op=if add{"-A"}else{"-D"};
    for tail in [vec!["INPUT","!","-i","lo","-p","tcp","--dport","33472","-m","comment","--comment",CONTROL_OWNER,"-j","REJECT"],vec!["OUTPUT","-p","tcp","--dport","33472","-m","owner","!","--uid-owner","0","-m","comment","--comment",CONTROL_OWNER,"-j","REJECT"]] {
        let exists=Command::new("iptables").arg("-C").args(&tail).output().is_ok_and(|o|o.status.success());
        if add && exists || !add && !exists {continue}
        let _=Command::new("iptables").arg(op).args(&tail).output();
    }
    if !add&&cleanup_native_nat(){let _=fs::remove_file(baseline_path());}
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
}
