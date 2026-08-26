use super::{CdpClient, CdpWebSocket};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct SiteTraffic {
    pub upload: Option<String>,
    pub download: Option<String>,
    pub ratio: Option<String>,
}

impl SiteTraffic {
    pub fn available(&self) -> bool {
        self.upload.is_some() || self.download.is_some() || self.ratio.is_some()
    }

    pub fn summary(&self) -> String {
        let upload = self.upload.as_deref().unwrap_or("未识别");
        let download = self.download.as_deref().unwrap_or("未识别");
        let ratio = self.ratio.as_deref().unwrap_or("未识别");
        format!("上传 {upload}，下载 {download}，分享率 {ratio}")
    }
}

#[derive(Debug, Deserialize)]
struct TrafficPageState {
    upload: Option<String>,
    download: Option<String>,
    ratio: Option<String>,
}

impl CdpClient {
    pub async fn read_site_traffic(&self, tab_id: &str) -> Result<SiteTraffic, String> {
        let Some(websocket_url) = self.websocket_url_for_tab(tab_id)? else {
            return Err("未找到站点标签页".to_string());
        };
        let mut websocket = CdpWebSocket::connect(&websocket_url, Duration::from_secs(10))?;
        let response = websocket.call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": TRAFFIC_EXPRESSION,
                "returnByValue": true,
                "awaitPromise": true
            }),
        )?;
        let value = response
            .get("result")
            .and_then(|value| value.get("result"))
            .and_then(|value| value.get("value"))
            .cloned()
            .ok_or_else(|| "读取站点流量信息失败".to_string())?;
        let state: TrafficPageState =
            serde_json::from_value(value).map_err(|err| format!("解析站点流量信息失败：{err}"))?;
        Ok(SiteTraffic {
            upload: state.upload,
            download: state.download,
            ratio: state.ratio,
        })
    }
}

const TRAFFIC_EXPRESSION: &str = r#"(() => {
    const clean = (value) => String(value || '').replace(/\s+/g, ' ').trim();
    const bodyText = clean(document.body?.innerText || '');
    // 云服务商页面把累计出入流量放在这两个固定节点中；它们不是 PT 分享率。
    const isProviderTraffic = Boolean(document.querySelector('#trafficout, #trafficin'));
    const sizeSource = '([\\d,.]+\\s*(?:[KMGTPE]i?B))';
    const uploadLabel = new RegExp('(?:^|\\s)(?:(?:上传|上傳)(?:量)?|上行(?:流量)?|upload(?:ed)?)\\s*[:：]?\\s*' + sizeSource, 'i');
    const downloadLabel = new RegExp('(?:^|\\s)(?:(?:下载|下載)(?:量)?|下行(?:流量)?|download(?:ed)?)\\s*[:：]?\\s*' + sizeSource, 'i');
    const uploadArrow = new RegExp('[↑⬆]\\s*' + sizeSource, 'i');
    const downloadArrow = new RegExp('[↓⬇]\\s*' + sizeSource, 'i');
    const ratioLabel = /(?:分享率|分享比率|传输比率|傳輸比率|ratio)\s*[:：]?\s*(∞|inf(?:inity)?|---|[\d,.]+)/i;
    const markerValue = (selector, pattern) => {
        for (const marker of document.querySelectorAll(selector)) {
            let localText = clean(marker.textContent);
            let value = localText.match(pattern)?.[1];
            if (value) return value;
            let sibling = marker.nextSibling;
            for (let index = 0; sibling && index < 3; index += 1, sibling = sibling.nextSibling) {
                if (sibling.nodeType === 1 && sibling.matches?.(selector)) break;
                localText = clean(`${localText} ${sibling.textContent || ''}`);
                value = localText.match(pattern)?.[1];
                if (value) return value;
            }
        }
        return null;
    };
    // NexusPHP 主题常把账号数据放在默认隐藏的控制面板中。优先读取语义 class，
    // 避免把首页“站点数据”里的总上传量、总下载量误当成当前账号数据。
    const accountUpload = markerValue(
        '#trafficout, .color_uploaded, .top-nav__stats-up, .ratio-bar__uploaded, [title="上传量"], [title="上傳量"]',
        new RegExp(sizeSource, 'i')
    );
    const accountDownload = markerValue(
        '#trafficin, .color_downloaded, .top-nav__stats-down, .ratio-bar__downloaded, [title="下载量"], [title="下載量"]',
        new RegExp(sizeSource, 'i')
    );
    const accountRatio = markerValue(
        '.color_ratio, .top-nav__stats-ratio, .ratio-bar__ratio, [title="分享率"], [title="分享比率"]',
        /(∞|inf(?:inity)?|---|[\d,.]+)/i
    );
    const directUpload = bodyText.match(uploadLabel)?.[1] || bodyText.match(uploadArrow)?.[1] || null;
    const directDownload = bodyText.match(downloadLabel)?.[1] || bodyText.match(downloadArrow)?.[1] || null;
    const directRatio = bodyText.match(ratioLabel)?.[1] || null;
    const exactSize = /^[\d,.]+\s*(?:[KMGTPE]i?B)$/i;
    const visible = (element) => {
        if (!element) return false;
        const style = getComputedStyle(element);
        return style.display !== 'none' && style.visibility !== 'hidden';
    };
    const attributes = (element) => {
        if (!element) return '';
        return clean([
            element.id,
            element.className,
            element.getAttribute?.('title'),
            element.getAttribute?.('aria-label'),
            element.getAttribute?.('alt'),
            element.getAttribute?.('href')
        ].filter(Boolean).join(' '));
    };
    const marker = (scope) => {
        if (!scope) return '';
        const descendants = Array.from(scope.querySelectorAll?.('i, img, svg, use') || [])
            .slice(0, 8)
            .map(attributes)
            .join(' ');
        return clean([attributes(scope), descendants].join(' ')).toLowerCase();
    };
    const uploadHint = /(upload|uploaded|traffic.?out|arrow.?up|fa.?up|icon.?up|上传|上傳|上行)/i;
    const downloadHint = /(download|downloaded|traffic.?in|arrow.?down|fa.?down|icon.?down|下载|下載|下行)/i;
    const ratioHint = /(share.?ratio|ratio|icon.?chart|fa.?chart|分享率|分享比率|传输比率|傳輸比率)/i;
    const candidates = [];
    for (const element of document.querySelectorAll('span, strong, b, div, td, a')) {
        if (!visible(element)) continue;
        const text = clean(element.textContent);
        const match = text.match(exactSize);
        if (!match) continue;
        let uploadScore = 0;
        let downloadScore = 0;
        const scopes = [element, element.parentElement, element.previousElementSibling];
        scopes.forEach((scope, index) => {
            const hint = marker(scope);
            const score = 12 - index * 3;
            if (uploadHint.test(hint)) uploadScore = Math.max(uploadScore, score);
            if (downloadHint.test(hint)) downloadScore = Math.max(downloadScore, score);
        });
        candidates.push({ value: match[0], uploadScore, downloadScore });
    }
    const best = (key) => candidates
        .filter((candidate) => candidate[key] > 0)
        .sort((left, right) => right[key] - left[key])[0]?.value || null;
    const exactRatio = /^(?:∞|inf(?:inity)?|---|[\d,.]+)$/i;
    const ratioCandidates = [];
    for (const element of document.querySelectorAll('span, strong, b, div, td, a')) {
        if (!visible(element)) continue;
        const text = clean(element.textContent);
        if (!exactRatio.test(text)) continue;
        let score = 0;
        [element, element.parentElement, element.previousElementSibling].forEach((scope, index) => {
            if (ratioHint.test(marker(scope))) score = Math.max(score, 12 - index * 3);
        });
        if (score > 0) ratioCandidates.push({ value: text, score });
    }
    const upload = clean(accountUpload || directUpload || best('uploadScore')) || null;
    const download = clean(accountDownload || directDownload || best('downloadScore')) || null;
    const calculatedRatio = (() => {
        const parseSize = (value) => {
            const match = clean(value).match(/^([\d,.]+)\s*([KMGTPE])i?B$/i);
            if (!match) return null;
            const number = Number(match[1].replace(/,/g, ''));
            const exponent = 'KMGTPE'.indexOf(match[2].toUpperCase()) + 1;
            return Number.isFinite(number) ? number * (1024 ** exponent) : null;
        };
        const uploaded = parseSize(upload);
        const downloaded = parseSize(download);
        if (uploaded === null || downloaded === null) return null;
        if (downloaded === 0) return uploaded > 0 ? '∞' : '0';
        return (uploaded / downloaded).toFixed(3).replace(/\.?0+$/, '');
    })();
    const detectedRatio = clean(
        accountRatio
        || directRatio
        || ratioCandidates.sort((left, right) => right.score - left.score)[0]?.value
    ) || null;
    const detectedRatioNumber = Number(clean(detectedRatio).replace(/,/g, ''));
    const fallbackRatio = isProviderTraffic ? null : calculatedRatio;
    const ratio = detectedRatioNumber === 0 && fallbackRatio && fallbackRatio !== '0'
        ? fallbackRatio
        : detectedRatio || fallbackRatio;
    return {
        upload,
        download,
        ratio: clean(ratio) || null
    };
})()"#;
