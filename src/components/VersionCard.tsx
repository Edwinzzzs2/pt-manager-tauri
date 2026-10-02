import { BellDot, CheckCircle2, Clock3, Download, RefreshCw } from "lucide-react";

export type UpdatePhase = "checking" | "confirming" | "installing" | null;

type VersionCardProps = {
  currentVersion: string;
  availableVersion: string | null;
  checking: boolean;
  phase: UpdatePhase;
  lastCheckedAt: number | null;
  checkFailed: boolean;
  onCheck: () => Promise<void>;
};

function describeUpdate(props: VersionCardProps) {
  if (props.phase === "installing") {
    return { label: "正在更新", button: "下载并安装中…", tone: "pending" };
  }
  if (props.phase === "confirming") {
    return { label: "等待确认", button: "等待确认…", tone: "pending" };
  }
  if (props.checking || props.phase === "checking") {
    return { label: "检查中", button: "正在检查…", tone: "pending" };
  }
  if (props.checkFailed) {
    return { label: "检查失败", button: "重新检查", tone: "warning" };
  }
  if (props.availableVersion) {
    return { label: "可更新", button: "立即更新", tone: "available" };
  }
  // 尚未成功检查时只显示自动检查，避免把网络失败或初始状态误报为最新版。
  if (props.lastCheckedAt !== null) {
    return { label: "已是最新", button: "检查更新", tone: "latest" };
  }
  return { label: "自动检查", button: "检查更新", tone: "idle" };
}

export function VersionCard(props: VersionCardProps) {
  const state = describeUpdate(props);
  const busy = props.phase !== null || props.checking;
  const hasUpdate = Boolean(props.availableVersion);
  const spinning = props.checking || props.phase === "checking" || props.phase === "installing";
  const ActionIcon = hasUpdate && !busy && !props.checkFailed ? Download : RefreshCw;
  const lastChecked = props.lastCheckedAt === null
    ? null
    : new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit" }).format(props.lastCheckedAt);
  const actionTitle = props.availableVersion
    ? `${state.button}：${props.currentVersion || "当前版本"} → ${props.availableVersion}`
    : state.button;

  return (
    <section className={`version-card${hasUpdate ? " has-update" : ""}`} aria-label="应用版本与更新">
      <div className="version-card-header">
        <div className="version-current">
          <span className="version-label">当前版本</span>
          <strong>{props.currentVersion || "读取中…"}</strong>
        </div>
        <span className={`version-state ${state.tone}`} role="status">
          {state.tone === "latest" && <CheckCircle2 size={11} aria-hidden="true" />}
          {state.label}
        </span>
      </div>

      {props.availableVersion && (
        <div className="version-release">
          <span className="version-release-icon"><BellDot size={16} aria-hidden="true" /></span>
          <div>
            <span className="version-label">发现新版本</span>
            <strong>{props.availableVersion}</strong>
          </div>
        </div>
      )}

      <button
        className="version-action"
        disabled={busy}
        onClick={props.onCheck}
        title={actionTitle}
        aria-label={actionTitle}
        type="button"
      >
        <ActionIcon size={14} className={spinning ? "version-spinning" : undefined} aria-hidden="true" />
        <span>{state.button}</span>
        {hasUpdate && <span className="version-compact-dot" aria-hidden="true" />}
      </button>

      <div className="version-footnote">
        <Clock3 size={12} aria-hidden="true" />
        <span>每小时自动检查</span>
        {lastChecked && <time title="上次成功检查时间">{lastChecked}</time>}
      </div>
      {props.checkFailed && <p className="version-hint">检查失败，可重试或等待自动检查</p>}
      {props.phase === "installing" && <p className="version-hint">安装完成后将自动重启</p>}
    </section>
  );
}
