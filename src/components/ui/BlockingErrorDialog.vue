<script setup lang="ts">
defineProps<{
  message: string;
}>();

const emit = defineEmits<{
  (e: "close"): void;
}>();
</script>

<template>
  <div v-if="message" class="modal-overlay" role="dialog" aria-modal="true">
    <section class="modal blocking-modal">
      <h3>操作被阻止</h3>
      <p>{{ message }}</p>
      <div class="row modal-actions">
        <button type="button" class="primary" @click="emit('close')">我知道了</button>
      </div>
    </section>
  </div>
</template>
