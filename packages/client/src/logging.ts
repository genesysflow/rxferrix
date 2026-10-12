export interface Logger {
  /** Protocol-level tracing (enabled by `verbose`). */
  debug(...args: unknown[]): void;
  /** Connection diagnostics (enabled by `reportDebugInfoToConsole` or `verbose`). */
  info(...args: unknown[]): void;
  warn(...args: unknown[]): void;
  error(...args: unknown[]): void;
  /** Server-side `logLines` forwarded from a function execution. */
  logLines(kind: string, udfPath: string, lines: string[] | undefined): void;
}

const PREFIX = "[rxferrix]";

export function createLogger(options: { verbose: boolean; reportDebugInfoToConsole: boolean }): Logger {
  const noop = () => {};
  return {
    debug: options.verbose ? (...args) => console.debug(PREFIX, ...args) : noop,
    info: options.verbose || options.reportDebugInfoToConsole ? (...args) => console.info(PREFIX, ...args) : noop,
    warn: (...args) => console.warn(PREFIX, ...args),
    error: (...args) => console.error(PREFIX, ...args),
    logLines: (kind, udfPath, lines) => {
      if (!lines || lines.length === 0) return;
      for (const line of lines) console.log(`${PREFIX} [${kind} ${udfPath}]`, line);
    },
  };
}
