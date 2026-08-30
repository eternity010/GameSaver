import { reportFrontendError, type FrontendErrorReport } from "./api";

let installed = false;

export function installGlobalErrorReporting() {
  if (installed) return;
  installed = true;

  window.addEventListener("error", (event) => {
    const error = event.error;
    send({
      source: "window_error",
      message: event.message || errorMessage(error),
      stack: error instanceof Error ? error.stack : undefined,
      url: event.filename || undefined,
      line: event.lineno || undefined,
      column: event.colno || undefined,
    });
  });

  window.addEventListener("unhandledrejection", (event) => {
    const reason = event.reason;
    send({
      source: "unhandled_rejection",
      message: errorMessage(reason),
      stack: reason instanceof Error ? reason.stack : undefined,
    });
  });
}

function send(report: FrontendErrorReport) {
  void reportFrontendError(report).catch(() => undefined);
}

function errorMessage(reason: unknown): string {
  if (reason instanceof Error) return reason.message;
  if (typeof reason === "string") return reason;
  try {
    return JSON.stringify(reason);
  } catch {
    return String(reason);
  }
}
