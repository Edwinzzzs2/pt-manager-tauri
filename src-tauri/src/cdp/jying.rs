//! 聚影登录和签到共用的公告关闭流程。

use super::{CdpProgress, CdpWebSocket, CDP_CANCELLED};
use std::time::Duration;

pub(super) async fn dismiss_announcement(
    websocket: &mut CdpWebSocket,
    progress: Option<&CdpProgress>,
) -> Result<(), String> {
    let mut clicked = false;
    for _ in 0..30 {
        if progress.is_some_and(CdpProgress::is_cancelled) {
            return Err(CDP_CANCELLED.to_string());
        }
        // Vue 关闭弹窗后还会播放退出动画，等遮罩消失再填写或点击底层表单。
        let expression = DISMISS_ANNOUNCEMENT_EXPRESSION.replace("__CLICK__", if clicked { "false" } else { "true" });
        let response = websocket.call("Runtime.evaluate", serde_json::json!({
            "expression": expression,
            "returnByValue": true,
            "userGesture": true
        }))?;
        let state = response.get("result")
            .and_then(|result| result.get("result"))
            .and_then(|result| result.get("value"))
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "无法读取聚影公告状态".to_string())?;
        match state {
            0 => {
                if clicked {
                    if let Some(p) = progress {
                        p.info("聚影已关闭站点公告".to_string()).await;
                    }
                }
                return Ok(());
            }
            1 => clicked = true,
            2 => {},
            3 => return Err("聚影公告未找到可用的关闭按钮，请在浏览器中关闭公告".to_string()),
            _ => return Err("聚影页面已离开本站，停止关闭公告".to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("等待聚影公告遮罩消失超时，请在浏览器中检查弹窗".to_string())
}

const DISMISS_ANNOUNCEMENT_EXPRESSION: &str = r#"(() => {
    if (!['jying.top', 'www.jying.top'].includes(location.hostname.toLowerCase())) return 4;
    const dialog = document.querySelector('.announcement-dialog');
    if (!dialog) return 0;
    const rect = dialog.getBoundingClientRect();
    const style = getComputedStyle(dialog);
    if (style.display === 'none' || style.visibility === 'hidden' || !rect.width || !rect.height) return 0;
    if (!__CLICK__) return 2;
    // 只点击公告的关闭按钮，不触发查看通知或长期免打扰等其他操作。
    const close = dialog.querySelector('button[aria-label="关闭公告"]');
    if (!close || close.disabled) return 3;
    close.click();
    return 1;
})()"#;
