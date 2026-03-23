import { describe, it, expect, vi, beforeEach, beforeAll } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'
import { mount, flushPromises } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

// Polyfill APIs missing from jsdom
beforeAll(() => {
  global.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver

  if (!global.IntersectionObserver) {
    global.IntersectionObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    } as unknown as typeof IntersectionObserver
  }

  if (!global.cancelAnimationFrame) {
    global.cancelAnimationFrame = vi.fn()
  }
  if (!global.requestAnimationFrame) {
    global.requestAnimationFrame = vi.fn((cb: FrameRequestCallback) => {
      cb(0)
      return 0
    })
  }
})

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
    upload: vi.fn(),
  },
}))

import FilePanel from '@/components/chat/FilePanel.vue'
import { useRoomStore, type FileEntry } from '@/stores/rooms'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

function makeFileEntry(overrides: Partial<FileEntry> = {}): FileEntry {
  return {
    id: 'f1',
    filename: 'document.txt',
    content_type: 'text/plain',
    size: 2048,
    url: '/files/document.txt',
    uploaded_by: 'u1',
    created_at: '2026-03-20T12:00:00Z',
    ...overrides,
  }
}

function mountFilePanel(props = { tenantId: 't1', roomId: 'r1' }) {
  const vuetify = createVuetify({ components, directives })
  return mount(FilePanel, {
    props,
    global: {
      plugins: [vuetify],
    },
  })
}

describe('FilePanel', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  describe('rendering', () => {
    it('should render file list from store', async () => {
      // Pre-populate store, then mock fetchRoomFiles to keep the data
      const files = [
        makeFileEntry({ id: 'f1', filename: 'report.pdf' }),
        makeFileEntry({ id: 'f2', filename: 'photo.png' }),
      ]
      mockApi.get.mockResolvedValue({ items: files })

      const wrapper = mountFilePanel()
      await flushPromises()

      expect(wrapper.text()).toContain('report.pdf')
      expect(wrapper.text()).toContain('photo.png')
    })

    it('should show empty state when no files', async () => {
      mockApi.get.mockResolvedValue({ items: [] })

      const wrapper = mountFilePanel()
      await flushPromises()

      expect(wrapper.text()).toContain('No files in this room')
    })

    it('should show loading indicator when filesLoading is true', async () => {
      // Never resolve the API call so filesLoading stays true
      mockApi.get.mockReturnValue(new Promise(() => {}))

      const wrapper = mountFilePanel()
      await flushPromises()

      const store = useRoomStore()
      // filesLoading should be true because the promise never resolves
      expect(store.filesLoading).toBe(true)
      expect(wrapper.find('.v-progress-circular').exists()).toBe(true)
    })
  })

  describe('search filtering', () => {
    it('should filter files by filename when search is entered', async () => {
      const files = [
        makeFileEntry({ id: 'f1', filename: 'report.pdf' }),
        makeFileEntry({ id: 'f2', filename: 'photo.png' }),
        makeFileEntry({ id: 'f3', filename: 'report-v2.pdf' }),
      ]
      mockApi.get.mockResolvedValue({ items: files })

      const wrapper = mountFilePanel()
      await flushPromises()

      // All files visible initially
      expect(wrapper.text()).toContain('report.pdf')
      expect(wrapper.text()).toContain('photo.png')
      expect(wrapper.text()).toContain('report-v2.pdf')

      // Type in search field
      const searchInput = wrapper.find('input[type="text"]')
      await searchInput.setValue('report')
      await flushPromises()

      expect(wrapper.text()).toContain('report.pdf')
      expect(wrapper.text()).toContain('report-v2.pdf')
      expect(wrapper.text()).not.toContain('photo.png')
    })

    it('should be case-insensitive when filtering', async () => {
      const files = [
        makeFileEntry({ id: 'f1', filename: 'Report.PDF' }),
        makeFileEntry({ id: 'f2', filename: 'other.txt' }),
      ]
      mockApi.get.mockResolvedValue({ items: files })

      const wrapper = mountFilePanel()
      await flushPromises()

      const searchInput = wrapper.find('input[type="text"]')
      await searchInput.setValue('report')
      await flushPromises()

      expect(wrapper.text()).toContain('Report.PDF')
      expect(wrapper.text()).not.toContain('other.txt')
    })
  })

  describe('upload', () => {
    it('should trigger file input click on upload button', async () => {
      mockApi.get.mockResolvedValue({ items: [] })

      const wrapper = mountFilePanel()
      await flushPromises()

      const fileInput = wrapper.find('input[type="file"]')
      const clickSpy = vi.spyOn(fileInput.element as HTMLInputElement, 'click')

      // Find the upload button (mdi-upload icon)
      const uploadIcon = wrapper.find('.mdi-upload')
      expect(uploadIcon.exists()).toBe(true)

      await uploadIcon.trigger('click')

      expect(clickSpy).toHaveBeenCalled()
    })

    it('should call store uploadRoomFile when file is selected', async () => {
      mockApi.get.mockResolvedValue({ items: [] })
      mockApi.upload.mockResolvedValueOnce(makeFileEntry({ id: 'f-new', filename: 'new.txt' }))

      const wrapper = mountFilePanel()
      await flushPromises()

      const fileInput = wrapper.find('input[type="file"]')
      const inputEl = fileInput.element as HTMLInputElement

      // Simulate file selection
      const file = new File(['content'], 'new.txt', { type: 'text/plain' })
      Object.defineProperty(inputEl, 'files', { value: [file], writable: false })
      await fileInput.trigger('change')

      expect(mockApi.upload).toHaveBeenCalledWith(
        '/tenant/t1/room/r1/file/upload',
        expect.any(FormData),
      )
    })
  })

  describe('delete', () => {
    it('should call store deleteRoomFile when delete button is clicked', async () => {
      const files = [makeFileEntry({ id: 'f1', filename: 'test.txt' })]
      mockApi.get.mockResolvedValue({ items: files })
      mockApi.delete.mockResolvedValueOnce({})

      const wrapper = mountFilePanel()
      await flushPromises()

      // Find the delete button (mdi-delete-outline icon)
      const deleteIcon = wrapper.find('.mdi-delete-outline')
      expect(deleteIcon.exists()).toBe(true)

      await deleteIcon.trigger('click')
      await flushPromises()

      expect(mockApi.delete).toHaveBeenCalledWith('/tenant/t1/file/f1')
    })
  })

  describe('icon mapping', () => {
    it('should show image icon for image content types', async () => {
      mockApi.get.mockResolvedValue({ items: [makeFileEntry({ id: 'f1', content_type: 'image/png', filename: 'pic.png' })] })

      const wrapper = mountFilePanel()
      await flushPromises()

      expect(wrapper.find('.mdi-image').exists()).toBe(true)
    })

    it('should show PDF icon for PDF content type', async () => {
      mockApi.get.mockResolvedValue({ items: [makeFileEntry({ id: 'f1', content_type: 'application/pdf', filename: 'doc.pdf' })] })

      const wrapper = mountFilePanel()
      await flushPromises()

      expect(wrapper.find('.mdi-file-pdf-box').exists()).toBe(true)
    })

    it('should show audio icon for audio content types', async () => {
      mockApi.get.mockResolvedValue({ items: [makeFileEntry({ id: 'f1', content_type: 'audio/mp3', filename: 'song.mp3' })] })

      const wrapper = mountFilePanel()
      await flushPromises()

      expect(wrapper.find('.mdi-music-note').exists()).toBe(true)
    })

    it('should show video icon for video content types', async () => {
      mockApi.get.mockResolvedValue({ items: [makeFileEntry({ id: 'f1', content_type: 'video/mp4', filename: 'clip.mp4' })] })

      const wrapper = mountFilePanel()
      await flushPromises()

      expect(wrapper.find('.mdi-video').exists()).toBe(true)
    })

    it('should show generic file icon for unknown content types', async () => {
      mockApi.get.mockResolvedValue({ items: [makeFileEntry({ id: 'f1', content_type: 'application/octet-stream', filename: 'data.bin' })] })

      const wrapper = mountFilePanel()
      await flushPromises()

      expect(wrapper.find('.mdi-file').exists()).toBe(true)
    })
  })
})
