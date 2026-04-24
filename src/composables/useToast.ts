import { ref, onUnmounted } from "vue";

export type ToastLevel = "success" | "error" | "info";

export function useToast() {
  const toast = ref<{ visible: boolean; message: string; level: ToastLevel }>({
    visible: false,
    message: "",
    level: "info",
  });

  let toastTimer: ReturnType<typeof setTimeout> | null = null;

  function showToast(message: string, level: ToastLevel = "info", timeoutMs = 2600) {
    if (toastTimer) {
      clearTimeout(toastTimer);
      toastTimer = null;
    }
    toast.value = { visible: true, message, level };
    toastTimer = setTimeout(() => {
      toast.value.visible = false;
      toastTimer = null;
    }, timeoutMs);
  }

  function closeToast() {
    if (toastTimer) {
      clearTimeout(toastTimer);
      toastTimer = null;
    }
    toast.value.visible = false;
  }

  onUnmounted(() => {
    if (toastTimer) {
      clearTimeout(toastTimer);
      toastTimer = null;
    }
  });

  return { toast, showToast, closeToast };
}
