import { createApp } from "vue";
import App from "./App.vue";
import { installGlobalErrorReporting } from "./error_reporting";
import "./style.css";

installGlobalErrorReporting();
try {
  createApp(App).mount("#app");
} catch (error) {
  window.dispatchEvent(new ErrorEvent("error", { error, message: error instanceof Error ? error.message : String(error) }));
  throw error;
}
