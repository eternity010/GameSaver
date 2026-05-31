<script setup lang="ts">
defineProps<{
  open: boolean;
  title: string;
  message: string;
  confirmText: string;
  cancelText: string;
  danger: boolean;
}>();

const emit = defineEmits<{
  (e: "resolve", value: boolean): void;
}>();
</script>

<template>
  <div v-if="open" class="modal-overlay" role="dialog" aria-modal="true">
    <section class="modal">
      <h3>{{ title }}</h3>
      <p>{{ message }}</p>
      <div class="row modal-actions">
        <button type="button" @click="emit('resolve', false)">{{ cancelText }}</button>
        <button
          type="button"
          :class="danger ? 'danger' : 'primary'"
          @click="emit('resolve', true)"
        >
          {{ confirmText }}
        </button>
      </div>
    </section>
  </div>
</template>
