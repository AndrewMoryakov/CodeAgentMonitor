import { useEffect, useRef } from "react";
import type { BackendMode, WorkspaceInfo } from "../../../types";

const INITIAL_THREAD_LIST_MAX_PAGES = 6;

type WorkspaceRestoreOptions = {
  workspaces: WorkspaceInfo[];
  hasLoaded: boolean;
  connectWorkspace: (workspace: WorkspaceInfo) => Promise<void>;
  listThreadsForWorkspaces: (
    workspaces: WorkspaceInfo[],
    options?: { preserveState?: boolean; maxPages?: number },
  ) => Promise<void>;
  backendMode?: BackendMode;
};

export function useWorkspaceRestore({
  workspaces,
  hasLoaded,
  connectWorkspace,
  listThreadsForWorkspaces,
  backendMode = "local",
}: WorkspaceRestoreOptions) {
  const restoredWorkspaces = useRef(new Set<string>());

  useEffect(() => {
    if (!hasLoaded) {
      return;
    }
    const pending = workspaces.filter(
      (workspace) => !restoredWorkspaces.current.has(workspace.id),
    );
    if (pending.length === 0) {
      return;
    }
    pending.forEach((workspace) => {
      restoredWorkspaces.current.add(workspace.id);
    });
    void (async () => {
      const connectedTargets: WorkspaceInfo[] = [];
      for (const workspace of pending) {
        try {
          // Always connect — on app startup backend sessions are empty
          // even if the workspace was "connected" in the previous session.
          await connectWorkspace(workspace);
          connectedTargets.push({ ...workspace, connected: true });
        } catch {
          // Silent: connection errors show in debug panel.
        }
      }
      if (connectedTargets.length === 0) {
        return;
      }
      if (backendMode === "claude") {
        // In Claude mode each workspace has its own session with its own
        // history, so we must list threads per-workspace individually.
        for (const ws of connectedTargets) {
          await listThreadsForWorkspaces([ws], {
            maxPages: INITIAL_THREAD_LIST_MAX_PAGES,
          });
        }
      } else {
        await listThreadsForWorkspaces(connectedTargets, {
          maxPages: INITIAL_THREAD_LIST_MAX_PAGES,
        });
      }
    })();
  }, [backendMode, connectWorkspace, hasLoaded, listThreadsForWorkspaces, workspaces]);
}
