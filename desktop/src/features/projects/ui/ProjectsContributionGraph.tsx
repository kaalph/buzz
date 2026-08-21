import * as React from "react";

import { useNow } from "@/shared/lib/useNow";

import { cn } from "@/shared/lib/cn";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/shared/ui/tooltip";

const WEEK_COUNT = 26;
const DAYS_PER_WEEK = 7;

// Intensity ramp shared by the cells and the legend dots.
// Tints of the theme's primary token: the most active cells are fully
// saturated primary, quieter days fade toward the muted background.
const LEVEL_CLASSES = [
  "bg-muted/40 dark:bg-muted/30",
  "bg-primary/25",
  "bg-primary/50",
  "bg-primary/75",
  "bg-primary",
];

// Descriptors matching the thresholds in `levelFor`.
const LEVEL_LABELS = [
  "No activity",
  "1–2 events",
  "3–5 events",
  "6–9 events",
  "10+ events",
];

/** Activity intensity legend shared with the contribution graph header. */
export function ProjectsContributionLegend() {
  return (
    <div className="flex items-center gap-1.5">
      {LEVEL_CLASSES.map((levelClass, level) => (
        <Tooltip key={levelClass}>
          <TooltipTrigger asChild>
            <span className={cn("h-2.5 w-2.5 rounded", levelClass)} />
          </TooltipTrigger>
          <TooltipContent>{LEVEL_LABELS[level]}</TooltipContent>
        </Tooltip>
      ))}
    </div>
  );
}

function dayKeyOf(date: Date) {
  const month = `${date.getMonth() + 1}`.padStart(2, "0");
  const day = `${date.getDate()}`.padStart(2, "0");
  return `${date.getFullYear()}-${month}-${day}`;
}

function levelFor(count: number) {
  if (count <= 0) return 0;
  if (count <= 2) return 1;
  if (count <= 5) return 2;
  if (count <= 9) return 3;
  return 4;
}

/** Week columns (each 7 days, Sunday first) ending with the current week. */
function buildWeeks(today: Date, weekCount: number) {
  const start = new Date(
    today.getFullYear(),
    today.getMonth(),
    today.getDate() - today.getDay() - (weekCount - 1) * DAYS_PER_WEEK,
  );

  return Array.from({ length: weekCount }, (_, weekIndex) =>
    Array.from({ length: DAYS_PER_WEEK }, (_, dayIndex) => {
      const date = new Date(start);
      date.setDate(start.getDate() + weekIndex * DAYS_PER_WEEK + dayIndex);
      return date;
    }),
  );
}

// Minimum columns between labels so adjacent months never overlap.
const MIN_LABEL_GAP = 3;

function monthLabels(weeks: Date[][]) {
  let lastLabeledIndex = -MIN_LABEL_GAP;
  return weeks.map((week, index) => {
    const isNewMonth =
      index === 0 || week[0].getMonth() !== weeks[index - 1][0].getMonth();
    if (!isNewMonth || index - lastLabeledIndex < MIN_LABEL_GAP) return "";
    lastLabeledIndex = index;
    return week[0].toLocaleDateString(undefined, { month: "short" });
  });
}

type ContributionCell = {
  key: string;
  isFuture: boolean;
  levelClass: string;
  label: string;
};

/**
 * GitHub-style activity heatmap for the last 26 weeks, fed by per-day
 * activity counts (see `ProjectActivitySummary.activityByDay`).
 *
 * Memoized and deliberately lightweight: an earlier version wrapped each of
 * the ~180 day cells in a Radix tooltip and re-derived every cell (including
 * per-cell Intl date formatting) on every parent render — the graph sits in
 * the always-visible context rail, so that cost taxed every Projects tab.
 * Cells are precomputed once per data change and hover details use native
 * `title` tooltips.
 */
export const ProjectsContributionGraph = React.memo(
  function ProjectsContributionGraph({
    activityByDay,
    className,
    compact = false,
  }: {
    activityByDay: Record<string, number>;
    className?: string;
    compact?: boolean;
  }) {
    // Day-granular key so the memo only re-derives when the calendar day (or
    // the data) changes. Ticked via useNow because React.memo otherwise
    // freezes the component past midnight: with stable props it never
    // re-renders, so a render-time `new Date()` alone cannot advance the day.
    const now = useNow(60_000);
    const todayKey = dayKeyOf(new Date(now));
    const { labels, cells, columns } = React.useMemo(() => {
      const weeks = buildWeeks(new Date(), compact ? 18 : WEEK_COUNT);
      const dayCells: ContributionCell[] = weeks.flat().map((day) => {
        const key = dayKeyOf(day);
        if (key > todayKey) {
          return { key, isFuture: true, levelClass: "", label: "" };
        }
        const count = activityByDay[key] ?? 0;
        const dateLabel = day.toLocaleDateString(undefined, {
          month: "short",
          day: "numeric",
        });
        return {
          key,
          isFuture: false,
          levelClass: LEVEL_CLASSES[levelFor(count)],
          label:
            count > 0
              ? `${count} ${count === 1 ? "event" : "events"} · ${dateLabel}`
              : `No activity · ${dateLabel}`,
        };
      });
      return {
        labels: monthLabels(weeks),
        cells: dayCells,
        columns: weeks.length,
      };
    }, [activityByDay, compact, todayKey]);
    const gridTemplateColumns = `repeat(${columns}, minmax(0, 1fr))`;

    return (
      <div className={cn("space-y-2", className)}>
        <div
          className={cn("grid", compact ? "gap-1" : "gap-2")}
          style={{ gridTemplateColumns }}
        >
          {labels.map((label, index) => (
            <span
              className="overflow-visible whitespace-nowrap text-2xs font-medium text-muted-foreground"
              // Columns are positional; labels have no stable content key.
              // biome-ignore lint/suspicious/noArrayIndexKey: fixed-size grid
              key={index}
            >
              {label}
            </span>
          ))}
        </div>
        <div
          className={cn(
            "grid grid-flow-col grid-rows-7",
            compact ? "gap-1" : "gap-2",
          )}
          style={{ gridTemplateColumns }}
        >
          {cells.map((cell) =>
            cell.isFuture ? (
              <span
                aria-hidden
                className="aspect-square rounded-[22%] border border-border/40 dark:border-border/30"
                key={cell.key}
              />
            ) : (
              <span
                aria-label={cell.label}
                className={cn(
                  "aspect-square w-full rounded-[22%]",
                  cell.levelClass,
                )}
                key={cell.key}
                role="img"
                title={cell.label}
              />
            ),
          )}
        </div>
      </div>
    );
  },
);
