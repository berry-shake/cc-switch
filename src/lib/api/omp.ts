import { invoke } from "@tauri-apps/api/core";
import type { UsageScript } from "@/types";
import type { PiCurrentState, PiSessionDiscovery } from "./pi";

export const ompApi = {
  async getCurrentState(): Promise<PiCurrentState> {
    return await invoke("get_omp_current_state");
  },

  async updateProviderUsageScript(
    id: string,
    usageScript: UsageScript,
  ): Promise<boolean> {
    return await invoke("update_omp_provider_usage_script", {
      id,
      usageScript,
    });
  },

  async getSessionDiscovery(): Promise<PiSessionDiscovery> {
    return await invoke("get_omp_session_discovery");
  },
};
