/** Light / dark / system theme, stored per browser. Applied as `data-theme` on <html> (see app.css). */
export type Theme = "light" | "dark" | "system";

const KEY = "ferrix.dashboard.theme";

function load(): Theme {
  try {
    const t = localStorage.getItem(KEY);
    if (t === "light" || t === "dark" || t === "system") return t;
  } catch {
    // storage unavailable
  }
  return "system";
}

class ThemeState {
  current = $state<Theme>(load());

  apply() {
    const t = this.current;
    if (t === "system") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.setAttribute("data-theme", t);
  }

  cycle() {
    this.current = this.current === "system" ? "dark" : this.current === "dark" ? "light" : "system";
    try {
      localStorage.setItem(KEY, this.current);
    } catch {
      // storage unavailable
    }
    this.apply();
  }
}

export const theme = new ThemeState();
