import { onUnmounted, ref } from "vue";

export type ConfirmDialogState = {
  open: boolean;
  title: string;
  message: string;
  confirmText: string;
  cancelText: string;
  danger: boolean;
};

export type ConfirmDialogOptions = {
  title: string;
  message: string;
  confirmText?: string;
  cancelText?: string;
  danger?: boolean;
};

export function useConfirmDialog() {
  const confirmDialog = ref<ConfirmDialogState>({
    open: false,
    title: "",
    message: "",
    confirmText: "确认",
    cancelText: "取消",
    danger: false,
  });

  let confirmResolver: ((value: boolean) => void) | null = null;

  function askConfirm(options: ConfirmDialogOptions) {
    return new Promise<boolean>((resolve) => {
      confirmResolver = resolve;
      confirmDialog.value = {
        open: true,
        title: options.title,
        message: options.message,
        confirmText: options.confirmText ?? "确认",
        cancelText: options.cancelText ?? "取消",
        danger: options.danger ?? false,
      };
    });
  }

  function resolveConfirm(result: boolean) {
    const resolver = confirmResolver;
    confirmResolver = null;
    confirmDialog.value.open = false;
    if (resolver) {
      resolver(result);
    }
  }

  onUnmounted(() => {
    confirmResolver = null;
  });

  return {
    confirmDialog,
    askConfirm,
    resolveConfirm,
  };
}
