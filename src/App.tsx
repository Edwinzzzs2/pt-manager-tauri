import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { getVersion, setTheme as setTauriTheme } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ask, message, open, save } from "@tauri-apps/plugin-dialog";
import { relaunch } from "@tauri-apps/plugin-process";
import {
  Activity,
  CheckCircle2,
  Clock3,
  Cookie,
  Download,
  Eye,
  EyeOff,
  FileUp,
  GripVertical,
  HelpCircle,
  ListChecks,
  Moon,
  PauseCircle,
  Play,
  Plus,
  RefreshCw,
  Save,
  Send,
  Search,
  Settings,
  Settings2,
  ShieldCheck,
  Square,
  Sun,
  Trash2,
  XCircle,
  type LucideIcon,
} from "lucide-react";
import "./App.css";

type Site = {
  id: string;
  name: string;
  url: string;
  username: string;
  password: string;
  totp_secret: string;
  auto_login: boolean;
  login_attempts_remaining: number | null;
  login_attempts_recorded_at: number | null;
  login_success_recorded_at: number | null;
  auto_keepalive: boolean;
  auto_signin: boolean;
  cookiecloud_upload: boolean;
};

type SiteDraft = Pick<
  Site,
  | "name"
  | "url"
  | "username"
  | "password"
  | "totp_secret"
  | "auto_login"
  | "auto_keepalive"
  | "auto_signin"
  | "cookiecloud_upload"
>;

type SiteAutomationSettings = Pick<
  Site,
  "auto_login" | "auto_keepalive" | "auto_signin" | "cookiecloud_upload"
>;

type AutomationSettingKey = keyof SiteAutomationSettings;

const automationFields: Array<{
  key: AutomationSettingKey;
  label: string;
  description: string;
}> = [
  {
    key: "auto_login",
    label: "自动登录",
    description: "保活时未登录才自动登录，需要已配置账号、密码、2FA 和 OCR。",
  },
  {
    key: "auto_keepalive",
    label: "自动保活",
    description: "纳入立即保活和定时保活任务；关闭后会跳过本站。",
  },
  {
    key: "auto_signin",
    label: "自动签到",
    description: "登录成功后自动查找并尝试签到入口。",
  },
  {
    key: "cookiecloud_upload",
    label: "自动上传 Cookie",
    description: "本站纳入自动上传范围，还需开启设置页的总开关。",
  },
];

type AppConfig = {
  sites: Site[];
  cron: string;
  cron_offset_minutes: number;
  cdp_port: number;
  visit_duration: number;
  random_delay: boolean;
  auto_launch: boolean;
  log_retention: number;
  auto_sync_cookie: boolean;
  auto_sync_cookie_after_keepalive: boolean;
  ocr_server_url: string;
  ocr_retry_count: number;
  min_login_attempts_remaining: number;
  update_proxy_url: string;
  update_proxy_password: string;
  cookiecloud: CookieCloudConfig;
  gotify: GotifyConfig;
};

type CookieCloudConfig = {
  server_url: string;
  uuid: string;
  password: string;
};

type GotifyConfig = {
  enabled: boolean;
  server_url: string;
  token: string;
  title: string;
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
  matched_local_storages: number;
  imported_local_storages: number;
};

type SiteImportResult = {
  config: AppConfig;
  imported: number;
  skipped: number;
};

type AppUpdateInfo = {
  version: string;
};

type TabKey = "dashboard" | "sites" | "settings" | "logs";
type ColorMode = "dark" | "light";

const themeStorageKey = "pt-manager-theme";
const releaseTag = import.meta.env.VITE_RELEASE_TAG as string | undefined;

const defaultConfig: AppConfig = {
  sites: [],
  cron: "0 9 * * *",
  cron_offset_minutes: 30,
  cdp_port: 9222,
  visit_duration: 30,
  random_delay: true,
  auto_launch: false,
  log_retention: 500,
  auto_sync_cookie: false,
  auto_sync_cookie_after_keepalive: false,
  ocr_server_url: "https://ocr-read.decoffee.top",
  ocr_retry_count: 2,
  min_login_attempts_remaining: 5,
  update_proxy_url: "",
  update_proxy_password: "",
  cookiecloud: {
    server_url: "",
    uuid: "",
    password: "",
  },
  gotify: {
    enabled: false,
    server_url: "",
    token: "",
    title: "PT Manager 保活结果",
  },
};

const navItems: Array<{ key: TabKey; label: string; icon: LucideIcon }> = [
  { key: "dashboard", label: "总览", icon: Activity },
  { key: "sites", label: "站点", icon: ListChecks },
  { key: "logs", label: "日志", icon: Clock3 },
  { key: "settings", label: "设置", icon: Settings },
];

function readStoredTheme(): ColorMode {
  const stored = window.localStorage.getItem(themeStorageKey);
  return stored === "light" ? "light" : "dark";
}

async function resolveDisplayVersion() {
  const tag = releaseTag?.trim();
  if (tag) {
    return tag;
  }
  return getVersion();
}

const knownLoginAttemptLimits: Record<string, number> = {
  "audiences.me": 20,
  "hdfans.org": 20,
  "hdkyl.in": 20,
  "pt.btschool.club": 20,
  "si-qi.xyz": 20,
  "xingtan.one": 10,
  "cspt.top": 10,
  "pandapt.net": 10,
  "hxpt.org": 10,
  "pttime.org": 10,
  "cyanbug.net": 10,
  "ptskit.org": 5,
  "open.cd": 11,
};

function knownLoginAttemptLimit(siteUrl: string): number | null {
  try {
    const parsed = new URL(siteUrl.includes("://") ? siteUrl : `https://${siteUrl}`);
    return knownLoginAttemptLimits[parsed.hostname.toLowerCase().replace(/^www\./, "")] ?? null;
  } catch {
    return null;
  }
}

function effectiveLoginAttemptThreshold(siteUrl: string, configured: number): number {
  const limit = knownLoginAttemptLimit(siteUrl);
  return limit == null ? configured : Math.min(configured, Math.max(1, limit - 1));
}

function App() {
  const [activeTab, setActiveTab] = useState<TabKey>("dashboard");
  const [colorMode, setColorMode] = useState<ColorMode>(() => readStoredTheme());
  const [config, setConfig] = useState<AppConfig>(defaultConfig);
  const [settingsDraft, setSettingsDraft] = useState<AppConfig>(defaultConfig);
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [newSite, setNewSite] = useState<SiteDraft>({
    name: "",
    url: "",
    username: "",
    password: "",
    totp_secret: "",
    auto_login: true,
    auto_keepalive: true,
    auto_signin: false,
    cookiecloud_upload: true,
  });
  const [editingSiteId, setEditingSiteId] = useState<string | null>(null);
  const [editingSite, setEditingSite] = useState<SiteDraft>({
    name: "",
    url: "",
    username: "",
    password: "",
    totp_secret: "",
    auto_login: true,
    auto_keepalive: true,
    auto_signin: false,
    cookiecloud_upload: true,
  });
  const [busy, setBusy] = useState(false);
  const [cdpBusy, setCdpBusy] = useState(false);
  const [cookieSyncBusy, setCookieSyncBusy] = useState(false);
  const [cookieCloudClearBusy, setCookieCloudClearBusy] = useState(false);
  const [gotifyTestBusy, setGotifyTestBusy] = useState(false);
  const [browserDataClearBusy, setBrowserDataClearBusy] = useState(false);
  const [cancelBusy, setCancelBusy] = useState(false);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [testingSiteId, setTestingSiteId] = useState<string | null>(null);
  const [recognizingSiteId, setRecognizingSiteId] = useState<string | null>(null);
  const [appVersion, setAppVersion] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const lastVisibleLog = useMemo(() => logs[logs.length - 1], [logs]);

  useEffect(() => {
    document.documentElement.dataset.theme = colorMode;
    window.localStorage.setItem(themeStorageKey, colorMode);
    setTauriTheme(colorMode).catch(showError);
    getCurrentWindow().setTheme(colorMode).catch(showError);
  }, [colorMode]);

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
    resolveDisplayVersion().then(setAppVersion).catch(showError);

    const timer = window.setInterval(() => {
      refreshStatus().catch(showError);
      refreshLogs().catch(showError);
    }, 1000);

    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    if (activeTab !== "sites") return;
    refreshConfig().catch(showError);
    const timer = window.setInterval(() => {
      refreshConfig().catch(showError);
    }, 60_000);
    return () => window.clearInterval(timer);
  }, [activeTab]);

  function showError(err: unknown) {
    setError(err instanceof Error ? err.message : String(err));
  }

  useEffect(() => {
    if (!error) return;
    const timer = window.setTimeout(() => setError(null), 6000);
    return () => window.clearTimeout(timer);
  }, [error]);

  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(null), 5000);
    return () => window.clearTimeout(timer);
  }, [notice]);

  function toggleColorMode() {
    setColorMode((mode) => (mode === "dark" ? "light" : "dark"));
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

  async function clearBrowserData() {
    const confirmed = await ask(
      "将清除专用 Chrome 的 Cookie、Local Storage 和缓存。清除后可重新同步 CookieCloud，确定继续？",
      {
        kind: "warning",
        okLabel: "清除",
        cancelLabel: "取消",
        title: "清除浏览器数据",
      },
    );
    if (!confirmed) {
      return;
    }

    setBrowserDataClearBusy(true);
    setError(null);
    try {
      await invoke("clear_browser_data");
      await refreshStatus();
      await refreshLogs();
      await message("浏览器数据已清除，可以重新同步 CookieCloud", {
        kind: "info",
        title: "清除完成",
      });
    } catch (err) {
      showError(err);
    } finally {
      setBrowserDataClearBusy(false);
    }
  }

  async function clearCookieCloudData() {
    const serverUrl = settingsDraft.cookiecloud.server_url.trim();
    if (
      !serverUrl ||
      !settingsDraft.cookiecloud.uuid.trim() ||
      !settingsDraft.cookiecloud.password
    ) {
      setError("请先填写 CookieCloud 地址、UUID 和密码");
      return;
    }
    const confirmed = await ask(
      `将永久清空 ${serverUrl || "当前 CookieCloud 服务器"} 上保存的全部 Cookie 和 Local Storage 数据，确定继续？`,
      {
        kind: "warning",
        okLabel: "清空云端数据",
        cancelLabel: "取消",
        title: "清空 CookieCloud 数据",
      },
    );
    if (!confirmed) return;

    setCookieCloudClearBusy(true);
    setError(null);
    try {
      await invoke("clear_cookiecloud_data", { config: settingsDraft.cookiecloud });
      await refreshLogs();
      setNotice("CookieCloud 服务器上的 Cookie 和 Local Storage 数据已清空");
    } catch (err) {
      showError(err);
    } finally {
      setCookieCloudClearBusy(false);
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
      const next = await invoke<AppConfig>("add_site", {
        name,
        url,
        username: newSite.username.trim(),
        password: newSite.password,
        totpSecret: newSite.totp_secret.trim(),
        autoLogin: newSite.auto_login,
        autoSignin: newSite.auto_signin,
        cookiecloudUpload: newSite.cookiecloud_upload,
      });
      setConfig(next);
      setSettingsDraft(next);
      setNewSite({
        name: "",
        url: "",
        username: "",
        password: "",
        totp_secret: "",
        auto_login: true,
        auto_keepalive: true,
        auto_signin: false,
        cookiecloud_upload: true,
      });
      await refreshStatus();
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  async function importSites() {
    const path = await open({
      directory: false,
      multiple: false,
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (!path) {
      return;
    }

    setBusy(true);
    setError(null);
    try {
      const result = await invoke<SiteImportResult>("import_sites_from_json", { path });
      setConfig(result.config);
      setSettingsDraft(result.config);
      await refreshStatus();
      await message(`成功导入 ${result.imported} 个站点，跳过 ${result.skipped} 个`, {
        kind: "info",
        title: "导入完成",
      });
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  async function exportConfig() {
    const path = await save({
      filters: [{ name: "JSON", extensions: ["json"] }],
      defaultPath: "pt-manager-config.json",
    });
    if (!path) {
      return;
    }

    setBusy(true);
    setError(null);
    try {
      await invoke("export_config", { path });
      await message("配置已成功导出为 JSON 文件", {
        kind: "info",
        title: "导出成功",
      });
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  async function importConfig() {
    const path = await open({
      directory: false,
      multiple: false,
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (!path) {
      return;
    }

    const confirmed = await ask("导入的配置会完全覆盖当前所有站点及选项设置，是否继续？", {
      title: "确认导入配置",
      kind: "warning",
      okLabel: "继续导入",
      cancelLabel: "取消",
    });
    if (!confirmed) {
      return;
    }

    setBusy(true);
    setError(null);
    try {
      const result = await invoke<AppConfig>("import_config", { path });
      setConfig(result);
      setSettingsDraft(result);
      await refreshStatus();
      await message("整个配置（包含站点和系统选项）已成功导入！", {
        kind: "info",
        title: "导入完成",
      });
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  function startEdit(site: Site) {
    setEditingSiteId(site.id);
    setEditingSite({
      name: site.name,
      url: site.url,
      username: site.username,
      password: site.password,
      totp_secret: site.totp_secret,
      auto_login: site.auto_login,
      auto_keepalive: site.auto_keepalive,
      auto_signin: site.auto_signin,
      cookiecloud_upload: site.cookiecloud_upload,
    });
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
      const next = await invoke<AppConfig>("update_site", {
        id,
        name,
        url,
        username: editingSite.username.trim(),
        password: editingSite.password,
        totpSecret: editingSite.totp_secret.trim(),
        autoLogin: editingSite.auto_login,
        autoKeepalive: editingSite.auto_keepalive,
        autoSignin: editingSite.auto_signin,
        cookiecloudUpload: editingSite.cookiecloud_upload,
      });
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

  async function removeSites(ids: string[]) {
    if (ids.length === 0) return false;
    setBusy(true);
    setError(null);
    try {
      const next = await invoke<AppConfig>("remove_sites", { ids });
      setConfig(next);
      setSettingsDraft(next);
      await refreshStatus();
      return true;
    } catch (err) {
      showError(err);
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function toggleSiteKeepalive(site: Site) {
    setBusy(true);
    setError(null);
    try {
      const next = await invoke<AppConfig>("update_site", {
        id: site.id,
        name: site.name,
        url: site.url,
        username: site.username,
        password: site.password,
        totpSecret: site.totp_secret,
        autoLogin: site.auto_login,
        autoKeepalive: !site.auto_keepalive,
        autoSignin: site.auto_signin,
        cookiecloudUpload: site.cookiecloud_upload,
      });
      setConfig(next);
      setSettingsDraft(next);
      await refreshStatus();
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  async function batchUpdateSiteAutomation(
    updates: Record<string, SiteAutomationSettings>,
  ) {
    if (Object.keys(updates).length === 0) return false;
    setBusy(true);
    setError(null);
    try {
      const nextSites = config.sites.map((site) =>
        updates[site.id] ? { ...site, ...updates[site.id] } : site,
      );
      const nextConfig = { ...config, sites: nextSites };
      await invoke("save_config", { config: nextConfig });
      setConfig(nextConfig);
      setSettingsDraft(nextConfig);
      await refreshStatus();
      return true;
    } catch (err) {
      showError(err);
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function reorderSites(siteIds: string[]) {
    if (siteIds.length < 2) return false;
    setBusy(true);
    setError(null);
    try {
      const order = new Map(siteIds.map((id, index) => [id, index]));
      const nextSites = [...config.sites].sort(
        (left, right) => (order.get(left.id) ?? 0) - (order.get(right.id) ?? 0),
      );
      const nextConfig = { ...config, sites: nextSites };
      await invoke("save_config", { config: nextConfig });
      setConfig(nextConfig);
      setSettingsDraft(nextConfig);
      await refreshStatus();
      return true;
    } catch (err) {
      showError(err);
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function testSiteLogin(site: Site) {
    setTestingSiteId(site.id);
    setError(null);
    try {
      await invoke<string>("test_site_login", { id: site.id });
    } catch (err) {
      showError(err);
    } finally {
      await refreshConfig().catch(showError);
      await refreshLogs().catch(showError);
      await refreshStatus().catch(showError);
      setTestingSiteId(null);
    }
  }

  async function recognizeSiteCaptcha(site: Site) {
    setRecognizingSiteId(site.id);
    setError(null);
    try {
      const result = await invoke<string>("recognize_site_captcha", { id: site.id });
      setNotice(result);
    } catch (err) {
      showError(err);
    } finally {
      await refreshConfig().catch(showError);
      await refreshLogs().catch(showError);
      setRecognizingSiteId(null);
    }
  }

  async function saveSettings() {
    const next: AppConfig = {
      ...settingsDraft,
      cdp_port: Number(settingsDraft.cdp_port) || 9222,
      visit_duration: Math.max(5, Number(settingsDraft.visit_duration) || 30),
      log_retention: clampNumber(Number(settingsDraft.log_retention) || 500, 50, 5000),
      cron: settingsDraft.cron.trim() || defaultConfig.cron,
      cron_offset_minutes: clampNumber(Number(settingsDraft.cron_offset_minutes) || 0, 0, 1440),
      random_delay: Number(settingsDraft.cron_offset_minutes) > 0,
      ocr_server_url: settingsDraft.ocr_server_url.trim(),
      ocr_retry_count: clampNumber(Number(settingsDraft.ocr_retry_count) || 2, 1, 5),
      min_login_attempts_remaining: clampNumber(
        Number(settingsDraft.min_login_attempts_remaining) || 5,
        1,
        20,
      ),
      update_proxy_url: settingsDraft.update_proxy_url.trim().replace(/\/+$/, ""),
      update_proxy_password: settingsDraft.update_proxy_password,
      gotify: {
        ...settingsDraft.gotify,
        server_url: settingsDraft.gotify.server_url.trim().replace(/\/+$/, ""),
        token: settingsDraft.gotify.token.trim(),
        title: settingsDraft.gotify.title.trim() || "PT Manager 保活结果",
      },
    };

    setBusy(true);
    setError(null);
    try {
      await invoke("save_config", { config: next });
      setConfig(next);
      setSettingsDraft(next);
      await refreshStatus();
      setNotice(next.ocr_server_url ? "设置已保存，OCR 服务已就绪" : "设置已保存");
    } catch (err) {
      showError(err);
    } finally {
      setBusy(false);
    }
  }

  async function testGotify() {
    setGotifyTestBusy(true);
    setError(null);
    try {
      const result = await invoke<string>("test_gotify", {
        config: {
          ...settingsDraft.gotify,
          server_url: settingsDraft.gotify.server_url.trim().replace(/\/+$/, ""),
          token: settingsDraft.gotify.token.trim(),
        },
      });
      setNotice(result);
    } catch (err) {
      showError(err);
    } finally {
      setGotifyTestBusy(false);
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
      const update = await invoke<AppUpdateInfo | null>("check_for_app_update");
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

      await invoke("download_and_install_app_update", {
        expectedVersion: update.version,
      });
      await relaunch();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setError(`更新失败：${message}`);
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
              className="icon-button theme-toggle"
              onClick={toggleColorMode}
              title={colorMode === "dark" ? "切换白色模式" : "切换黑色模式"}
              type="button"
            >
              {colorMode === "dark" ? <Sun size={17} /> : <Moon size={17} />}
            </button>
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

        <div className="workspace-scroll">
          {error ? (
            <div className="error-banner" role="alert">
              <XCircle size={18} />
              <span>{error}</span>
              <button aria-label="关闭提示" onClick={() => setError(null)} type="button">
                <XCircle size={15} />
              </button>
            </div>
          ) : null}
          {notice ? (
            <div className="notice-toast" role="status">
              <CheckCircle2 size={18} />
              <span>{notice}</span>
              <button aria-label="关闭提示" onClick={() => setNotice(null)} type="button">
                <XCircle size={15} />
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
              recentLogs={logs.slice(-30).reverse()}
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
              onImport={importSites}
              onCancelEdit={() => setEditingSiteId(null)}
              onEditChange={setEditingSite}
              onNewSiteChange={setNewSite}
              onRemove={removeSite}
              onRemoveSelected={removeSites}
              onRecognizeCaptcha={recognizeSiteCaptcha}
              onSave={saveSite}
              onStartEdit={startEdit}
              onTestLogin={testSiteLogin}
              onStopTest={stopTask}
              testingSiteId={testingSiteId}
              recognizingSiteId={recognizingSiteId}
              onToggleKeepalive={toggleSiteKeepalive}
              onBatchUpdateAutomation={batchUpdateSiteAutomation}
              onReorderSites={reorderSites}
            />
          ) : null}

          {activeTab === "settings" ? (
            <SettingsPanel
              busy={busy}
              browserDataClearBusy={browserDataClearBusy}
              cookieCloudClearBusy={cookieCloudClearBusy}
              cookieSyncBusy={cookieSyncBusy}
              gotifyTestBusy={gotifyTestBusy}
              draft={settingsDraft}
              onChange={setSettingsDraft}
              onClearBrowserData={clearBrowserData}
              onClearCookieCloudData={clearCookieCloudData}
              onSave={saveSettings}
              onSyncCookieCloud={syncCookieCloud}
              onTestGotify={testGotify}
              taskRunning={!!status?.is_running}
              onImportConfig={importConfig}
              onExportConfig={exportConfig}
            />
          ) : null}

          {activeTab === "logs" ? (
            <LogsPanel logs={logs} onClear={clearLogs} onRefresh={refreshLogs} />
          ) : null}
        </div>
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
      if (payload?.cookie_data || payload?.local_storage_data) {
        return payload;
      }
      if (payload?.encrypted) {
        throw new Error("CookieCloud 服务端返回了密文，请确认服务端支持 password 解密接口");
      }
      throw new Error("CookieCloud 返回数据缺少 cookie_data/local_storage_data");
    } catch (err) {
      errors.push(`${endpoint}: ${readableError(err)}`);
    }
  }

  throw new Error(
    [
      "CookieCloud 无法连接，请确认服务地址和协议是否与浏览器插件一致。",
      "常见格式：https://cookiecloud.example.com 或 http://127.0.0.1:8088",
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
  const latestResult = status?.last_result ?? lastLog;
  const [copiedKey, setCopiedKey] = useState<string | null>(null);

  function handleCopyLog(entry: LogEntry, key: string) {
    copyLogEntry(entry);
    setCopiedKey(key);
    setTimeout(() => setCopiedKey((k) => (k === key ? null : k)), 1500);
  }

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

        <div className="panel result-panel">
          <div className="panel-heading">
            <div>
              <p className="eyebrow">最近结果</p>
              <h2>{latestResult ? levelLabel(latestResult.level) : "暂无任务"}</h2>
            </div>
            <span className={latestResult ? levelClass(latestResult.level) : "badge"}>
              {latestResult ? formatTime(latestResult.timestamp) : "待执行"}
            </span>
          </div>
          <p className="result-text" title={latestResult?.message}>
            {latestResult
              ? summarizeResult(latestResult.message)
              : "添加站点后即可开始，Chrome 会在运行时自动准备。"}
          </p>
        </div>
      </section>

      <section className="panel live-log-panel">
        <div className="panel-heading live-log-heading">
          <div>
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
            recentLogs.map((entry, index) => {
              const rowKey = `${entry.timestamp}-${index}`;
              const copied = copiedKey === rowKey;
              return (
                <div
                  className={copied ? "compact-log-row copied" : "compact-log-row"}
                  key={rowKey}
                  onClick={() => handleCopyLog(entry, rowKey)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" || event.key === " ") {
                      event.preventDefault();
                      handleCopyLog(entry, rowKey);
                    }
                  }}
                  role="button"
                  tabIndex={0}
                  title={copied ? "已复制！" : `${entry.message}\n\n(点击复制完整日志)`}
                >
                  <span className={levelClass(entry.level)}>{entry.level}</span>
                  <time>{formatLogTime(entry.timestamp)}</time>
                  <p>{copied ? "✓ 已复制" : entry.message}</p>
                </div>
              );
            })
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
  onImport,
  onCancelEdit,
  onEditChange,
  onNewSiteChange,
  onRecognizeCaptcha,
  onRemove,
  onRemoveSelected,
  onSave,
  onStartEdit,
  onTestLogin,
  onStopTest,
  recognizingSiteId,
  testingSiteId,
  onToggleKeepalive,
  onBatchUpdateAutomation,
  onReorderSites,
}: {
  busy: boolean;
  config: AppConfig;
  editingSite: SiteDraft;
  editingSiteId: string | null;
  newSite: SiteDraft;
  onAdd: () => void;
  onImport: () => void;
  onCancelEdit: () => void;
  onEditChange: (site: SiteDraft) => void;
  onNewSiteChange: (site: SiteDraft) => void;
  onRecognizeCaptcha: (site: Site) => void;
  onRemove: (id: string) => Promise<void>;
  onRemoveSelected: (ids: string[]) => Promise<boolean>;
  onSave: (id: string) => void;
  onStartEdit: (site: Site) => void;
  onTestLogin: (site: Site) => void;
  onStopTest?: () => void;
  recognizingSiteId: string | null;
  testingSiteId: string | null;
  onToggleKeepalive: (site: Site) => void;
  onBatchUpdateAutomation: (
    updates: Record<string, SiteAutomationSettings>,
  ) => Promise<boolean>;
  onReorderSites: (siteIds: string[]) => Promise<boolean>;
}) {
  const [selectedSiteIds, setSelectedSiteIds] = useState<Set<string>>(() => new Set());
  const [showSitePassword, setShowSitePassword] = useState(false);
  const [showTotpSecret, setShowTotpSecret] = useState(false);
  const [batchSettingsOpen, setBatchSettingsOpen] = useState(false);
  const [batchSettings, setBatchSettings] = useState<
    Record<string, SiteAutomationSettings>
  >({});
  const [batchDraggingSiteId, setBatchDraggingSiteId] = useState<string | null>(null);
  const [batchDragOverSiteId, setBatchDragOverSiteId] = useState<string | null>(null);
  const batchDragOverSiteRef = useRef<string | null>(null);
  const batchSettingsListRef = useRef<HTMLDivElement | null>(null);
  const batchPointerPositionRef = useRef<{ clientX: number; clientY: number } | null>(null);
  const allSelected = config.sites.length > 0 && selectedSiteIds.size === config.sites.length;

  useEffect(() => {
    const availableIds = new Set(config.sites.map((site) => site.id));
    setSelectedSiteIds((current) =>
      new Set([...current].filter((id) => availableIds.has(id))),
    );
  }, [config.sites]);

  function toggleSite(id: string) {
    setSelectedSiteIds((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function startEditingSite(site: Site) {
    setShowSitePassword(false);
    setShowTotpSecret(false);
    onStartEdit(site);
  }

  function cancelEditingSite() {
    setShowSitePassword(false);
    setShowTotpSecret(false);
    onCancelEdit();
  }

  async function removeSelectedSites() {
    const count = selectedSiteIds.size;
    if (count === 0) return;
    const confirmed = await ask(`确定删除选中的 ${count} 个站点吗？此操作无法撤销。`, {
      kind: "warning",
      title: "批量删除站点",
    });
    if (!confirmed) return;
    if (await onRemoveSelected([...selectedSiteIds])) {
      setSelectedSiteIds(new Set());
    }
  }

  function openBatchSettings() {
    if (selectedSiteIds.size === 0) return;
    const next: Record<string, SiteAutomationSettings> = {};
    config.sites.forEach((site) => {
      if (!selectedSiteIds.has(site.id)) return;
      next[site.id] = {
        auto_login: site.auto_login,
        auto_keepalive: site.auto_keepalive,
        auto_signin: site.auto_signin,
        cookiecloud_upload: site.cookiecloud_upload,
      };
    });
    setBatchSettings(next);
    setBatchSettingsOpen(true);
  }

  function updateBatchSetting(
    siteId: string,
    key: AutomationSettingKey,
    value: boolean,
  ) {
    setBatchSettings((current) => ({
      ...current,
      [siteId]: {
        ...current[siteId],
        [key]: value,
      },
    }));
  }

  function setAllBatchSettings(value: boolean) {
    setBatchSettings((current) =>
      Object.fromEntries(
        Object.entries(current).map(([siteId, settings]) => [
          siteId,
          {
            ...settings,
            auto_login: value,
            auto_keepalive: value,
            auto_signin: value,
            cookiecloud_upload: value,
          },
        ]),
      ) as Record<string, SiteAutomationSettings>,
    );
  }

  async function saveBatchSettings() {
    const success = await onBatchUpdateAutomation(batchSettings);
    if (success) {
      setBatchSettingsOpen(false);
    }
  }

  async function reorderBatchSites(sourceSiteId: string, targetSiteId: string) {
    if (sourceSiteId === targetSiteId) return;
    const selectedIds = config.sites
      .filter((site) => batchSettings[site.id])
      .map((site) => site.id);
    const sourceIndex = selectedIds.indexOf(sourceSiteId);
    const targetIndex = selectedIds.indexOf(targetSiteId);
    if (sourceIndex < 0 || targetIndex < 0) return;
    selectedIds.splice(sourceIndex, 1);
    const insertIndex = sourceIndex < targetIndex ? targetIndex - 1 : targetIndex;
    selectedIds.splice(insertIndex, 0, sourceSiteId);

    const selectedSet = new Set(selectedIds);
    let selectedIndex = 0;
    const nextIds = config.sites.map((site) =>
      selectedSet.has(site.id) ? selectedIds[selectedIndex++] : site.id,
    );
    await onReorderSites(nextIds);
  }

  function handleBatchPointerDown(event: ReactPointerEvent<HTMLElement>, siteId: string) {
    if (busy) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture?.(event.pointerId);
    batchPointerPositionRef.current = {
      clientX: event.clientX,
      clientY: event.clientY,
    };
    batchDragOverSiteRef.current = siteId;
    setBatchDraggingSiteId(siteId);
    setBatchDragOverSiteId(siteId);
  }

  useEffect(() => {
    if (!batchDraggingSiteId) return;

    const updateDragTarget = (clientX: number, clientY: number) => {
      const element = document.elementFromPoint(clientX, clientY);
      const row = element?.closest<HTMLElement>("[data-batch-site-id]");
      const targetSiteId = row?.dataset.batchSiteId;
      if (targetSiteId && targetSiteId !== batchDraggingSiteId) {
        batchDragOverSiteRef.current = targetSiteId;
        setBatchDragOverSiteId(targetSiteId);
      }
    };

    const handlePointerMove = (event: PointerEvent) => {
      batchPointerPositionRef.current = {
        clientX: event.clientX,
        clientY: event.clientY,
      };
      updateDragTarget(event.clientX, event.clientY);
    };

    let autoScrollFrame = 0;
    const autoScroll = () => {
      const pointer = batchPointerPositionRef.current;
      const list = batchSettingsListRef.current;
      if (pointer && list) {
        const rect = list.getBoundingClientRect();
        const edge = 72;
        let scrollDelta = 0;
        if (pointer.clientY < rect.top + edge) {
          scrollDelta = -Math.max(3, Math.round((rect.top + edge - pointer.clientY) / 8));
        } else if (pointer.clientY > rect.bottom - edge) {
          scrollDelta = Math.max(3, Math.round((pointer.clientY - (rect.bottom - edge)) / 8));
        }
        if (scrollDelta !== 0) list.scrollTop += scrollDelta;
        updateDragTarget(pointer.clientX, pointer.clientY);
      }
      autoScrollFrame = window.requestAnimationFrame(autoScroll);
    };
    autoScrollFrame = window.requestAnimationFrame(autoScroll);

    const handlePointerUp = () => {
      const sourceSiteId = batchDraggingSiteId;
      const targetSiteId = batchDragOverSiteRef.current;
      batchDragOverSiteRef.current = null;
      batchPointerPositionRef.current = null;
      setBatchDraggingSiteId(null);
      setBatchDragOverSiteId(null);
      if (targetSiteId && targetSiteId !== sourceSiteId) {
        reorderBatchSites(sourceSiteId, targetSiteId).catch(() => undefined);
      }
    };

    window.addEventListener("pointermove", handlePointerMove);
    window.addEventListener("pointerup", handlePointerUp, { once: true });
    window.addEventListener("pointercancel", handlePointerUp, { once: true });
    return () => {
      window.removeEventListener("pointermove", handlePointerMove);
      window.removeEventListener("pointerup", handlePointerUp);
      window.removeEventListener("pointercancel", handlePointerUp);
      window.cancelAnimationFrame(autoScrollFrame);
    };
  }, [batchDraggingSiteId, config.sites, batchSettings, onReorderSites]);

  async function removeSingleSite(site: Site) {
    const confirmed = await ask(`确定删除站点“${site.name}”吗？此操作无法撤销。`, {
      kind: "warning",
      title: "删除站点",
    });
    if (confirmed) await onRemove(site.id);
  }

  return (
    <div className="content-stack sites-stack">
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
        <div className="site-form-actions">
          <button
            className="ghost"
            disabled={busy}
            onClick={onImport}
            title={'支持 [{"name":"站点名","url":"https://example.com"}] 或 {"sites":[...] }'}
            type="button"
          >
            <FileUp size={17} />
            <span>导入 JSON</span>
          </button>
          <button disabled={busy} onClick={onAdd} type="button">
            <Plus size={17} />
            <span>新增</span>
          </button>
        </div>
      </section>

      {config.sites.length > 0 ? (
        <div className="site-selection-bar">
          <label>
            <input
              checked={allSelected}
              onChange={() =>
                setSelectedSiteIds(
                  allSelected ? new Set() : new Set(config.sites.map((site) => site.id)),
                )
              }
              type="checkbox"
            />
            <span>{allSelected ? "取消全选" : "全选"}</span>
          </label>
          <span>已选 {selectedSiteIds.size} 项</span>
          <button
            className="ghost batch-settings-button"
            disabled={busy || selectedSiteIds.size === 0}
            onClick={openBatchSettings}
            type="button"
          >
            <Settings2 size={16} />
            <span>批量操作</span>
          </button>
          <button
            className="danger-action batch-delete-btn"
            disabled={busy || selectedSiteIds.size === 0}
            onClick={removeSelectedSites}
            type="button"
          >
            <Trash2 size={16} />
            <span>批量删除</span>
          </button>
        </div>
      ) : null}

      <section className="site-list site-list-scroll">
        {config.sites.length === 0 ? (
          <div className="empty-state">暂无站点</div>
        ) : (
          config.sites.map((site) => {
            const editing = editingSiteId === site.id;
            return (
              <article
                className={`site-row${editing ? " editing" : ""}`}
                key={site.id}
              >
                <input
                  aria-label={`选择 ${site.name}`}
                  checked={selectedSiteIds.has(site.id)}
                  className="site-select-checkbox"
                  onChange={() => toggleSite(site.id)}
                  type="checkbox"
                />
                {editing ? (
                  <div className="site-edit-fields">
                    <input
                      onChange={(event) =>
                        onEditChange({ ...editingSite, name: event.target.value })
                      }
                      placeholder="站点名称"
                      value={editingSite.name}
                    />
                    <input
                      onChange={(event) =>
                        onEditChange({ ...editingSite, url: event.target.value })
                      }
                      placeholder="站点 URL"
                      value={editingSite.url}
                    />
                    <input
                      autoComplete="username"
                      onChange={(event) =>
                        onEditChange({ ...editingSite, username: event.target.value })
                      }
                      placeholder="登录用户名"
                      value={editingSite.username}
                    />
                    <div className="password-field">
                      <input
                        autoComplete="new-password"
                        onChange={(event) =>
                          onEditChange({ ...editingSite, password: event.target.value })
                        }
                        placeholder="登录密码"
                        type={showSitePassword ? "text" : "password"}
                        value={editingSite.password}
                      />
                      <button
                        aria-label={showSitePassword ? "隐藏登录密码" : "显示登录密码"}
                        onClick={() => setShowSitePassword((value) => !value)}
                        type="button"
                      >
                        {showSitePassword ? <EyeOff size={16} /> : <Eye size={16} />}
                      </button>
                    </div>
                    <div className="password-field site-secret-field">
                      <input
                        autoComplete="off"
                        onChange={(event) =>
                          onEditChange({ ...editingSite, totp_secret: event.target.value })
                        }
                        placeholder="2FA Base32 密钥"
                        type={showTotpSecret ? "text" : "password"}
                        value={editingSite.totp_secret}
                      />
                      <button
                        aria-label={showTotpSecret ? "隐藏 2FA 密钥" : "显示 2FA 密钥"}
                        onClick={() => setShowTotpSecret((value) => !value)}
                        type="button"
                      >
                        {showTotpSecret ? <EyeOff size={16} /> : <Eye size={16} />}
                      </button>
                    </div>
                    <label className="switch-row site-auto-login">
                      <span className="label-with-help">
                        自动登录
                        <span
                          className="help-tip"
                          data-tooltip="执行保活时，只有未登录才会自动登录；需要已填写账号、密码、2FA 和 OCR 配置。"
                          tabIndex={0}
                        >
                          <HelpCircle size={14} />
                        </span>
                      </span>
                      <input
                        checked={editingSite.auto_login}
                        onChange={(event) =>
                          onEditChange({ ...editingSite, auto_login: event.target.checked })
                        }
                        type="checkbox"
                      />
                    </label>
                    <label className="switch-row site-auto-keepalive">
                      <span className="label-with-help">
                        自动保活
                        <span
                          className="help-tip"
                          data-tooltip="参加“立即保活”和定时保活；关闭后跳过本站，但仍可单独测试登录。"
                          tabIndex={0}
                        >
                          <HelpCircle size={14} />
                        </span>
                      </span>
                      <input
                        checked={editingSite.auto_keepalive}
                        onChange={(event) =>
                          onEditChange({ ...editingSite, auto_keepalive: event.target.checked })
                        }
                        type="checkbox"
                      />
                    </label>
                    <label
                      className="switch-row site-auto-signin"
                    >
                      <span className="label-with-help">
                        自动签到
                        <span
                          className="help-tip"
                          data-tooltip="登录成功后自动查找签到入口；找不到或暂不适配时只记日志，不影响保活。"
                          tabIndex={0}
                        >
                          <HelpCircle size={14} />
                        </span>
                      </span>
                      <input
                        checked={editingSite.auto_signin}
                        onChange={(event) =>
                          onEditChange({ ...editingSite, auto_signin: event.target.checked })
                        }
                        type="checkbox"
                      />
                    </label>
                    <label
                      className="switch-row site-cookiecloud-upload"
                    >
                      <span className="label-with-help">
                        自动上传 Cookie
                        <span
                          className="help-tip"
                          data-tooltip="本站是否纳入自动上传范围；还需开启设置页的自动上传总开关。关闭不影响手动同步和云端导入。"
                          tabIndex={0}
                        >
                          <HelpCircle size={14} />
                        </span>
                      </span>
                      <input
                        checked={editingSite.cookiecloud_upload}
                        onChange={(event) =>
                          onEditChange({
                            ...editingSite,
                            cookiecloud_upload: event.target.checked,
                          })
                        }
                        type="checkbox"
                      />
                    </label>
                  </div>
                ) : (
                  <div className="site-main">
                    <div className="site-title-line">
                      <strong>{site.name}</strong>
                      {!site.auto_keepalive ? (
                        <span className="site-login-badge disabled">
                          未启用
                        </span>
                      ) : site.auto_login ? (
                        <span className="site-login-badge active">
                          自动登录
                        </span>
                      ) : (
                        <span className="site-login-badge enabled">
                          已启用
                        </span>
                      )}
                      {site.auto_signin ? (
                        <span className="site-login-badge active">自动签到</span>
                      ) : null}
                    </div>
                    <div className="site-details">
                      <span className="site-url">{site.url}</span>
                      {site.username || site.totp_secret || site.login_attempts_remaining != null ? (
                        <div className="site-auth-details">
                          {site.username ? <span>账号：{site.username}</span> : null}
                          {site.totp_secret ? <TotpCode secret={site.totp_secret} /> : null}
                          {site.login_attempts_remaining != null ? (
                            <span
                              className={`site-attempts${
                                site.login_attempts_remaining <=
                                effectiveLoginAttemptThreshold(
                                  site.url,
                                  config.min_login_attempts_remaining,
                                )
                                  ? " danger"
                                  : ""
                              }`}
                            >
                              剩余尝试：{site.login_attempts_remaining}
                            </span>
                          ) : null}
                        </div>
                      ) : null}
                    </div>
                  </div>
                )}
                <div className="row-actions">
                  {editing ? (
                    <>
                      <button onClick={() => onSave(site.id)} type="button">
                        <Save size={16} />
                        <span>保存</span>
                      </button>
                      <button className="ghost" onClick={cancelEditingSite} type="button">
                        取消
                      </button>
                    </>
                  ) : (
                    <>
                      {site.auto_login ? (
                        <button
                          className={`ghost site-test-button ${testingSiteId === site.id ? "testing" : ""}`}
                          disabled={busy || (testingSiteId !== null && testingSiteId !== site.id)}
                          onClick={() => {
                            if (testingSiteId === site.id) {
                              onStopTest?.();
                            } else {
                              onTestLogin(site);
                            }
                          }}
                          title={testingSiteId === site.id ? "终止当前测试" : "单独测试自动登录"}
                          type="button"
                        >
                          {testingSiteId === site.id ? (
                            <>
                              <RefreshCw size={16} className="icon-testing spin" />
                              <XCircle size={16} className="icon-stop" />
                              <span className="text-testing">测试中</span>
                              <span className="text-stop">终止</span>
                            </>
                          ) : (
                            <>
                              <ShieldCheck size={16} />
                              <span>测试</span>
                            </>
                          )}
                        </button>
                      ) : null}
                      {site.auto_login && !site.url.toLowerCase().includes("kp.m-team.cc") && !site.url.toLowerCase().includes("hdkyl.in") ? (
                        <button
                          className="ghost site-captcha-button"
                          disabled={busy || testingSiteId !== null || recognizingSiteId !== null}
                          onClick={() => onRecognizeCaptcha(site)}
                          title="识别并填入当前图片验证码"
                          type="button"
                        >
                          {recognizingSiteId === site.id ? (
                            <RefreshCw size={16} />
                          ) : (
                            <Search size={16} />
                          )}
                          <span>{recognizingSiteId === site.id ? "识别中" : "识别码"}</span>
                        </button>
                      ) : null}
                      <button onClick={() => startEditingSite(site)} type="button">
                        编辑
                      </button>
                      <button
                        aria-label={`删除 ${site.name}`}
                        className="danger-action site-delete-button"
                        disabled={busy}
                        onClick={() => removeSingleSite(site)}
                        title="删除站点"
                        type="button"
                      >
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

      {batchSettingsOpen ? (
        <div
          className="batch-settings-overlay"
          onMouseDown={(event) => {
            if (event.target === event.currentTarget) setBatchSettingsOpen(false);
          }}
          role="presentation"
        >
          <div
            aria-labelledby="batch-settings-title"
            aria-modal="true"
            className="batch-settings-modal"
            onMouseDown={(event) => event.stopPropagation()}
            role="dialog"
          >
            <div className="batch-settings-heading">
              <div>
                <span className="eyebrow">批量设置</span>
                <h2 id="batch-settings-title">批量操作站点</h2>
                <p>已选择 {Object.keys(batchSettings).length} 个站点，可逐站调整四项自动化开关。</p>
              </div>
              <button
                aria-label="关闭批量设置"
                className="icon-button"
                onClick={() => setBatchSettingsOpen(false)}
                type="button"
              >
                <XCircle size={18} />
              </button>
            </div>

            <div className="batch-settings-toolbar">
              <span>快速设置所选站点</span>
              <div className="batch-settings-quick-actions">
                <button className="ghost" onClick={() => setAllBatchSettings(true)} type="button">
                  <CheckCircle2 size={15} />
                  <span>全部开启</span>
                </button>
                <button className="ghost" onClick={() => setAllBatchSettings(false)} type="button">
                  <PauseCircle size={15} />
                  <span>全部关闭</span>
                </button>
              </div>
            </div>

            <div className="batch-settings-list" ref={batchSettingsListRef}>
              {config.sites
                .filter((site) => batchSettings[site.id])
                .map((site) => {
                  const settings = batchSettings[site.id];
                  return (
                    <div
                      className={`batch-site-row${batchDraggingSiteId === site.id ? " dragging" : ""}${batchDragOverSiteId === site.id ? " drag-over" : ""}`}
                      data-batch-site-id={site.id}
                      key={site.id}
                    >
                      <div
                        aria-label={`拖动调整 ${site.name} 顺序`}
                        aria-disabled={busy}
                        className="batch-site-drag-handle"
                        onPointerDown={(event) => handleBatchPointerDown(event, site.id)}
                        title="拖动调整站点顺序"
                        role="button"
                        tabIndex={0}
                      >
                        <GripVertical size={17} />
                      </div>
                      <div className="batch-site-meta">
                        <strong>{site.name}</strong>
                        <span>{site.url}</span>
                      </div>
                      <div className="batch-site-switches">
                        {automationFields.map((field) => (
                          <label
                            className="batch-switch"
                            key={field.key}
                            title={field.description}
                          >
                            <span>{field.label}</span>
                            <input
                              checked={settings[field.key]}
                              onChange={(event) =>
                                updateBatchSetting(site.id, field.key, event.target.checked)
                              }
                              type="checkbox"
                            />
                          </label>
                        ))}
                      </div>
                    </div>
                  );
                })}
            </div>

            <div className="batch-settings-footer">
              <span>保存后立即应用到这些站点。</span>
              <div className="row-actions">
                <button className="ghost" onClick={() => setBatchSettingsOpen(false)} type="button">
                  取消
                </button>
                <button disabled={busy} onClick={saveBatchSettings} type="button">
                  <Save size={16} />
                  <span>保存设置</span>
                </button>
              </div>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}

function SettingsPanel({
  browserDataClearBusy,
  busy,
  cookieCloudClearBusy,
  cookieSyncBusy,
  gotifyTestBusy,
  draft,
  onChange,
  onClearBrowserData,
  onClearCookieCloudData,
  onSave,
  onSyncCookieCloud,
  onTestGotify,
  taskRunning,
  onImportConfig,
  onExportConfig,
}: {
  browserDataClearBusy: boolean;
  busy: boolean;
  cookieCloudClearBusy: boolean;
  cookieSyncBusy: boolean;
  gotifyTestBusy: boolean;
  draft: AppConfig;
  onChange: (config: AppConfig) => void;
  onClearBrowserData: () => void;
  onClearCookieCloudData: () => void;
  onSave: () => void;
  onSyncCookieCloud: () => void;
  onTestGotify: () => void;
  taskRunning: boolean;
  onImportConfig: () => void;
  onExportConfig: () => void;
}) {
  const [showProxyPassword, setShowProxyPassword] = useState(false);
  const [showCookiePassword, setShowCookiePassword] = useState(false);
  const [showGotifyToken, setShowGotifyToken] = useState(false);

  return (
    <div className="settings-stack">
      <div className="settings-scroll">
        <section className="panel settings-card">
          <div className="panel-heading">
            <div>
              <p className="eyebrow">Updater</p>
              <div className="title-with-help">
                <h2>应用更新</h2>
                <span
                  className="help-tip"
                  data-tooltip="检查清单和下载安装包都会通过该地址转发；留空时直接连接 GitHub。"
                  tabIndex={0}
                >
                  <HelpCircle size={16} />
                </span>
              </div>
            </div>
          </div>
          <div className="settings-form">
            <label>
              <span className="label-with-help">
                更新代理地址
                <span
                  className="help-tip"
                  aria-label="更新代理部署说明"
                  data-tooltip="默认留空，直接连接 GitHub。如需代理下载，请自行部署 https://github.com/Edwinzzzs2/vercel-proxy 项目，再将部署后的服务地址填入此处。"
                  tabIndex={0}
                >
                  <HelpCircle size={14} />
                </span>
              </span>
              <input
                onChange={(event) => onChange({ ...draft, update_proxy_url: event.target.value })}
                placeholder="https://proxy.example.com"
                type="url"
                value={draft.update_proxy_url}
              />
            </label>
            <label>
              <span>更新代理密码</span>
              <div className="password-field">
                <input
                  autoComplete="new-password"
                  onChange={(event) =>
                    onChange({ ...draft, update_proxy_password: event.target.value })
                  }
                  placeholder="X-Proxy-Token"
                  type={showProxyPassword ? "text" : "password"}
                  value={draft.update_proxy_password}
                />
                <button
                  aria-label={showProxyPassword ? "隐藏代理密码" : "显示代理密码"}
                  onClick={() => setShowProxyPassword((value) => !value)}
                  title={showProxyPassword ? "隐藏代理密码" : "显示代理密码"}
                  type="button"
                >
                  {showProxyPassword ? <EyeOff size={16} /> : <Eye size={16} />}
                </button>
              </div>
            </label>
            <p className="field-hint">
              填写 URL 前缀型代理地址；如代理启用了鉴权，请填写与服务端
              PROXY_AUTH_TOKEN 一致的密码。检查更新和安装包下载都会使用该配置。
            </p>
          </div>
        </section>

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
              placeholder="0 9 * * *"
              value={draft.cron}
            />
          </label>
          <label>
            <span className="label-with-help">
              随机延迟上限（分钟）
              <span
                className="help-tip"
                data-tooltip="计划触发后随机等待 0 到该数值分钟；填 0 表示按 Cron 准点执行。"
                tabIndex={0}
              >
                <HelpCircle size={14} />
              </span>
            </span>
            <input
              min={0}
              max={1440}
              onChange={(event) =>
                onChange({ ...draft, cron_offset_minutes: Number(event.target.value) })
              }
              placeholder="0"
              title="计划触发后随机延迟 0 到该数值分钟；0 表示不随机延迟"
              type="number"
              value={draft.cron_offset_minutes}
            />
          </label>
          <label>
            <span>保活页面停留时间（秒）</span>
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

        </section>

        <section className="panel settings-card cookiecloud-panel">
        <div className="panel-heading">
          <div>
            <p className="eyebrow">CookieCloud</p>
            <div className="title-with-help">
              <h2>Cookie 同步</h2>
              <span
                className="help-tip"
                data-tooltip="填写与 CookieCloud 插件相同的服务地址、UUID 和密码；“同步 Cookie”会把云端 Cookie 与 Local Storage 导入专用 Chrome。"
                tabIndex={0}
              >
                <HelpCircle size={16} />
              </span>
            </div>
          </div>
          <div className="row-actions">
            <button
              className="danger-action cookie-clear-action"
              disabled={cookieCloudClearBusy || cookieSyncBusy || taskRunning}
              onClick={onClearCookieCloudData}
              title="永久清空当前配置的 CookieCloud 服务器上的 Cookie 和 Local Storage；不会删除本地浏览器数据"
              type="button"
            >
              {cookieCloudClearBusy ? <RefreshCw size={16} /> : <Cookie size={16} />}
              <span>
                {taskRunning ? "任务执行中" : cookieCloudClearBusy ? "清空中" : "清空云端数据"}
              </span>
            </button>
            <button
              className="ghost"
              disabled={cookieCloudClearBusy || cookieSyncBusy || taskRunning}
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
              placeholder="https://cookiecloud.example.com"
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
            <div className="password-field">
              <input
                onChange={(event) =>
                  onChange({
                    ...draft,
                    cookiecloud: { ...draft.cookiecloud, password: event.target.value },
                  })
                }
                type={showCookiePassword ? "text" : "password"}
                value={draft.cookiecloud.password}
              />
              <button
                aria-label={showCookiePassword ? "隐藏密码" : "显示密码"}
                onClick={() => setShowCookiePassword((value) => !value)}
                title={showCookiePassword ? "隐藏密码" : "显示密码"}
                type="button"
              >
                {showCookiePassword ? <EyeOff size={16} /> : <Eye size={16} />}
              </button>
            </div>
          </label>
          <label className="switch-row">
            <span className="label-with-help">
              保活前自动同步
              <span
                className="help-tip"
                data-tooltip="每次保活开始前，从 CookieCloud 下载 Cookie 和 Local Storage 到专用 Chrome；不会上传本地数据。"
                tabIndex={0}
              >
                <HelpCircle size={14} />
              </span>
            </span>
            <input
              checked={draft.auto_sync_cookie}
              onChange={(event) => onChange({ ...draft, auto_sync_cookie: event.target.checked })}
              type="checkbox"
            />
          </label>
          <label className="switch-row">
            <span className="label-with-help">
              保活/测试后自动上传
              <span
                className="help-tip"
                data-tooltip="自动上传总开关；开启后，保活任务和单站点测试只上传站点页中已开启“自动上传 Cookie”的站点。"
                tabIndex={0}
              >
                <HelpCircle size={14} />
              </span>
            </span>
            <input
              checked={draft.auto_sync_cookie_after_keepalive}
              onChange={(event) =>
                onChange({
                  ...draft,
                  auto_sync_cookie_after_keepalive: event.target.checked,
                })
              }
              type="checkbox"
            />
          </label>
          <p className="field-hint cookiecloud-upload-summary">
            自动上传范围：已选择 {draft.sites.filter((site) => site.cookiecloud_upload).length} / {draft.sites.length} 个站点。可在“站点”页编辑每个站点的上传开关。
          </p>
        </div>
        </section>

        <section className="panel settings-card ocr-panel">
          <div className="panel-heading">
            <div>
              <p className="eyebrow">OCR</p>
              <div className="title-with-help">
                <h2>验证码识别</h2>
                <span
                  className="help-tip"
                  data-tooltip="当前兼容 Edwinzzzs2/ocrRead（基于 ddddocr）的 HTTP API，主要识别常见单行图片验证码中的中文、英文、数字及部分特殊字符，并支持颜色过滤。本应用会调用 /status、/initialize 和 /ocr；目标检测、滑块匹配等接口由服务提供但不会在本站点登录流程中自动调用。保存设置时会检查 /status；OCR 未加载时自动调用 /initialize。服务重启后，下次识别也会自动重新检查。"
                  tabIndex={0}
                >
                  <HelpCircle size={16} />
                </span>
              </div>
            </div>
          </div>
          <div className="settings-form">
            <label>
              <span>OCR 服务地址</span>
              <input
                onChange={(event) => onChange({ ...draft, ocr_server_url: event.target.value })}
                placeholder="https://ocr-read.decoffee.top"
                type="url"
                value={draft.ocr_server_url}
              />
            </label>
            <label>
              <span>识别尝试次数</span>
              <input
                max={5}
                min={1}
                onChange={(event) =>
                  onChange({ ...draft, ocr_retry_count: Number(event.target.value) })
                }
                type="number"
                value={draft.ocr_retry_count}
              />
            </label>
            <label>
              <span className="title-with-help">
                最低剩余登录次数
                <span
                  className="help-tip"
                  data-tooltip="站点显示的剩余尝试次数小于或等于该值时，停止自动填写、验证码识别和登录重试，避免 IP 被封锁。"
                  tabIndex={0}
                >
                  <HelpCircle size={15} />
                </span>
              </span>
              <input
                max={20}
                min={1}
                onChange={(event) =>
                  onChange({
                    ...draft,
                    min_login_attempts_remaining: Number(event.target.value),
                  })
                }
                type="number"
                value={draft.min_login_attempts_remaining}
              />
            </label>
          </div>
        </section>

        <section className="panel settings-card">
          <div className="panel-heading">
            <div>
              <p className="eyebrow">Gotify</p>
              <h2>任务通知</h2>
            </div>
            <div className="row-actions">
              <button
                className="ghost"
                disabled={
                  gotifyTestBusy ||
                  !draft.gotify.server_url.trim() ||
                  !draft.gotify.token.trim()
                }
                onClick={onTestGotify}
                type="button"
              >
                {gotifyTestBusy ? <RefreshCw size={16} /> : <Send size={16} />}
                <span>{gotifyTestBusy ? "测试中" : "测试通知"}</span>
              </button>
            </div>
          </div>
          <div className="settings-form">
            <label className="switch-row">
              <span>启用 Gotify 通知</span>
              <input
                checked={draft.gotify.enabled}
                onChange={(event) =>
                  onChange({
                    ...draft,
                    gotify: { ...draft.gotify, enabled: event.target.checked },
                  })
                }
                type="checkbox"
              />
            </label>
            <label>
              <span>Gotify 服务地址</span>
              <input
                onChange={(event) =>
                  onChange({
                    ...draft,
                    gotify: { ...draft.gotify, server_url: event.target.value },
                  })
                }
                placeholder="https://gotify.example.com"
                type="url"
                value={draft.gotify.server_url}
              />
            </label>
            <label>
              <span>通知标题</span>
              <input
                onChange={(event) =>
                  onChange({
                    ...draft,
                    gotify: { ...draft.gotify, title: event.target.value },
                  })
                }
                placeholder="PT Manager 保活结果"
                type="text"
                value={draft.gotify.title}
              />
            </label>
            <label>
              <span>应用 Token</span>
              <div className="password-field">
                <input
                  onChange={(event) =>
                    onChange({
                      ...draft,
                      gotify: { ...draft.gotify, token: event.target.value },
                    })
                  }
                  placeholder="Gotify 应用 Token"
                  type={showGotifyToken ? "text" : "password"}
                  value={draft.gotify.token}
                />
                <button
                  aria-label={showGotifyToken ? "隐藏 Token" : "显示 Token"}
                  onClick={() => setShowGotifyToken((value) => !value)}
                  title={showGotifyToken ? "隐藏 Token" : "显示 Token"}
                  type="button"
                >
                  {showGotifyToken ? <EyeOff size={16} /> : <Eye size={16} />}
                </button>
              </div>
            </label>
            <p className="field-hint">
              每次保活任务结束后发送一条通知；开启自动登录时会同时汇总登录结果。
            </p>
          </div>
        </section>

        <section className="panel settings-card" aria-labelledby="site-support-title">
          <div className="panel-heading">
            <div>
              <p className="eyebrow">Compatibility</p>
              <h2 id="site-support-title">站点支持说明</h2>
            </div>
          </div>
          <p className="field-hint">
            可添加站点进行定时访问保活；自动登录、签到和流量读取的支持范围各不相同。
          </p>
          <details className="site-support-details">
            <summary>查看当前适配清单与支持范围</summary>
            {/* Keep this list aligned with the login, signin and traffic modules in src-tauri/src/cdp. */}
            <dl className="site-support-list">
              <div>
                <dt>专门适配的自动登录</dt>
                <dd>M-Team（kp.m-team.cc）、HDKylin（hdkyl.in）、PTing（pting.club）、SixCloud（666clouds.com）。</dd>
              </div>
              <div>
                <dt>专门适配的自动签到</dt>
                <dd>Audiences（audiences.me）、HDFans（hdfans.org）、PterClub（pterclub.*）、YemaPT（yemapt.org）、Hares（club.hares.top）、Rousi（rousi.pro）、PTing（pting.club）。</dd>
              </div>
              <div>
                <dt>通用站点支持</dt>
                <dd>其他站点尝试使用 NexusPHP 兼容登录流程及通用签到入口识别。未列出的站点也可添加，但能否自动登录、签到取决于页面结构；需要先在站点配置中启用对应功能。</dd>
              </div>
              <div>
                <dt>流量信息读取</dt>
                <dd>支持识别常见 PT 页面中的上传量、下载量和分享率，并适配 SixCloud 的出入流量显示；云服务流量不代表 PT 分享率。</dd>
              </div>
            </dl>
            <p className="field-hint">
              适配不保证每次执行成功。站点改版、登录状态及验证码可能影响结果，请以任务日志为准；需要 OCR 的登录流程请先配置 OCR 服务。
            </p>
          </details>
        </section>

        <section className="panel settings-card config-backup-panel">
          <div className="panel-heading">
            <div>
              <p className="eyebrow">Backup</p>
              <h2>备份与还原</h2>
            </div>
            <div className="row-actions" style={{ gap: "10px" }}>
              <button
                className="ghost"
                disabled={busy}
                onClick={onImportConfig}
                type="button"
                style={{ minHeight: "34px", padding: "0 12px" }}
              >
                <FileUp size={16} />
                <span>导入全部配置</span>
              </button>
              <button
                className="ghost"
                disabled={busy}
                onClick={onExportConfig}
                type="button"
                style={{ minHeight: "34px", padding: "0 12px" }}
              >
                <Download size={16} />
                <span>导出全部配置</span>
              </button>
            </div>
          </div>
        </section>

        <section className="panel settings-card browser-data-panel">
          <div className="panel-heading">
            <div>
              <p className="eyebrow">Chrome</p>
              <h2>浏览器数据</h2>
            </div>
            <div className="row-actions">
              <button
                className="danger-action"
                disabled={browserDataClearBusy || taskRunning}
                onClick={onClearBrowserData}
                type="button"
              >
                {browserDataClearBusy ? <RefreshCw size={16} /> : <Trash2 size={16} />}
                <span>
                  {taskRunning ? "任务执行中" : browserDataClearBusy ? "清除中" : "清除浏览器数据"}
                </span>
              </button>
            </div>
          </div>
        </section>
      </div>

      <div className="settings-actions settings-actions-bottom">
        <button className="primary-action settings-save" disabled={busy} onClick={onSave} type="button">
          <Save size={18} />
          <span>保存全部设置</span>
        </button>
      </div>
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
  const [levelFilter, setLevelFilter] = useState<LogEntry["level"] | "ALL">("ALL");
  const [copiedKey, setCopiedKey] = useState<string | null>(null);
  const filteredLogs = useMemo(() => {
    const query = searchText.trim().toLowerCase();
    return logs.filter((entry) => {
      if (levelFilter !== "ALL" && entry.level !== levelFilter) return false;
      if (!query) return true;
      return `${formatLogTime(entry.timestamp)} ${entry.level} ${entry.message}`
        .toLowerCase()
        .includes(query);
    });
  }, [levelFilter, logs, searchText]);

  function handleCopyLog(entry: LogEntry, key: string) {
    copyLogEntry(entry);
    setCopiedKey(key);
    setTimeout(() => setCopiedKey((k) => (k === key ? null : k)), 1500);
  }

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
          <label className="log-level-filter">
            <span>级别</span>
            <select
              aria-label="按日志级别筛选"
              onChange={(event) =>
                setLevelFilter(event.target.value as LogEntry["level"] | "ALL")
              }
              value={levelFilter}
            >
              <option value="ALL">全部</option>
              <option value="SUCCESS">成功 SUCCESS</option>
              <option value="INFO">信息 INFO</option>
              <option value="ERROR">错误 ERROR</option>
            </select>
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
            .map((entry, index) => {
              const rowKey = `${entry.timestamp}-${index}`;
              const copied = copiedKey === rowKey;
              return (
                <div
                  className={copied ? "log-row copied" : "log-row"}
                  key={rowKey}
                  onClick={() => handleCopyLog(entry, rowKey)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" || event.key === " ") {
                      event.preventDefault();
                      handleCopyLog(entry, rowKey);
                    }
                  }}
                  role="button"
                  tabIndex={0}
                  title={copied ? "已复制！" : `${entry.message}\n\n(点击复制完整日志)`}
                >
                  <span className={levelClass(entry.level)}>{entry.level}</span>
                  <time>{formatLogTime(entry.timestamp)}</time>
                  <p>{copied ? "✓ 已复制" : entry.message}</p>
                </div>
              );
            })
        )}
      </div>
    </section>
  );
}

function TotpCode({ secret }: { secret: string }) {
  const [code, setCode] = useState("------");
  const [remaining, setRemaining] = useState(30);

  useEffect(() => {
    let active = true;
    const update = async () => {
      const now = Math.floor(Date.now() / 1000);
      setRemaining(30 - (now % 30));
      try {
        const next = await generateTotp(secret, now);
        if (active) setCode(next);
      } catch {
        if (active) setCode("密钥无效");
      }
    };
    update();
    const timer = window.setInterval(update, 1000);
    return () => {
      active = false;
      window.clearInterval(timer);
    };
  }, [secret]);

  return (
    <span className="totp-code" title="当前 2FA 动态验证码">
      2FA <strong>{code}</strong>
      {code !== "密钥无效" ? <small>{remaining}s</small> : null}
    </span>
  );
}

async function generateTotp(secret: string, unixSeconds: number) {
  const keyBytes = decodeBase32(secret);
  const counter = BigInt(Math.floor(unixSeconds / 30));
  const message = new ArrayBuffer(8);
  new DataView(message).setBigUint64(0, counter);
  const key = await crypto.subtle.importKey(
    "raw",
    keyBytes,
    { name: "HMAC", hash: "SHA-1" },
    false,
    ["sign"],
  );
  const digest = new Uint8Array(await crypto.subtle.sign("HMAC", key, message));
  const offset = digest[digest.length - 1] & 0x0f;
  const value =
    (((digest[offset] & 0x7f) << 24) |
      ((digest[offset + 1] & 0xff) << 16) |
      ((digest[offset + 2] & 0xff) << 8) |
      (digest[offset + 3] & 0xff)) %
    1_000_000;
  return value.toString().padStart(6, "0");
}

function decodeBase32(value: string) {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  const normalized = value.toUpperCase().replace(/[\s=-]/g, "");
  if (!normalized || [...normalized].some((char) => !alphabet.includes(char))) {
    throw new Error("Invalid Base32 secret");
  }
  let bits = "";
  for (const char of normalized) {
    bits += alphabet.indexOf(char).toString(2).padStart(5, "0");
  }
  const bytes = new Uint8Array(Math.floor(bits.length / 8));
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Number.parseInt(bits.slice(index * 8, index * 8 + 8), 2);
  }
  return bytes;
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

function summarizeResult(message: string) {
  if (message.startsWith("CookieCloud 同步完成：")) {
    return message.split("；", 1)[0];
  }
  return message;
}

function levelLabel(level: LogEntry["level"]) {
  return {
    SUCCESS: "执行成功",
    ERROR: "执行失败",
    INFO: "执行信息",
  }[level];
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

function copyLogEntry(entry: LogEntry) {
  const content = `${formatLogTime(entry.timestamp)} [${entry.level}] ${entry.message}`;
  void navigator.clipboard.writeText(content).catch(() => undefined);
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
