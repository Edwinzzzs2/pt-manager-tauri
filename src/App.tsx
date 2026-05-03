import { useEffect, useMemo, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { ask, message } from "@tauri-apps/plugin-dialog";
import { relaunch } from "@tauri-apps/plugin-process";
import { check } from "@tauri-apps/plugin-updater";
import {
  Activity,
  CheckCircle2,
  Clock3,
  Download,
  ListChecks,
  PauseCircle,
  Play,
  Plus,
  RefreshCw,
  Save,
  Search,
  Settings,
  ShieldCheck,
  Square,
  Trash2,
  XCircle,
  type LucideIcon,
} from "lucide-react";
import "./App.css";

type Site = {
  id: string;
  name: string;
  url: string;
};

type AppConfig = {
  sites: Site[];
  cron: string;
  cdp_port: number;
  visit_duration: number;
  random_delay: boolean;
  auto_launch: boolean;
  log_retention: number;
  auto_sync_cookie: boolean;
  cookiecloud: CookieCloudConfig;
};

type CookieCloudConfig = {
  server_url: string;
  uuid: string;
  password: string;
};

type LogEntry = {
  timestamp: string;
  level: "INFO" | "SUCCESS" | "ERROR";
  message: string;
};

type AppStatus = {
  cdp_connected: boolean;
  chrome_installed: boolean;
  active_cdp_port: number | null;
  next_run: string | null;
  last_result: LogEntry | null;
  is_running: boolean;
  cancel_requested: boolean;
};

type CookieCloudSyncResult = {
  matched_cookies: number;
  imported_cookies: number;
};

type TabKey = "dashboard" | "sites" | "settings" | "logs";

const defaultConfig: AppConfig = {
  sites: [],
  cron: "0 9 * * *",
  cdp_port: 9222,
  visit_duration: 30,
  random_delay: true,
  auto_launch: false,
  log_retention: 500,
  auto_sync_cookie: false,
  cookiecloud: {
    server_url: "",
    uuid: "",
    password: "",
  },
};

const navItems: Array<{ key: TabKey; label: string; icon: LucideIcon }> = [
  { key: "dashboard", label: "总览", icon: Activity },
  { key: "sites", label: "站点", icon: ListChecks },
  { key: "settings", label: "设置", icon: Settings },
  { key: "logs", label: "日志", icon: Clock3 },
];

function App() {
  const [activeTab, setActiveTab] = useState<TabKey>("dashboard");
  const [config, setConfig] = useState<AppConfig>(defaultConfig);
  const [settingsDraft, setSettingsDraft] = useState<AppConfig>(defaultConfig);
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [newSite, setNewSite] = useState({ name: "", url: "" });
  const [editingSiteId, setEditingSiteId] = useState<string | null>(null);
  const [editingSite, setEditingSite] = useState({ name: "", url: "" });
  const [busy, setBusy] = useState(false);
  const [cdpBusy, setCdpBusy] = useState(false);
  const [cookieSyncBusy, setCookieSyncBusy] = useState(false);
  const [cancelBusy, setCancelBusy] = useState(false);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [appVersion, setAppVersion] = useState("");
  const [error, setError] = useState<string | null>(null);

  const lastVisibleLog = useMemo(() => logs[logs.length - 1], [logs]);

  async function refreshConfig() {
    const next = await invoke<AppConfig>("get_config");
    setConfig(next);
    setSettingsDraft(next);
  }

  async function refreshStatus() {
    const next = await invoke<AppStatus>("get_status");
    setStatus(next);
  }

  async function refreshLogs() {
    const next = await invoke<LogEntry[]>("get_logs");
    setLogs(next);
  }

  useEffect(() => {
    refreshConfig().catch(showError);
    refreshStatus().catch(showError);
    refreshLogs().catch(showError);
    getVersion()
      .then((version) => setAppVersion(version))
      .catch(showError);

    const timer = window.setInterval(() => {
      refreshStatus().catch(showError);
      refreshLogs().catch(showError);
    }, 1000);

    return () => window.clearInterval(timer);
  }, []);

  function showError(err: unknown) {
    setError(err instanceof Error ? err.message : String(err));
  }

  async function runNow() {
    setBusy(true);
    setError(null);
    try {
      await invoke("run_task");
      await refreshStatus();
      await refreshLogs();
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  async function stopTask() {
    setCancelBusy(true);
    setError(null);
    try {
      await invoke("stop_task");
      await refreshStatus();
      await refreshLogs();
    } catch (err) {
      showError(err);
    } finally {
      setCancelBusy(false);
    }
  }

  async function ensureCdp() {
    setCdpBusy(true);
    setError(null);
    try {
      await invoke("ensure_cdp");
      await refreshStatus();
      await refreshLogs();
    } catch (err) {
      showError(err);
    } finally {
      setCdpBusy(false);
    }
  }

  async function openChromeDownload() {
    setError(null);
    try {
      await invoke("open_chrome_download");
    } catch (err) {
      showError(err);
    }
  }

  async function syncCookieCloud() {
    setCookieSyncBusy(true);
    setError(null);
    try {
      await invoke<CookieCloudSyncResult>("sync_cookiecloud_from_config", {
        config: settingsDraft,
      });
      await refreshStatus();
      await refreshLogs();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      if (message.includes("仅支持 http://")) {
        try {
          const cookieData = await fetchCookieCloudData(settingsDraft.cookiecloud);
          await invoke<CookieCloudSyncResult>("sync_cookiecloud_cookies", {
            cookies: cookieData,
          });
          await refreshStatus();
          await refreshLogs();
          return;
        } catch (fallbackErr) {
          showError(fallbackErr);
        }
      } else {
        showError(err);
      }
    } finally {
      setCookieSyncBusy(false);
    }
  }

  async function addSite() {
    const name = newSite.name.trim();
    const url = newSite.url.trim();
    if (!name || !url) {
      setError("站点名称和 URL 不能为空");
      return;
    }

    setBusy(true);
    setError(null);
    try {
      const next = await invoke<AppConfig>("add_site", { name, url });
      setConfig(next);
      setSettingsDraft(next);
      setNewSite({ name: "", url: "" });
      await refreshStatus();
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  function startEdit(site: Site) {
    setEditingSiteId(site.id);
    setEditingSite({ name: site.name, url: site.url });
  }

  async function saveSite(id: string) {
    const name = editingSite.name.trim();
    const url = editingSite.url.trim();
    if (!name || !url) {
      setError("站点名称和 URL 不能为空");
      return;
    }

    setBusy(true);
    setError(null);
    try {
      const next = await invoke<AppConfig>("update_site", { id, name, url });
      setConfig(next);
      setSettingsDraft(next);
      setEditingSiteId(null);
      await refreshStatus();
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  async function removeSite(id: string) {
    setBusy(true);
    setError(null);
    try {
      const next = await invoke<AppConfig>("remove_site", { id });
      setConfig(next);
      setSettingsDraft(next);
      await refreshStatus();
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  async function saveSettings() {
    const next: AppConfig = {
      ...settingsDraft,
      cdp_port: Number(settingsDraft.cdp_port) || 9222,
      visit_duration: Math.max(5, Number(settingsDraft.visit_duration) || 30),
      log_retention: clampNumber(Number(settingsDraft.log_retention) || 500, 50, 5000),
      cron: settingsDraft.cron.trim() || defaultConfig.cron,
    };

    setBusy(true);
    setError(null);
    try {
      await invoke("save_config", { config: next });
      setConfig(next);
      setSettingsDraft(next);
      await refreshStatus();
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  async function clearLogs() {
    setError(null);
    try {
      await invoke("clear_logs");
      setLogs([]);
    } catch (err) {
      showError(err);
    }
  }

  async function checkForUpdates() {
    setUpdateBusy(true);
    setError(null);
    try {
      const update = await check();
      if (!update) {
        await message("当前已是最新版本", {
          kind: "info",
          title: "检查更新",
        });
        return;
      }

      const shouldInstall = await ask(
        `发现新版本 ${formatVersion(update.version)}，是否现在下载并安装？`,
        {
          kind: "info",
          okLabel: "立即更新",
          cancelLabel: "稍后",
          title: "发现新版本",
        },
      );
      if (!shouldInstall) {
        return;
      }

      await update.downloadAndInstall();
      await relaunch();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setError(`检查更新失败：${message}`);
    } finally {
      setUpdateBusy(false);
    }
  }

  return (
    <main className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <ShieldCheck size={24} />
          <div>
            <strong>PT Manager</strong>
            <span>CDP Keepalive</span>
          </div>
        </div>

        <nav className="nav-list">
          {navItems.map((item) => {
            const Icon = item.icon;
            return (
              <button
                key={item.key}
                className={activeTab === item.key ? "nav-item active" : "nav-item"}
                onClick={() => setActiveTab(item.key)}
                type="button"
              >
                <Icon size={18} />
                <span>{item.label}</span>
              </button>
            );
          })}
        </nav>

        <div className="sidebar-status">
          <span className={status?.cdp_connected ? "dot ok" : "dot standby"} />
          <div>
            <strong>
              {status?.cdp_connected
                ? "Chrome 已连接"
                : status?.chrome_installed === false
                  ? "需要安装 Chrome"
                  : "自动模式待命"}
            </strong>
            <span>
              {status?.cdp_connected
                ? `localhost:${status.active_cdp_port ?? config.cdp_port}`
                : status?.chrome_installed === false
                  ? "安装后自动接管"
              : "运行时自动准备"}
            </span>
          </div>
        </div>

        <div className="version-card">
          <div>
            <span>当前版本</span>
            <strong>{appVersion ? formatVersion(appVersion) : "读取中"}</strong>
          </div>
          <button disabled={updateBusy} onClick={checkForUpdates} type="button">
            <RefreshCw size={14} />
            <span>{updateBusy ? "检查中" : "检查更新"}</span>
          </button>
        </div>
      </aside>

      <section className="workspace">
        <header className="topbar">
          <div>
            <p className="eyebrow">MVP 控制台</p>
            <h1>{pageTitle(activeTab)}</h1>
          </div>
          <div className="topbar-actions">
            <button
              className="primary-action"
              disabled={busy || status?.is_running}
              onClick={runNow}
              type="button"
            >
              {busy || status?.is_running ? <RefreshCw size={18} /> : <Play size={18} />}
              <span>{busy || status?.is_running ? "执行中" : "立即保活"}</span>
            </button>
            {status?.is_running ? (
              <button
                className="danger-action"
                disabled={cancelBusy || status.cancel_requested}
                onClick={stopTask}
                type="button"
              >
                {cancelBusy || status.cancel_requested ? <RefreshCw size={18} /> : <Square size={16} />}
                <span>{cancelBusy || status.cancel_requested ? "终止中" : "终止"}</span>
              </button>
            ) : null}
          </div>
        </header>

        {error ? (
          <div className="error-banner">
            <XCircle size={18} />
            <span>{error}</span>
            <button onClick={() => setError(null)} type="button">
              关闭
            </button>
          </div>
        ) : null}

        {activeTab === "dashboard" ? (
          <Dashboard
            cdpBusy={cdpBusy}
            config={config}
            lastLog={lastVisibleLog}
            onEnsureCdp={ensureCdp}
            onOpenChromeDownload={openChromeDownload}
            recentLogs={logs.slice(-5).reverse()}
            status={status}
            onRefresh={() => {
              refreshStatus().catch(showError);
              refreshLogs().catch(showError);
            }}
          />
        ) : null}

        {activeTab === "sites" ? (
          <SitesPanel
            busy={busy}
            config={config}
            editingSite={editingSite}
            editingSiteId={editingSiteId}
            newSite={newSite}
            onAdd={addSite}
            onCancelEdit={() => setEditingSiteId(null)}
            onEditChange={setEditingSite}
            onNewSiteChange={setNewSite}
            onRemove={removeSite}
            onSave={saveSite}
            onStartEdit={startEdit}
          />
        ) : null}

        {activeTab === "settings" ? (
          <SettingsPanel
            busy={busy}
            cookieSyncBusy={cookieSyncBusy}
            draft={settingsDraft}
            onChange={setSettingsDraft}
            onSave={saveSettings}
            onSyncCookieCloud={syncCookieCloud}
            taskRunning={!!status?.is_running}
          />
        ) : null}

        {activeTab === "logs" ? (
          <LogsPanel logs={logs} onClear={clearLogs} onRefresh={refreshLogs} />
        ) : null}
      </section>
    </main>
  );
}

async function fetchCookieCloudData(config: CookieCloudConfig) {
  const serverUrl = config.server_url.trim();
  const uuid = config.uuid.trim();
  const password = config.password;
  if (!serverUrl || !uuid || !password) {
    throw new Error("请先填写 CookieCloud 地址、UUID 和密码");
  }

  const endpoints = buildCookieCloudEndpoints(serverUrl, uuid);
  const errors: string[] = [];

  for (const endpoint of endpoints) {
    try {
      const payload = await requestCookieCloudPayload(endpoint, password);
      if (payload?.cookie_data) {
        return payload.cookie_data;
      }
      if (payload?.encrypted) {
        throw new Error("CookieCloud 服务端返回了密文，请确认服务端支持 password 解密接口");
      }
      throw new Error("CookieCloud 返回数据缺少 cookie_data");
    } catch (err) {
      errors.push(`${endpoint}: ${readableError(err)}`);
    }
  }

  throw new Error(
    [
      "CookieCloud 无法连接，请确认服务地址和协议是否与浏览器插件一致。",
      "常见格式：https://ccc.ft07.com 或 http://127.0.0.1:8088",
      errors[0] ? `最近一次错误：${errors[0]}` : "",
    ]
      .filter(Boolean)
      .join("\n"),
  );
}

async function requestCookieCloudPayload(endpoint: string, password: string) {
  let response = await fetch(endpoint, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ password }),
  });
  if (!response.ok) {
    response = await fetch(`${endpoint}?password=${encodeURIComponent(password)}`);
  }
  if (!response.ok) {
    throw new Error(`HTTP ${response.status}`);
  }

  const text = await response.text();
  return parseCookieCloudPayload(text);
}

function buildCookieCloudEndpoints(serverUrl: string, uuid: string) {
  const encodedUuid = encodeURIComponent(uuid);
  const bases = buildCookieCloudBaseCandidates(serverUrl);
  return bases.map((base) => {
    if (/\/get\/[^/]+\/?$/i.test(base)) {
      return base.replace(/\/+$/, "");
    }
    if (/\/get\/?$/i.test(base)) {
      return `${base.replace(/\/+$/, "")}/${encodedUuid}`;
    }
    return `${base.replace(/\/+$/, "")}/get/${encodedUuid}`;
  });
}

function buildCookieCloudBaseCandidates(input: string) {
  const cleaned = input.trim().replace(/\/+$/, "");
  const candidates: string[] = [];

  // CookieCloud 插件常允许直接填域名；这里按公网优先 https、本机优先保留原协议来补齐。
  const push = (value: string) => {
    if (!candidates.includes(value)) {
      candidates.push(value);
    }
  };

  if (!/^https?:\/\//i.test(cleaned)) {
    push(`https://${cleaned}`);
    push(`http://${cleaned}`);
    return candidates;
  }

  push(cleaned);
  try {
    const url = new URL(cleaned);
    if (url.protocol === "http:" && !isLocalCookieCloudHost(url.hostname)) {
      url.protocol = "https:";
      push(url.toString().replace(/\/+$/, ""));
    }
  } catch {
    // URL 已经在 fetch 阶段报错，这里只负责生成候选地址。
  }

  return candidates;
}

function isLocalCookieCloudHost(hostname: string) {
  return ["localhost", "127.0.0.1", "::1"].includes(hostname);
}

function readableError(err: unknown) {
  const message = err instanceof Error ? err.message : String(err);
  if (message.includes("10061") || message.includes("积极拒绝")) {
    return "目标地址没有 CookieCloud 服务在监听";
  }
  return message;
}

function parseCookieCloudPayload(text: string): Record<string, unknown> | null {
  const parsed = JSON.parse(text);
  return typeof parsed === "string" ? JSON.parse(parsed) : parsed;
}

function Dashboard({
  cdpBusy,
  config,
  lastLog,
  onEnsureCdp,
  onOpenChromeDownload,
  recentLogs,
  status,
  onRefresh,
}: {
  cdpBusy: boolean;
  config: AppConfig;
  lastLog?: LogEntry;
  onEnsureCdp: () => void;
  onOpenChromeDownload: () => void;
  recentLogs: LogEntry[];
  status: AppStatus | null;
  onRefresh: () => void;
}) {
  const chromeInstalled = status?.chrome_installed !== false;
  const chromeConnected = !!status?.cdp_connected;

  return (
    <div className="dashboard">
      <section className="metric-grid">
        <MetricCard
          icon={chromeConnected ? CheckCircle2 : chromeInstalled ? PauseCircle : Download}
          label="Chrome 环境"
          tone={chromeConnected ? "ok" : chromeInstalled ? "muted" : "warning"}
          value={chromeConnected ? "已就绪" : chromeInstalled ? "自动模式" : "需安装"}
        />
        <MetricCard icon={ListChecks} label="站点数量" value={`${config.sites.length}`} />
        <MetricCard
          icon={Clock3}
          label="下一轮任务"
          value={formatDate(status?.next_run)}
        />
        <MetricCard
          icon={status?.is_running ? RefreshCw : PauseCircle}
          label="执行状态"
          tone={status?.is_running ? "warning" : "muted"}
          value={status?.is_running ? "运行中" : "待机"}
        />
      </section>

      <section className="dashboard-grid">
        <div className="panel setup-panel">
          <div className="panel-heading">
            <div>
              <p className="eyebrow">Chrome</p>
              <h2>自动模式</h2>
            </div>
            <div className="row-actions">
              {!chromeInstalled ? (
                <button
                  className="ghost install-action"
                  onClick={onOpenChromeDownload}
                  type="button"
                >
                  <Download size={16} />
                  <span>安装 Chrome</span>
                </button>
              ) : !chromeConnected ? (
                <button
                  className="ghost"
                  disabled={cdpBusy || status?.is_running}
                  onClick={onEnsureCdp}
                  type="button"
                >
                  {cdpBusy ? <RefreshCw size={16} /> : <Play size={16} />}
                  <span>{cdpBusy ? "准备中" : "预先准备"}</span>
                </button>
              ) : null}
              <button className="icon-button" onClick={onRefresh} title="刷新状态" type="button">
                <RefreshCw size={17} />
              </button>
            </div>
          </div>
          <code className="command-line">
            {chromeConnected
              ? `CDP 已连接：localhost:${status?.active_cdp_port ?? config.cdp_port}`
              : chromeInstalled
                ? "自动模式待命：执行保活时会自动启动专用 Chrome"
                : "未检测到 Chrome：安装完成后即可自动启动专用浏览器"}
          </code>
          <div className="setup-steps">
            <span>1. 默认自动模式，保活时自动启动专用 Chrome Profile</span>
            <span>2. 首次打开后登录站点，后续会复用同一个专用浏览器环境</span>
            <span>3. 未安装 Chrome 时先安装，安装完成后点刷新或立即保活</span>
          </div>
        </div>

        <div className="panel">
          <div className="panel-heading">
            <div>
              <p className="eyebrow">最近结果</p>
              <h2>{status?.last_result?.level ?? "暂无任务"}</h2>
            </div>
            <span className={status?.last_result ? levelClass(status.last_result.level) : "badge"}>
              {status?.last_result ? formatTime(status.last_result.timestamp) : "待执行"}
            </span>
          </div>
          <p className="result-text">
            {status?.last_result?.message ?? lastLog?.message ?? "添加站点后即可开始，Chrome 会在运行时自动准备。"}
          </p>
        </div>
      </section>

      <section className="panel live-log-panel">
        <div className="panel-heading">
          <div>
            <p className="eyebrow">Live</p>
            <h2>实时日志</h2>
          </div>
          <button className="icon-button" onClick={onRefresh} title="刷新日志" type="button">
            <RefreshCw size={17} />
          </button>
        </div>
        <div className="compact-log-list">
          {recentLogs.length === 0 ? (
            <div className="empty-state">暂无日志</div>
          ) : (
            recentLogs.map((entry, index) => (
              <div
                className="compact-log-row"
                key={`${entry.timestamp}-${index}`}
                title={entry.message}
              >
                <span className={levelClass(entry.level)}>{entry.level}</span>
                <time>{formatLogTime(entry.timestamp)}</time>
                <p>{entry.message}</p>
              </div>
            ))
          )}
        </div>
      </section>
    </div>
  );
}

function MetricCard({
  icon: Icon,
  label,
  tone = "muted",
  value,
}: {
  icon: LucideIcon;
  label: string;
  tone?: "ok" | "danger" | "warning" | "muted";
  value: string;
}) {
  return (
    <div className={`metric-card ${tone}`}>
      <Icon size={22} />
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

function SitesPanel({
  busy,
  config,
  editingSite,
  editingSiteId,
  newSite,
  onAdd,
  onCancelEdit,
  onEditChange,
  onNewSiteChange,
  onRemove,
  onSave,
  onStartEdit,
}: {
  busy: boolean;
  config: AppConfig;
  editingSite: { name: string; url: string };
  editingSiteId: string | null;
  newSite: { name: string; url: string };
  onAdd: () => void;
  onCancelEdit: () => void;
  onEditChange: (site: { name: string; url: string }) => void;
  onNewSiteChange: (site: { name: string; url: string }) => void;
  onRemove: (id: string) => void;
  onSave: (id: string) => void;
  onStartEdit: (site: Site) => void;
}) {
  return (
    <div className="content-stack">
      <section className="panel site-form">
        <input
          onChange={(event) => onNewSiteChange({ ...newSite, name: event.target.value })}
          placeholder="站点名称"
          value={newSite.name}
        />
        <input
          onChange={(event) => onNewSiteChange({ ...newSite, url: event.target.value })}
          placeholder="https://example.com"
          value={newSite.url}
        />
        <button disabled={busy} onClick={onAdd} type="button">
          <Plus size={17} />
          <span>新增</span>
        </button>
      </section>

      <section className="site-list">
        {config.sites.length === 0 ? (
          <div className="empty-state">暂无站点</div>
        ) : (
          config.sites.map((site) => {
            const editing = editingSiteId === site.id;
            return (
              <article className="site-row" key={site.id}>
                {editing ? (
                  <>
                    <input
                      onChange={(event) =>
                        onEditChange({ ...editingSite, name: event.target.value })
                      }
                      value={editingSite.name}
                    />
                    <input
                      onChange={(event) =>
                        onEditChange({ ...editingSite, url: event.target.value })
                      }
                      value={editingSite.url}
                    />
                  </>
                ) : (
                  <div className="site-main">
                    <strong>{site.name}</strong>
                    <span>{site.url}</span>
                  </div>
                )}
                <div className="row-actions">
                  {editing ? (
                    <>
                      <button onClick={() => onSave(site.id)} type="button">
                        <Save size={16} />
                        <span>保存</span>
                      </button>
                      <button className="ghost" onClick={onCancelEdit} type="button">
                        取消
                      </button>
                    </>
                  ) : (
                    <>
                      <button onClick={() => onStartEdit(site)} type="button">
                        编辑
                      </button>
                      <button className="danger-button" onClick={() => onRemove(site.id)} type="button">
                        <Trash2 size={16} />
                      </button>
                    </>
                  )}
                </div>
              </article>
            );
          })
        )}
      </section>
    </div>
  );
}

function SettingsPanel({
  busy,
  cookieSyncBusy,
  draft,
  onChange,
  onSave,
  onSyncCookieCloud,
  taskRunning,
}: {
  busy: boolean;
  cookieSyncBusy: boolean;
  draft: AppConfig;
  onChange: (config: AppConfig) => void;
  onSave: () => void;
  onSyncCookieCloud: () => void;
  taskRunning: boolean;
}) {
  return (
    <div className="settings-stack">
      <section className="panel settings-card">
        <div className="panel-heading">
          <div>
            <p className="eyebrow">Keepalive</p>
            <h2>任务设置</h2>
          </div>
        </div>

        <div className="settings-form">
          <label>
            <span>Cron 表达式</span>
            <input
              onChange={(event) => onChange({ ...draft, cron: event.target.value })}
              value={draft.cron}
            />
          </label>
          <label>
            <span>页面停留秒数</span>
            <input
              min={5}
              onChange={(event) =>
                onChange({ ...draft, visit_duration: Number(event.target.value) })
              }
              type="number"
              value={draft.visit_duration}
            />
          </label>
          <label>
            <span>日志保留条数</span>
            <input
              max={5000}
              min={50}
              onChange={(event) =>
                onChange({ ...draft, log_retention: Number(event.target.value) })
              }
              type="number"
              value={draft.log_retention}
            />
          </label>
          <label className="switch-row">
            <span>随机延迟</span>
            <input
              checked={draft.random_delay}
              onChange={(event) => onChange({ ...draft, random_delay: event.target.checked })}
              type="checkbox"
            />
          </label>
          <label className="switch-row">
            <span>开机自启</span>
            <input
              checked={draft.auto_launch}
              onChange={(event) => onChange({ ...draft, auto_launch: event.target.checked })}
              type="checkbox"
            />
          </label>
        </div>

        <details className="advanced-settings">
          <summary>手动端口（可选）</summary>
          <label>
            <span>Chrome 调试端口</span>
            <input
              min={1}
              onChange={(event) => onChange({ ...draft, cdp_port: Number(event.target.value) })}
              type="number"
              value={draft.cdp_port}
            />
          </label>
        </details>

        <div className="settings-actions">
          <button className="primary-action settings-save" disabled={busy} onClick={onSave} type="button">
            <Save size={18} />
            <span>保存设置</span>
          </button>
        </div>
      </section>

      <section className="panel settings-card cookiecloud-panel">
        <div className="panel-heading">
          <div>
            <p className="eyebrow">CookieCloud</p>
            <h2>Cookie 同步</h2>
          </div>
          <div className="row-actions">
            <button
              className="ghost"
              disabled={cookieSyncBusy || taskRunning}
              onClick={onSyncCookieCloud}
              type="button"
            >
              <RefreshCw size={16} />
              <span>{taskRunning ? "任务执行中" : cookieSyncBusy ? "同步中" : "同步 Cookie"}</span>
            </button>
          </div>
        </div>
        <div className="settings-form cookiecloud-grid">
          <label>
            <span>服务地址</span>
            <input
              onChange={(event) =>
                onChange({
                  ...draft,
                  cookiecloud: { ...draft.cookiecloud, server_url: event.target.value },
                })
              }
              placeholder="https://ccc.ft07.com"
              value={draft.cookiecloud.server_url}
            />
          </label>
          <label>
            <span>UUID</span>
            <input
              onChange={(event) =>
                onChange({
                  ...draft,
                  cookiecloud: { ...draft.cookiecloud, uuid: event.target.value },
                })
              }
              value={draft.cookiecloud.uuid}
            />
          </label>
          <label>
            <span>密码</span>
            <input
              onChange={(event) =>
                onChange({
                  ...draft,
                  cookiecloud: { ...draft.cookiecloud, password: event.target.value },
                })
              }
              type="password"
              value={draft.cookiecloud.password}
            />
          </label>
          <label className="switch-row">
            <span>保活前自动同步</span>
            <input
              checked={draft.auto_sync_cookie}
              onChange={(event) => onChange({ ...draft, auto_sync_cookie: event.target.checked })}
              type="checkbox"
            />
          </label>
        </div>
      </section>
    </div>
  );
}

function LogsPanel({
  logs,
  onClear,
  onRefresh,
}: {
  logs: LogEntry[];
  onClear: () => void;
  onRefresh: () => void;
}) {
  const [searchText, setSearchText] = useState("");
  const filteredLogs = useMemo(() => {
    const query = searchText.trim().toLowerCase();
    if (!query) {
      return logs;
    }

    return logs.filter((entry) =>
      `${formatLogTime(entry.timestamp)} ${entry.level} ${entry.message}`
        .toLowerCase()
        .includes(query),
    );
  }, [logs, searchText]);

  return (
    <section className="panel logs-panel">
      <div className="panel-heading">
        <div>
          <p className="eyebrow">Runtime</p>
          <h2>运行日志</h2>
        </div>
        <div className="row-actions">
          <label className="log-search">
            <Search size={15} />
            <input
              onChange={(event) => setSearchText(event.target.value)}
              placeholder="搜索日志"
              value={searchText}
            />
          </label>
          <button className="ghost" onClick={onRefresh} type="button">
            <RefreshCw size={16} />
            <span>刷新</span>
          </button>
          <button className="ghost" onClick={onClear} type="button">
            清空
          </button>
        </div>
      </div>
      <div className="log-list">
        {logs.length === 0 ? (
          <div className="empty-state">暂无日志</div>
        ) : filteredLogs.length === 0 ? (
          <div className="empty-state">没有匹配的日志</div>
        ) : (
          filteredLogs
            .slice()
            .reverse()
            .map((entry, index) => (
              <div
                className="log-row"
                key={`${entry.timestamp}-${index}`}
                title={entry.message}
              >
                <span className={levelClass(entry.level)}>{entry.level}</span>
                <time>{formatLogTime(entry.timestamp)}</time>
                <p>{entry.message}</p>
              </div>
            ))
        )}
      </div>
    </section>
  );
}

function pageTitle(tab: TabKey) {
  return {
    dashboard: "状态总览",
    sites: "站点管理",
    settings: "保活设置",
    logs: "运行日志",
  }[tab];
}

function formatDate(value?: string | null) {
  if (!value) return "未计划";
  return new Intl.DateTimeFormat("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(value));
}

function formatTime(value: string) {
  return new Intl.DateTimeFormat("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).format(new Date(value));
}

function formatLogTime(value: string) {
  const date = new Date(value);
  const pad = (part: number) => String(part).padStart(2, "0");
  return [
    date.getFullYear(),
    pad(date.getMonth() + 1),
    pad(date.getDate()),
  ].join("-") + ` ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

function clampNumber(value: number, min: number, max: number) {
  return Math.min(max, Math.max(min, value));
}

function formatVersion(version: string) {
  return version.startsWith("v") ? version : `v${version}`;
}

function levelClass(level: LogEntry["level"]) {
  return `badge ${level.toLowerCase()}`;
}

export default App;
