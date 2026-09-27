import { useEffect, useRef, useState } from "react";
import { FolderSearch, Plus, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  settingsApi,
  type ClaudeScanDirectoryStatus,
} from "@/lib/api/settings";

interface Props {
  primary?: string;
  dirs: string[];
  onChange: (dirs: string[]) => void;
}

export function ClaudeScanDirectories({ primary, dirs, onChange }: Props) {
  const { t } = useTranslation();
  const [statuses, setStatuses] = useState<ClaudeScanDirectoryStatus[]>([]);
  const [error, setError] = useState<string>();
  const [draft, setDraft] = useState("");
  const [browsing, setBrowsing] = useState(false);
  const latest = useRef({ dirs, onChange });
  latest.current = { dirs, onChange };
  const directoryKey = JSON.stringify(dirs);

  useEffect(() => {
    let cancelled = false;
    setStatuses([]);
    if (!dirs.length) return;
    const timer = setTimeout(() => {
      settingsApi.inspectClaudeScanDirectories(dirs, primary).then(
        (result) => {
          if (!cancelled) {
            setStatuses(result);
            setError(undefined);
          }
        },
        (reason: unknown) => {
          if (!cancelled) setError(String(reason));
        },
      );
    }, 250);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
    // A stable value key avoids re-inspecting after unrelated form updates.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [directoryKey, primary]);

  const add = async (path: string) => {
    const value = path.trim();
    if (!value) return;
    try {
      const current = latest.current;
      const result = await settingsApi.inspectClaudeScanDirectories(
        [...current.dirs, value],
        primary,
      );
      const status = result.at(-1)?.status;
      if (status === "invalid" || status === "duplicate") {
        setError(t(`settings.claudeScan.status.${status}`));
        return;
      }
      // Re-read after the async check so adding/removing another row is not lost.
      if (!latest.current.dirs.includes(value))
        latest.current.onChange([...latest.current.dirs, value]);
      setDraft("");
      setError(undefined);
    } catch (reason) {
      setError(String(reason));
    }
  };

  return (
    <div className="space-y-2 pl-3 border-l border-border/60">
      <p className="text-xs font-medium">{t("settings.claudeScan.title")}</p>
      <p className="text-xs text-muted-foreground">
        {t("settings.claudeScan.description")}
      </p>
      {dirs.map((dir, index) => (
        <div key={`${index}:${dir}`} className="flex items-center gap-2">
          <span className="min-w-0 flex-1 truncate text-xs" title={dir}>
            {dir}
          </span>
          <span className="text-xs text-muted-foreground">
            {statuses[index]?.path === dir.trim()
              ? t(`settings.claudeScan.status.${statuses[index].status}`)
              : ""}
          </span>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            aria-label={t("settings.claudeScan.remove", { path: dir })}
            onClick={() => onChange(dirs.filter((_, i) => i !== index))}
          >
            <X className="h-3.5 w-3.5" />
          </Button>
        </div>
      ))}
      <div className="flex items-center gap-2">
        <Input
          className="text-xs"
          value={draft}
          aria-label={t("settings.claudeScan.title")}
          placeholder={t("settings.claudeScan.placeholder")}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              void add(draft);
            }
          }}
        />
        <Button
          type="button"
          variant="outline"
          size="icon"
          disabled={browsing}
          title={t("settings.browseDirectory")}
          aria-label={t("settings.browseDirectory")}
          onClick={async () => {
            setBrowsing(true);
            try {
              const selected = await settingsApi.pickDirectory(
                draft || undefined,
              );
              if (selected) await add(selected);
            } catch (reason) {
              setError(String(reason));
            } finally {
              setBrowsing(false);
            }
          }}
        >
          <FolderSearch className="h-4 w-4" />
        </Button>
        <Button
          type="button"
          variant="outline"
          size="icon"
          disabled={!draft.trim()}
          aria-label={t("settings.claudeScan.add")}
          title={t("settings.claudeScan.add")}
          onClick={() => void add(draft)}
        >
          <Plus className="h-4 w-4" />
        </Button>
      </div>
      {error && (
        <p role="alert" className="text-xs text-destructive">
          {error}
        </p>
      )}
    </div>
  );
}
