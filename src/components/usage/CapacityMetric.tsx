import { cn } from "@/lib/utils";

interface CapacityMetricProps {
  label: string;
  value: string;
  title?: string;
  emphasized?: boolean;
}

export function CapacityMetric({
  label,
  value,
  title,
  emphasized = false,
}: CapacityMetricProps) {
  return (
    <div
      className={cn(
        "min-w-0 rounded-xl border border-border/50 bg-background/45 p-3.5 shadow-sm",
        emphasized &&
          "border-emerald-500/20 bg-emerald-500/[0.045] dark:bg-emerald-500/[0.07]",
      )}
    >
      <div className="mb-1.5 text-[11px] font-medium leading-snug text-muted-foreground">
        {label}
      </div>
      <div
        className={cn(
          "truncate text-lg font-bold tabular-nums tracking-tight",
          emphasized && "text-emerald-600 dark:text-emerald-400",
        )}
        title={title ?? value}
      >
        {value}
      </div>
    </div>
  );
}
