export type ThemePreference = "system" | "light" | "dark";

const THEME_STORAGE_KEY = "wb-switch.theme";
const LEGACY_DARK_STORAGE_KEY = "wb-switch.dark";

function isThemePreference(value: string | null): value is ThemePreference {
  return value === "system" || value === "light" || value === "dark";
}

export function getThemePreference(): ThemePreference {
  try {
    const stored = localStorage.getItem(THEME_STORAGE_KEY);
    if (isThemePreference(stored)) return stored;

    const legacyDark = localStorage.getItem(LEGACY_DARK_STORAGE_KEY);
    if (legacyDark === "1") return "dark";
    if (legacyDark === "0") return "light";
  } catch {
    // 存储不可用时仍可跟随系统主题。
  }

  // 默认深色：本应用的深色主题是专门设计过的"控制台"配色
  // （青主色 + 蓝黑底 + 网格底纹），浅色只是可用状态。
  // 跟随系统会让大多数人在白天看到一个并非设计重点的界面。
  // 用户仍可在设置里显式选择 system / light。
  return "dark";
}

export function applyTheme(preference: ThemePreference) {
  const dark =
    preference === "dark" ||
    (preference === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
}

export function setThemePreference(preference: ThemePreference) {
  try {
    localStorage.setItem(THEME_STORAGE_KEY, preference);
    localStorage.removeItem(LEGACY_DARK_STORAGE_KEY);
  } catch {
    // 存储不可用时仍立即应用当前选择。
  }
  applyTheme(preference);
}

export function watchSystemTheme() {
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const onChange = () => {
    if (getThemePreference() === "system") applyTheme("system");
  };

  media.addEventListener("change", onChange);
  return () => media.removeEventListener("change", onChange);
}
