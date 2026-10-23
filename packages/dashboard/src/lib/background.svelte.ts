/**
 * Whether the views of the execution log (the Overview's function charts and tables, a function's activity, Logs)
 * leave out background work: scheduled jobs, crons and workflow steps, and the functions they call. What is left is
 * what clients ran. Stored per browser, shared by every view.
 */
const KEY = "rxferrix.dashboard.hideBackground";

function load(): boolean {
  try {
    return localStorage.getItem(KEY) === "1";
  } catch {
    // storage unavailable
    return false;
  }
}

class BackgroundState {
  #hidden = $state(load());

  get hidden(): boolean {
    return this.#hidden;
  }

  set hidden(hidden: boolean) {
    this.#hidden = hidden;
    try {
      if (hidden) localStorage.setItem(KEY, "1");
      else localStorage.removeItem(KEY);
    } catch {
      // storage unavailable
    }
  }
}

export const background = new BackgroundState();
