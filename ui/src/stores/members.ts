import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

/** A tenant member row from `GET /tenant/{id}/member` (paginated). */
export interface Member {
  id: string
  user_id: string
  nickname?: string | null
  display_name: string
  role_ids: string[]
  joined_at: string
}

interface MembersPage {
  items: Member[]
  total: number
  page: number
  per_page: number
  total_pages: number
}

export const useMembersStore = defineStore('members', () => {
  const items = ref<Member[]>([])
  const total = ref(0)
  const page = ref(1)
  const perPage = ref(25)
  const totalPages = ref(1)
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function fetchMembers(tenantId: string, toPage = 1) {
    loading.value = true
    error.value = null
    try {
      const resp = await api.get<MembersPage>(
        `/tenant/${tenantId}/member?page=${toPage}&per_page=${perPage.value}`,
      )
      items.value = resp.items
      total.value = resp.total
      page.value = resp.page
      totalPages.value = resp.total_pages
    } catch (e) {
      error.value = (e as Error).message
      items.value = []
    } finally {
      loading.value = false
    }
  }

  /**
   * Local mirror of a server-side role assign/unassign — the role store owns
   * the API calls; this keeps the loaded member rows honest without a refetch.
   */
  function setMemberRole(userId: string, roleId: string, present: boolean) {
    const member = items.value.find((m) => m.user_id === userId)
    if (!member) return
    const has = member.role_ids.includes(roleId)
    if (present && !has) member.role_ids.push(roleId)
    if (!present && has) member.role_ids = member.role_ids.filter((r) => r !== roleId)
  }

  return {
    items,
    total,
    page,
    perPage,
    totalPages,
    loading,
    error,
    fetchMembers,
    setMemberRole,
  }
})
