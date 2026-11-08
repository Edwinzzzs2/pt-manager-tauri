import { useEffect, useId, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type CronPreview = {
  state: "pending" | "valid" | "invalid";
  text: string;
};

export function useCronPreview(value: string): CronPreview {
  const expression = value.trim();
  const [result, setResult] = useState<{ expression: string; preview: CronPreview } | null>(null);

  useEffect(() => {
    if (!expression) return;
    let current = true;
    const timer = window.setTimeout(async () => {
      let preview: CronPreview;
      try {
        const text = await invoke<string>("describe_cron", { expression });
        preview = { state: "valid", text };
      } catch (err) {
        preview = { state: "invalid", text: err instanceof Error ? err.message : String(err) };
      }
      if (current) setResult({ expression, preview });
    }, 150);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [expression]);

  if (!expression) return { state: "invalid", text: "请输入 Cron 表达式" };
  if (result?.expression === expression) return result.preview;
  return { state: "pending", text: "正在校验…" };
}

export function CronPreviewText({ preview, id }: { preview: CronPreview; id?: string }) {
  return (
    <small id={id} className={`cron-preview cron-preview-${preview.state}`} aria-live="polite">
      {preview.text}
    </small>
  );
}

export function CronField({ value, preview, onChange, className }: {
  value: string;
  preview: CronPreview;
  onChange: (value: string) => void;
  className?: string;
}) {
  const hintId = useId();
  return (
    <label className={className ? `cron-field ${className}` : "cron-field"}>
      <span>Cron 表达式</span>
      <input
        aria-describedby={hintId}
        aria-invalid={preview.state === "invalid"}
        onChange={(event) => onChange(event.target.value)}
        placeholder="0 9 * * *"
        required
        value={value}
      />
      <CronPreviewText id={hintId} preview={preview} />
    </label>
  );
}
