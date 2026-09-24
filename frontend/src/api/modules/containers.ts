import type { RequestOptions } from '../request'
import request from '../request'

export interface ContainerSlot {
  id: string
  name: string
  accountId: string | null
  accountName: string | null
  accountEnabled: boolean
  proxyId: string | null
  proxyName: string | null
  hostname: string
  machineId: string
  installationId: string
  timezone: string
  running: boolean
  generation: number
  state: string
  reason: string | null
}

interface Existing { id: string, expectedGeneration: number }
export type ContainerCommand
  = { action: 'create', name: string }
    | (Existing & { action: 'configureProxy', proxyId: string | null })
    | (Existing & { action: 'bind', accountId: string | null })
    | (Existing & { action: 'start' | 'stop' | 'delete' })

export function getContainers(options: RequestOptions = {}) {
  return request<{ globalEnabled: boolean, items: ContainerSlot[] }>({
    url: '/api/admin/containers',
    method: 'GET',
    ...options,
  })
}

export function updateContainer(data: ContainerCommand, options: RequestOptions = {}) {
  return request<{ saved: boolean }>({
    url: '/api/admin/containers/update',
    method: 'POST',
    data,
    ...options,
  })
}
