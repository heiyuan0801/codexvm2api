<script setup lang="ts">
import type { ContainerSlot } from '@/api/modules/containers'
import { computed } from 'vue'
import BaseTag from '@/components/base/BaseTag.vue'

const props = defineProps<{ container?: ContainerSlot | null, error?: string }>()
const labels: Record<string, string> = {
  'not-created': '尚未启动',
  'stopped': '已停止',
  'stopping': '停止中',
  'global-disabled': '功能未开启',
  'starting': '启动中',
  'ready': '运行中',
  'degraded': '运行异常',
  'deleting': '删除中',
  'delete-failed': '清理失败',
  'unknown': '状态待确认',
}
const label = computed(() => {
  if (props.error)
    return '读取失败'
  if (props.container === undefined)
    return '读取中'
  if (props.container === null)
    return '未绑定'
  return labels[props.container.state] ?? '状态待确认'
})
const tone = computed(() => {
  if (props.error || props.container?.state === 'degraded' || props.container?.state === 'delete-failed')
    return 'danger'
  if (props.container?.state === 'ready')
    return 'success'
  if (props.container?.state === 'starting' || props.container?.state === 'stopping' || props.container?.state === 'deleting')
    return 'warning'
  return 'neutral'
})
</script>

<template>
  <div class="flex min-w-0 flex-col items-start gap-1">
    <span v-if="container && !error" class="max-w-full truncate text-cp-sm text-cp-text" :title="container.name">{{ container.name }}</span>
    <BaseTag :type="tone" :title="error || container?.reason || undefined" role="status">
      {{ label }}
    </BaseTag>
  </div>
</template>
