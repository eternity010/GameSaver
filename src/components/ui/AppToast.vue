<script setup lang="ts">
import type { ToastLevel } from "../../composables/useToast";

defineProps<{
  visible: boolean;
  message: string;
  level: ToastLevel;
}>();

const emit = defineEmits<{
  (e: "close"): void;
}>();
</script>

<template>
  <transition name="toast-fade">
    <div v-if="visible" class="toast" :class="level" role="status" aria-live="polite">
      <span>{{ message }}</span>
      <button type="button" class="toast-close" @click="emit('close')">关闭</button>
    </div>
  </transition>
</template>
