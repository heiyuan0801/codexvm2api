<script setup lang="ts">
import type { Account, OutboundProxyRecord } from '@/api'
import type { ContainerCommand, ContainerSlot } from '@/api/modules/containers'
import { Plus, RefreshCw } from '@lucide/vue'
import { useDocumentVisibility } from '@vueuse/core'
import { computed, onMounted, onScopeDispose, ref } from 'vue'
import { getAccounts, getProxies } from '@/api'
import { getContainers, updateContainer } from '@/api/modules/containers'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseModal from '@/components/base/BaseModal/index.vue'
import BasePageHeader from '@/components/base/BasePageHeader.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import { defineTableColumns } from '@/components/base/BaseTable/columns'
import BaseTable from '@/components/base/BaseTable/index.vue'
import BaseTag from '@/components/base/BaseTag.vue'
import { toast } from '@/components/base/BaseToast'
import { useRequestState } from '@/composables/useRequestState'

const slots = ref<ContainerSlot[]>([])
const globalEnabled = ref(false)
const accounts = ref<Account[]>([])
const proxies = ref<OutboundProxyRecord[]>([])
const query = useRequestState()
const catalog = useRequestState()
const mutation = useRequestState()
const { loading, error } = query
const { loading: busy, error: mutationError } = mutation
const visibility = useDocumentVisibility()
const createOpen = ref(false)
const configureOpen = ref(false)
const deleteTarget = ref<ContainerSlot | null>(null)
const deleteOpen = ref(false)
const name = ref('')
const selectedId = ref('')
const draftGeneration = ref(0)
const proxyId = ref('')
const accountId = ref('')
const selected = computed(() => slots.value.find(slot => slot.id === selectedId.value))
const proxyOptions = computed(() => [{ label: '暂不配置代理', value: '' }, ...proxies.value.map(proxy => ({ label: `${proxy.name} · ${proxy.endpoint}`, value: proxy.id }))])
const oauthAccounts = computed(() => accounts.value.filter(account => account.authenticationKind === 'oauth'))
const accountOptions = computed(() => [{ label: selected.value?.accountId ? '解除当前账号绑定' : '暂不绑定账号', value: '', disabled: false }, ...oauthAccounts.value.map((account) => {
  const bound = slots.value.find(slot => slot.accountId === account.id)
  const binding = bound ? bound.id === selectedId.value ? '当前容器' : `已绑定：${bound.name}` : '未绑定'
  return {
    label: account.name,
    description: `${binding}${account.enabled ? '' : '，已停用'}`,
    value: account.id,
    disabled: (!!bound && bound.id !== selectedId.value) || (!selected.value?.proxyId && account.id !== selected.value?.accountId),
  }
})])
const accountAvailability = computed(() => {
  if (!selected.value?.proxyId)
    return ''
  if (!oauthAccounts.value.length)
    return '还没有 OpenAI OAuth 账号，请先前往账号管理导入或授权'
  if (accountOptions.value.slice(1).every(option => option.disabled))
    return '现有账号均已绑定其他容器，更换绑定需先在原容器停止并解绑'
  if (accountOptions.value.some(option => option.disabled))
    return '灰色账号已绑定其他容器，更换绑定需先在原容器停止并解绑'
  return ''
})
const columns = defineTableColumns<ContainerSlot>([
  { key: 'name', label: '容器名称', kind: 'identity', size: 'lg' },
  { key: 'proxy', label: '代理', kind: 'custom', size: 'lg' },
  { key: 'account', label: '绑定账号', kind: 'custom', size: 'lg' },
  { key: 'state', label: '运行状态', kind: 'custom', size: 'lg' },
  { key: 'actions', label: '操作', kind: 'actions', size: 'lg' },
])
const labels: Record<string, string> = { 'deleting': '删除中', 'delete-failed': '清理失败', 'not-created': '尚未启动', 'stopped': '已停止', 'stopping': '停止中', 'starting': '启动中', 'ready': '运行中', 'degraded': '运行异常', 'global-disabled': '功能未开启', 'unknown': '状态待确认' }
function stateLabel(slot: ContainerSlot) {
  return labels[slot.state] ?? '状态待确认'
}
function deleting(slot: ContainerSlot) {
  return slot.state === 'deleting' || slot.state === 'delete-failed'
}
function requestDelete(slot: ContainerSlot) {
  deleteTarget.value = { ...slot }
  mutationError.value = ''
  deleteOpen.value = true
}
async function remove() {
  if (!deleteTarget.value)
    return
  if (await mutate({ action: 'delete', id: deleteTarget.value.id, expectedGeneration: deleteTarget.value.generation }))
    deleteOpen.value = false
}
function canStart(slot: ContainerSlot) {
  return !deleting(slot) && globalEnabled.value && !!slot.proxyId && !!slot.accountId && slot.accountEnabled && !slot.running
}

async function load(silent = false) {
  const id = query.start(silent)
  try {
    const data = await getContainers({ signal: query.signal })
    if (!query.isCurrent(id))
      return
    slots.value = data.items
    globalEnabled.value = data.globalEnabled
    error.value = ''
  }
  catch (cause) { query.fail(id, cause) }
  finally { query.finish(id) }
}
async function loadCatalog() {
  const id = catalog.start()
  try {
    const accountItems: Account[] = []
    const proxyItems: OutboundProxyRecord[] = []
    for (let page = 1; ; page++) {
      const data = await getAccounts({ provider: 'openai', page, pageSize: 200 }, { signal: catalog.signal })
      if (!catalog.isCurrent(id))
        return
      accountItems.push(...data.items)
      if (page >= data.page.totalPages)
        break
    }
    for (let page = 1; ; page++) {
      const data = await getProxies({ page, pageSize: 200 }, { signal: catalog.signal })
      if (!catalog.isCurrent(id))
        return
      proxyItems.push(...data.items)
      if (page >= data.page.totalPages)
        break
    }
    accounts.value = accountItems
    proxies.value = proxyItems
  }
  catch (cause) { catalog.fail(id, cause) }
  finally { catalog.finish(id) }
}
function configure(slot: ContainerSlot) {
  selectedId.value = slot.id
  draftGeneration.value = slot.generation
  proxyId.value = slot.proxyId ?? ''
  accountId.value = slot.accountId ?? ''
  mutationError.value = ''
  configureOpen.value = true
  void loadCatalog()
}
async function mutate(command: ContainerCommand) {
  if (busy.value)
    return false
  const id = mutation.start()
  try {
    await updateContainer(command, { signal: mutation.signal })
    if (!mutation.isCurrent(id))
      return false
    await load(true)
    if (!mutation.isCurrent(id))
      return false
    toast.success(command.action === 'start' ? '已请求启动容器' : command.action === 'stop' ? '已请求停止容器' : command.action === 'delete' ? '已提交删除' : '已保存')
    return true
  }
  catch (cause) {
    mutation.fail(id, cause)
    if (mutation.isCurrent(id))
      await load(true)
    return false
  }
  finally { mutation.finish(id) }
}
async function create() {
  const previous = new Set(slots.value.map(slot => slot.id))
  if (await mutate({ action: 'create', name: name.value.trim() })) {
    createOpen.value = false
    name.value = ''
    const created = slots.value.find(slot => !previous.has(slot.id))
    if (created)
      configure(created)
  }
}
async function command(slot: ContainerSlot, action: 'start' | 'stop') {
  if (await mutate({ action, id: slot.id, expectedGeneration: slot.generation })) {
    if (selectedId.value === slot.id)
      draftGeneration.value = selected.value?.generation ?? 0
  }
}
async function saveProxy() {
  if (!selected.value)
    return
  if (await mutate({ action: 'configureProxy', id: selected.value.id, expectedGeneration: draftGeneration.value, proxyId: proxyId.value || null }))
    draftGeneration.value = selected.value?.generation ?? 0
}
async function bind() {
  if (!selected.value)
    return
  if (await mutate({ action: 'bind', id: selected.value.id, expectedGeneration: draftGeneration.value, accountId: accountId.value || null }))
    draftGeneration.value = selected.value?.generation ?? 0
}
onMounted(() => void load())
const timer = setInterval(() => {
  if (visibility.value === 'visible' && !busy.value && !loading.value)
    void load(true)
}, 5000)
onScopeDispose(() => clearInterval(timer))
</script>

<template>
  <div>
    <BasePageHeader title="容器管理" description="创建空槽，设置代理并绑定账号后启动" />
    <p v-if="!globalEnabled && !loading && !error" class="mt-4 text-cp-sm text-cp-warning-text" role="status">
      容器功能尚未在部署配置中开启，可先创建空槽并完成配置
    </p>
    <p v-if="error" class="mt-4 text-cp-sm text-cp-error" role="alert">
      {{ error }}
    </p>
    <p v-if="mutationError && !configureOpen && !createOpen && !deleteOpen" class="mt-4 text-cp-sm text-cp-error" role="alert">
      {{ mutationError }}
    </p>
    <BaseCard class="mt-5">
      <template #header>
        <div class="flex w-full flex-wrap items-center justify-between gap-3">
          <span class="text-cp-sm text-cp-text-secondary">{{ slots.length }} 个槽位</span>
          <div class="flex gap-2">
            <BaseButton variant="secondary" :disabled="busy" :loading="loading" @click="load()">
              <template #icon>
                <RefreshCw class="size-4" />
              </template>刷新
            </BaseButton>
            <BaseButton variant="primary" :disabled="busy || loading || !!error" @click="mutationError = ''; createOpen = true">
              <template #icon>
                <Plus class="size-4" />
              </template>创建容器
            </BaseButton>
          </div>
        </div>
      </template>
      <template #body>
        <BaseTable :columns="columns" :rows="slots" :loading="loading" empty-text="暂无容器，先创建一个空槽，无需提前添加账号">
          <template #name="{ row }">
            <span class="text-cp font-emphasis text-cp-text">{{ row.name }}</span>
          </template>
          <template #proxy="{ row }">
            <span :class="row.proxyId ? 'text-cp-text' : 'text-cp-text-tertiary'">{{ row.proxyName ?? '待配置代理' }}</span>
          </template>
          <template #account="{ row }">
            <span :class="row.accountId ? 'text-cp-text' : 'text-cp-text-tertiary'">{{ row.accountName ?? '待绑定账号' }}</span>
          </template>
          <template #state="{ row }">
            <div class="grid gap-1">
              <BaseTag :type="row.state === 'ready' ? 'success' : (row.state === 'degraded' || row.state === 'delete-failed') ? 'danger' : 'neutral'">
                {{ stateLabel(row) }}
              </BaseTag><span v-if="row.reason" class="text-cp-xs text-cp-text-tertiary">{{ row.reason }}</span>
            </div>
          </template>
          <template #actions="{ row }">
            <div class="flex flex-wrap gap-2">
              <BaseButton size="sm" variant="secondary" :disabled="busy || !!error || deleting(row)" @click="configure(row)">
                配置
              </BaseButton><BaseButton v-if="row.running" size="sm" variant="secondary" :disabled="busy || !!error" @click="command(row, 'stop')">
                停止
              </BaseButton><BaseButton v-else size="sm" variant="primary" :disabled="busy || !!error || !canStart(row)" @click="command(row, 'start')">
                启动
              </BaseButton><BaseButton size="sm" variant="destructive" :disabled="busy || !!error || deleting(row)" @click="requestDelete(row)">
                删除
              </BaseButton>
            </div>
          </template>
        </BaseTable>
      </template>
    </BaseCard>
    <BaseModal v-model="deleteOpen" title="删除容器" size="md" :dismissible="!busy">
      <div v-if="deleteTarget" class="grid gap-3 text-cp-sm text-cp-text-secondary">
        <p>确认删除「{{ deleteTarget.name }}」？容器、独立网络和数据卷中的全部数据将永久删除，代理记录和账号记录会保留</p>
        <p v-if="deleteTarget.running || deleteTarget.accountId" class="text-cp-warning-text">
          请先停止容器，再通过「配置」解绑账号后删除；解绑后的账号将恢复原有路由方式
        </p>
        <p v-if="mutationError" class="text-cp-error" role="alert">
          {{ mutationError }}
        </p>
      </div>
      <template #footer>
        <BaseButton variant="secondary" :disabled="busy" @click="deleteOpen = false">
          取消
        </BaseButton><BaseButton variant="destructive" :loading="busy" :disabled="!deleteTarget || deleteTarget.running || !!deleteTarget.accountId || !!error" @click="remove">
          永久删除
        </BaseButton>
      </template>
    </BaseModal>
    <BaseModal v-model="createOpen" title="创建容器" size="md" :dismissible="!busy">
      <BaseFormItem label="容器名称" required description="先创建空槽，代理和账号可稍后配置，启动时创建 Docker 容器">
        <BaseInput v-model="name" aria-label="容器名称" placeholder="例如 Codex 01" maxlength="100" :disabled="busy" @keyup.enter="name.trim() && create()" />
      </BaseFormItem>
      <p v-if="mutationError" class="text-cp-sm text-cp-error" role="alert">
        {{ mutationError }}
      </p>
      <template #footer>
        <BaseButton variant="secondary" :disabled="busy" @click="createOpen = false">
          取消
        </BaseButton><BaseButton variant="primary" :loading="busy" :disabled="!name.trim()" @click="create">
          创建空槽
        </BaseButton>
      </template>
    </BaseModal>
    <BaseModal v-model="configureOpen" :title="selected?.name ?? '配置容器'" size="md-wide" :dismissible="!busy">
      <div v-if="selected" class="grid gap-6">
        <p v-if="error" class="m-0 text-cp-sm text-cp-error" role="alert">
          {{ error }}
        </p>
        <p v-if="selected.running" class="m-0 text-cp-sm text-cp-text-secondary">
          容器已请求运行，修改代理或账号前请先停止
        </p>
        <p v-if="catalog.error.value" class="m-0 text-cp-sm text-cp-error" role="alert">
          {{ catalog.error.value }} <BaseButton size="sm" variant="secondary" @click="loadCatalog">
            重试
          </BaseButton>
        </p>
        <BaseFormItem label="1 · 设置代理 IP" description="选择代理管理中的出口，容器转发使用此代理">
          <div class="flex flex-col gap-2 sm:flex-row">
            <BaseSelect v-model="proxyId" class="min-w-0 flex-1" :options="proxyOptions" :disabled="busy || !!error || selected.running || catalog.loading.value || !!catalog.error.value" /><BaseButton variant="secondary" :disabled="busy || !!error || selected.running || catalog.loading.value || !!catalog.error.value || (proxyId || null) === selected.proxyId" @click="saveProxy">
              保存代理
            </BaseButton>
          </div>
          <RouterLink class="mt-2 inline-block text-cp-sm text-cp-primary" to="/proxies">
            前往代理管理添加或测试代理
          </RouterLink>
        </BaseFormItem>
        <BaseFormItem label="2 · 绑定账号" :description="selected.proxyId ? '已授权的 OpenAI OAuth 账号可直接选择，每个账号仅绑定一个容器' : selected.accountId ? '当前未配置代理，可先解绑账号，新绑定需先保存代理' : '请先保存代理，再绑定账号'">
          <div class="flex flex-col gap-2 sm:flex-row">
            <BaseSelect v-model="accountId" class="min-w-0 flex-1" :options="accountOptions" :disabled="busy || !!error || selected.running || (!selected.proxyId && !selected.accountId) || catalog.loading.value || !!catalog.error.value" /><BaseButton variant="secondary" :disabled="busy || !!error || selected.running || (!selected.proxyId && !!accountId) || catalog.loading.value || !!catalog.error.value || (accountId || null) === selected.accountId" @click="bind">
              {{ accountId ? '绑定账号' : '解绑账号' }}
            </BaseButton>
          </div>
          <p v-if="!catalog.loading.value && !catalog.error.value && accountAvailability" class="mt-2 mb-0 text-cp-sm text-cp-text-secondary" role="status">
            {{ accountAvailability }}
          </p>
          <div class="mt-2 flex flex-wrap items-center justify-between gap-2">
            <RouterLink class="text-cp-sm text-cp-primary" to="/accounts">
              前往账号管理查看绑定或授权账号
            </RouterLink>
            <BaseButton size="sm" variant="secondary" :disabled="busy" :loading="catalog.loading.value" @click="loadCatalog">
              刷新账号列表
            </BaseButton>
          </div>
        </BaseFormItem>
        <BaseFormItem label="3 · 启动容器">
          <div class="flex items-center justify-between gap-3">
            <span class="text-cp-sm text-cp-text-secondary">{{ selected.reason ?? stateLabel(selected) }}</span><BaseButton v-if="selected.running" variant="secondary" :loading="busy" @click="command(selected, 'stop')">
              停止容器
            </BaseButton><BaseButton v-else variant="primary" :loading="busy" :disabled="!canStart(selected) || !!error || (proxyId || null) !== selected.proxyId || (accountId || null) !== selected.accountId" @click="command(selected, 'start')">
              启动容器
            </BaseButton>
          </div>
          <p v-if="!globalEnabled" class="mt-2 text-cp-xs text-cp-warning-text">
            请先在部署配置中开启容器功能
          </p>
          <p v-else-if="selected.accountId && !selected.accountEnabled" class="mt-2 text-cp-xs text-cp-warning-text">
            绑定账号已停用，请先在账号管理中启用
          </p>
        </BaseFormItem>
        <div v-if="mutationError" class="grid gap-2">
          <p class="m-0 text-cp-sm text-cp-error" role="alert">
            {{ mutationError }}
          </p>
          <BaseButton variant="secondary" :disabled="busy" @click="configure(selected)">
            重新读取配置
          </BaseButton>
        </div>
      </div>
      <p v-else class="text-cp-sm text-cp-text-secondary">
        槽位不存在，请刷新列表
      </p>
      <template #footer>
        <BaseButton variant="secondary" :disabled="busy" @click="configureOpen = false">
          关闭
        </BaseButton>
      </template>
    </BaseModal>
  </div>
</template>
