<template>
  <div class="mention-list" v-if="items.length">
    <button
      v-for="(item, index) in items"
      :key="item.id"
      class="mention-item"
      :class="{ 'is-selected': index === selectedIndex }"
      @click="selectItem(index)"
    >
      <v-avatar size="24" color="primary" class="mr-2">
        <span class="text-caption">{{ initial(item) }}</span>
      </v-avatar>
      <span class="mention-name">{{ item.display_name || item.username }}</span>
    </button>
  </div>
</template>

<script setup lang="ts">
import { ref, watch } from 'vue'

export interface MentionItem {
  id: string
  user_id?: string
  username?: string
  display_name: string
  avatar?: string
}

const props = defineProps<{
  items: MentionItem[]
  command: (item: { id: string; label: string }) => void
}>()

const selectedIndex = ref(0)

watch(
  () => props.items,
  () => { selectedIndex.value = 0 },
)

function initial(item: MentionItem) {
  const name = item.display_name || item.username || '?'
  return name.charAt(0).toUpperCase()
}

function selectItem(index: number) {
  const item = props.items[index]
  if (item) {
    props.command({
      id: item.user_id || item.id,
      label: item.display_name || item.username || item.id,
    })
  }
}

function onKeyDown(event: { event: KeyboardEvent }): boolean {
  if (event.event.key === 'ArrowUp') {
    selectedIndex.value = (selectedIndex.value + props.items.length - 1) % props.items.length
    return true
  }
  if (event.event.key === 'ArrowDown') {
    selectedIndex.value = (selectedIndex.value + 1) % props.items.length
    return true
  }
  if (event.event.key === 'Enter') {
    selectItem(selectedIndex.value)
    return true
  }
  return false
}

defineExpose({ onKeyDown })
</script>

<style scoped>
.mention-list {
  background: rgb(var(--v-theme-surface));
  border: 1px solid rgba(var(--v-theme-on-surface), 0.12);
  border-radius: 8px;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.15);
  padding: 4px;
  max-height: 200px;
  overflow-y: auto;
}

.mention-item {
  display: flex;
  align-items: center;
  width: 100%;
  padding: 6px 8px;
  border: none;
  border-radius: 4px;
  background: transparent;
  color: rgb(var(--v-theme-on-surface));
  cursor: pointer;
  font-size: 0.875rem;
}

.mention-item:hover,
.mention-item.is-selected {
  background: rgba(var(--v-theme-primary), 0.12);
}

.mention-name {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
</style>
