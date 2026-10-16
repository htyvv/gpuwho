//! `--notify URL`: post a message to a chat webhook when GPUs free up. The payload is
//! shaped for the service the URL points at (Slack, Discord, Feishu/Lark, WeCom, DingTalk,
//! ntfy, Telegram bot API), with a generic {"text": ...} otherwise. Sent with curl, so
//! proxies and certificates work the way they already do on that machine.

use serde_json::{json, Value};

pub fn payload(url: &str, text: &str) -> (Option<Value>, String) {
    let u = url.to_ascii_lowercase();
    let body =
        if u.contains("discord.com/api/webhooks") || u.contains("discordapp.com/api/webhooks") {
            json!({"content": text})
        } else if u.contains("open.feishu.cn") || u.contains("open.larksuite.com") {
            json!({"msg_type": "text", "content": {"text": text}})
        } else if u.contains("qyapi.weixin.qq.com") || u.contains("oapi.dingtalk.com") {
            json!({"msgtype": "text", "text": {"content": text}})
        } else if u.contains("ntfy") {
            return (None, text.to_string()); // ntfy takes the message as a plain body
        } else if u.contains("api.telegram.org") {
            return (None, text.to_string()); // sent as the `text` form field below
        } else {
            json!({"text": text}) // Slack, Mattermost, Rocket.Chat, Teams workflows ...
        };
    (Some(body.clone()), body.to_string())
}

pub fn send(url: &str, text: &str) -> Result<(), String> {
    let (json_body, body) = payload(url, text);
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-fsS", "-m", "15", "-o", "/dev/null"]);
    if url.to_ascii_lowercase().contains("api.telegram.org") {
        cmd.args(["--data-urlencode", &format!("text={}", text), url]);
    } else {
        if json_body.is_some() {
            cmd.args(["-H", "Content-Type: application/json"]);
        }
        cmd.args(["--data-binary", &body, url]);
    }
    match cmd.output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(format!("can't run curl: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        let t = "gpu1: GPU 3 is free";
        assert_eq!(
            payload("https://hooks.slack.com/services/X", t).0.unwrap()["text"],
            t
        );
        assert_eq!(
            payload("https://discord.com/api/webhooks/1/x", t)
                .0
                .unwrap()["content"],
            t
        );
        assert_eq!(
            payload("https://open.feishu.cn/open-apis/bot/v2/hook/x", t)
                .0
                .unwrap()["content"]["text"],
            t
        );
        assert_eq!(
            payload("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=x", t)
                .0
                .unwrap()["text"]["content"],
            t
        );
        assert_eq!(payload("https://ntfy.sh/mylab", t), (None, t.to_string()));
    }
}
