//! The official core's local text protocol. Password-bearing fields are deliberately omitted.
use serde::Serialize;
use std::net::Ipv4Addr;

pub const SEP: &str = "<$!$>";

#[derive(Clone, Debug, Default, Serialize)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub cidr: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ServerClient {
    pub id: String,
    pub addr: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct State {
    pub logged_in: bool,
    pub phase: String,
    pub error: String,
    pub code: String,
    pub user: String,
    pub local_ip: String,
    pub client_connected: bool,
    pub server_running: bool,
    pub peers: Vec<Peer>,
    pub server_clients: Vec<ServerClient>,
    pub version: String,
    pub platform: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            logged_in: false,
            phase: "logged_out".into(),
            error: String::new(),
            code: String::new(),
            user: String::new(),
            local_ip: String::new(),
            client_connected: false,
            server_running: false,
            peers: Vec::new(),
            server_clients: Vec::new(),
            version: String::new(),
            platform: String::new(),
        }
    }
}

fn field<'a>(fields: &[&'a str], n: usize) -> &'a str {
    fields.get(n).copied().unwrap_or("")
}

fn plain(input: &str) -> String {
    let mut text = String::new();
    let mut tag = false;
    for ch in input.chars().take(4096) {
        match ch {
            '<' => tag = true,
            '>' => tag = false,
            _ if !tag && (!ch.is_control() || ch == '\n' || ch == '\t') => text.push(ch),
            _ => {}
        }
    }
    text.replace("&nbsp;", " ").trim().chars().take(1024).collect()
}

fn code(value: &str) -> String {
    let value = value.trim();
    if value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit()) {
        value.into()
    } else {
        String::new()
    }
}

fn ip(value: &str) -> String {
    value.trim().parse::<Ipv4Addr>().map(|a| a.to_string()).unwrap_or_default()
}

fn settle_phase(state: &mut State) {
    state.phase = if !state.logged_in {
        "logged_out"
    } else if state.server_running {
        "server"
    } else if state.client_connected {
        "client"
    } else {
        "idle"
    }.into();
}

/// Apply a complete message. Unknown or incomplete messages leave the state intact.
pub fn apply_message(raw: &str, state: &mut State) {
    if raw.len() > 1_048_576 {
        return;
    }
    let f: Vec<&str> = raw.split(SEP).collect();
    match field(&f, 0) {
        "start" if f.len() >= 3 => {
            state.version = plain(f[1]);
            state.platform = plain(f[2]);
        }
        "0" if f.len() >= 4 => {
            state.logged_in = false;
            state.server_running = false;
            state.client_connected = false;
            state.user = plain(f[1]);
            state.code.clear();
            state.local_ip.clear();
            state.server_clients.clear();
            state.peers.clear();
            state.error = plain(field(&f, 5));
            settle_phase(state);
        }
        "1" | "2" if f.len() >= 4 => {
            state.user = plain(f[1]);
            state.code = code(f[3]);
            state.logged_in = !state.code.is_empty();
            state.server_running = f[0] == "2" && state.logged_in;
            state.server_clients.clear();
            if state.server_running {
                let clients: Vec<&str> = field(&f, 5).split('|').collect();
                for pair in clients.chunks_exact(2).take(1024) {
                    if !pair[0].is_empty() {
                        state.server_clients.push(ServerClient { id: plain(pair[0]), addr: plain(pair[1]) });
                    }
                }
                state.error = plain(field(&f, 12));
            } else {
                state.error = plain(field(&f, 10));
            }
            if !state.logged_in {
                state.client_connected = false;
                state.local_ip.clear();
            }
            settle_phase(state);
        }
        "pclist" if f.len() >= 6 => {
            // Metadata is aligned at the end. The connection-password slot is never retained.
            let tail = &f[f.len() - 5..];
            let mut records = &f[1..f.len() - 5];
            if records.len() % 4 == 1 && records.last() == Some(&"") {
                records = &records[..records.len() - 1];
            }
            if records.len() % 4 != 0 {
                return;
            }
            let mut peers = Vec::new();
            for record in records.chunks_exact(4).take(1024) {
                if record[0].is_empty() { continue; }
                let cidr = record[3].strip_prefix("1|").unwrap_or("");
                peers.push(Peer { id: plain(record[0]), name: plain(record[1]), ip: ip(record[2]), cidr: plain(cidr) });
            }
            state.peers = peers;
            state.local_ip = ip(tail[0]);
            // Port-mapping sessions may have no virtual IP. Route/data health is assessed
            // separately; this flag reflects only the core's session state.
            state.client_connected = !tail[3].trim().is_empty();
            if state.client_connected { state.error.clear(); }
            settle_phase(state);
        }
        "conpc" if f.len() >= 2 => {
            if f[1].trim().eq_ignore_ascii_case("yes") {
                state.local_ip = ip(field(&f, 3));
                state.client_connected = true;
                state.error.clear();
                settle_phase(state);
            } else if f[1].trim().eq_ignore_ascii_case("no") {
                // Official versions emit either [no, reason] or [no, id, reason].
                // Preserve the actual rejection instead of turning a three-field
                // firewall/NAT error into an unrelated "service not started" hint.
                let reason = if f.len() >= 4 { field(&f, 3) } else { field(&f, 2) };
                state.error = plain(reason);
                if state.error.is_empty() { state.error = "对方未开启服务".into(); }
            }
        }
        "conerr" | "authorize" if f.len() >= 2 => {
            state.error = plain(&f[1..].join(" "));
        }
        "info" | "tips" if f.len() >= 2 => {
            let text = plain(&f[1..].join(" "));
            let lower = text.to_ascii_lowercase();
            if ["超时", "失败", "错误", "不存在", "密码", "未开启", "断开"].iter().any(|word| text.contains(word))
                || ["timeout", "error", "failed", "invalid", "not exist"].iter().any(|word| lower.contains(word)) {
                state.error = text;
            }
        }
        "discon" => {
            state.client_connected = false;
            state.local_ip.clear();
            if f.len() > 1 { state.error = plain(&f[1..].join(" ")); }
            settle_phase(state);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_never_serializes_password_fields() {
        let mut state = State::default();
        apply_message(&["1", "account", "0", "12345678", "", "SECRET_CONN", "0", "", "phone", "SECRET_WEB", ""].join(SEP), &mut state);
        assert!(state.logged_in);
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("SECRET"));
    }

    #[test]
    fn pclist_tail_and_logout_reset_client() {
        let mut state = State::default();
        apply_message(&["1", "u", "0", "12345678"].join(SEP), &mut state);
        apply_message(&["pclist", "7", "computer", "10.6.22.1", "", "10.6.22.11", "87654321", "SECRET", "1", "7"].join(SEP), &mut state);
        assert_eq!(state.phase, "client");
        assert_eq!(state.peers[0].id, "7");
        assert!(!serde_json::to_string(&state).unwrap().contains("SECRET"));
        apply_message(&["0", "u", "SECRET", "0", "0", "timeout"].join(SEP), &mut state);
        assert!(!state.client_connected);
        assert!(state.local_ip.is_empty());
        assert!(state.peers.is_empty());
        assert_eq!(state.error, "timeout");
    }

    #[test]
    fn malformed_and_empty_list_do_not_panic() {
        let mut state = State::default();
        for message in ["", "0", "1", "2", "pclist", "conpc", "conerr"] { apply_message(message, &mut state); }
        apply_message(&["pclist", "", "", "", "SECRET", "", ""].join(SEP), &mut state);
        assert!(state.peers.is_empty());
        assert!(!state.client_connected);
    }

    #[test]
    fn info_login_timeout_is_an_error() {
        let mut state = State::default();
        apply_message(&["info", "登录错误：连接超时"].join(SEP), &mut state);
        assert_eq!(state.error, "登录错误：连接超时");
        assert!(!state.logged_in);
    }

    #[test]
    fn connection_rejection_retains_both_official_message_formats() {
        let mut state = State::default();
        let blocked = "连接被阻断！请检查网络或防火墙。";
        apply_message(&["conpc", "no", blocked].join(SEP), &mut state);
        assert_eq!(state.error, blocked);
        apply_message(&["conpc", "no", "peer-id", "密码错误"].join(SEP), &mut state);
        assert_eq!(state.error, "密码错误");
        apply_message(&["conpc", "no"].join(SEP), &mut state);
        assert_eq!(state.error, "对方未开启服务");
        assert!(!state.client_connected);
    }
}
