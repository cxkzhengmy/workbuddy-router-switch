// 更新源必须与 Rust 侧 `update::GITHUB_OWNER/REPO` 一致：这里只用于界面展示
// 与「打开 Release 页面」，真正的检查在 Rust 侧（`update_service`）。
// 指错仓库的话，用户点「打开发布页」会进到别人（且没有内嵌网关）的项目。
export const GITHUB_OWNER = "cxkzhengmy";
export const GITHUB_REPO = "workbuddy-router-switch";
export const GITHUB_REPOSITORY_URL = `https://github.com/${GITHUB_OWNER}/${GITHUB_REPO}`;
export const GITHUB_RELEASE_URL = `${GITHUB_REPOSITORY_URL}/releases/latest`;

/** 在桌面端通过 Tauri opener 打开，在 webui 端打开新标签页。 */
export async function openReleaseUrl(url = GITHUB_RELEASE_URL): Promise<void> {
  const isWebui = typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);
  if (isWebui) {
    window.open(url, "_blank", "noopener,noreferrer");
    return;
  }
  const { openUrl } = await import("@tauri-apps/plugin-opener");
  await openUrl(url);
}
