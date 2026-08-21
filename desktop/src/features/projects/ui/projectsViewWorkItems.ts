import * as React from "react";

import type { ProjectsWorkItemsResult } from "@/features/projects/projectWorkItems";
import type { Project } from "@/features/projects/hooks";

// Split from ProjectsView.tsx to keep that file under the per-file line cap.

/**
 * Stable empty fallback: a fresh `[]` per render would defeat the memoized
 * activity feed while work items load.
 */
export const EMPTY_ITEMS: never[] = [];

/**
 * Memoized flat issue/PR arrays for the overview context panel. Fresh
 * `.map()` arrays per render would change the panel's memo deps every
 * render, re-walking every issue and pull request in the community on
 * unrelated Projects-view state changes.
 */
export function useContextWorkItems(
  workItemsData: ProjectsWorkItemsResult<Project> | undefined,
) {
  const contextIssues = React.useMemo(
    () => workItemsData?.issues.items.map(({ issue }) => issue) ?? [],
    [workItemsData],
  );
  const contextPullRequests = React.useMemo(
    () =>
      workItemsData?.pullRequests.items.map(({ pullRequest }) => pullRequest) ??
      [],
    [workItemsData],
  );
  return { contextIssues, contextPullRequests };
}
