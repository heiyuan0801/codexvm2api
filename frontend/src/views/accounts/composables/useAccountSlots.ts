import type { Ref } from 'vue'
import type { ContainerSlot } from '@/api/modules/containers'
import { useIntervalFn } from '@vueuse/core'
import { shallowRef, watch } from 'vue'
import { getContainers } from '@/api/modules/containers'
import { useRequestState } from '@/composables/useRequestState'

export function useAccountSlots(accountIds: Ref<string[]>) {
  const items = shallowRef<Record<string, ContainerSlot | null>>({})
  const globalEnabled = shallowRef(false)
  const request = useRequestState()

  async function refresh() {
    const id = request.start()
    try {
      const ids = accountIds.value
      if (!ids.length) {
        items.value = {}
        return
      }
      const result = await getContainers({ signal: request.signal, silent: true })
      if (request.isCurrent(id)) {
        const bindings = new Map(result.items.map(item => [item.accountId, item]))
        // 只有查询成功后才将没有绑定的账号记为 null，避免把读取中或失败误报为未绑定。
        items.value = Object.fromEntries(ids.map(accountId => [accountId, bindings.get(accountId) ?? null]))
        globalEnabled.value = result.globalEnabled
      }
    }
    catch (error) {
      if (request.isCurrent(id))
        items.value = {}
      request.fail(id, error)
    }
    finally {
      request.finish(id)
    }
  }

  watch(accountIds, () => {
    items.value = {}
    void refresh()
  }, { immediate: true })
  useIntervalFn(() => {
    if (!document.hidden && !request.loading.value)
      void refresh()
  }, 5000)

  return { items, globalEnabled, refresh, loading: request.loading, error: request.error }
}
