<template>
  <div class="message-editor">
    <div class="editor-toolbar d-flex align-center ga-1 px-2 py-1">
      <v-btn
        :icon="mode === 'rich' ? 'mdi-format-text' : 'mdi-format-text'"
        size="x-small"
        variant="text"
        :color="mode === 'rich' ? 'primary' : undefined"
        @click="toggleMode"
      >
        <v-tooltip activator="parent" location="top">
          {{ mode === 'minimal' ? 'Rich text mode' : 'Minimal mode' }}
        </v-tooltip>
      </v-btn>

      <v-divider vertical class="mx-1" />

      <template v-if="mode === 'rich'">
        <v-btn
          icon="mdi-format-bold"
          size="x-small"
          variant="text"
          :color="editor?.isActive('bold') ? 'primary' : undefined"
          @click="editor?.chain().focus().toggleBold().run()"
        />
        <v-btn
          icon="mdi-format-italic"
          size="x-small"
          variant="text"
          :color="editor?.isActive('italic') ? 'primary' : undefined"
          @click="editor?.chain().focus().toggleItalic().run()"
        />
        <v-btn
          icon="mdi-format-underline"
          size="x-small"
          variant="text"
          :color="editor?.isActive('underline') ? 'primary' : undefined"
          @click="editor?.chain().focus().toggleUnderline().run()"
        />
        <v-btn
          icon="mdi-format-strikethrough"
          size="x-small"
          variant="text"
          :color="editor?.isActive('strike') ? 'primary' : undefined"
          @click="editor?.chain().focus().toggleStrike().run()"
        />

        <v-divider vertical class="mx-1" />

        <v-btn
          icon="mdi-code-tags"
          size="x-small"
          variant="text"
          :color="editor?.isActive('code') ? 'primary' : undefined"
          @click="editor?.chain().focus().toggleCode().run()"
        />
        <v-btn
          icon="mdi-code-braces"
          size="x-small"
          variant="text"
          :color="editor?.isActive('codeBlock') ? 'primary' : undefined"
          @click="editor?.chain().focus().toggleCodeBlock().run()"
        />

        <v-divider vertical class="mx-1" />

        <v-btn
          icon="mdi-format-list-bulleted"
          size="x-small"
          variant="text"
          :color="editor?.isActive('bulletList') ? 'primary' : undefined"
          @click="editor?.chain().focus().toggleBulletList().run()"
        />
        <v-btn
          icon="mdi-format-list-numbered"
          size="x-small"
          variant="text"
          :color="editor?.isActive('orderedList') ? 'primary' : undefined"
          @click="editor?.chain().focus().toggleOrderedList().run()"
        />
        <v-btn
          icon="mdi-format-quote-close"
          size="x-small"
          variant="text"
          :color="editor?.isActive('blockquote') ? 'primary' : undefined"
          @click="editor?.chain().focus().toggleBlockquote().run()"
        />

        <v-divider vertical class="mx-1" />

        <v-btn
          icon="mdi-link"
          size="x-small"
          variant="text"
          :color="editor?.isActive('link') ? 'primary' : undefined"
          @click="toggleLink"
        />
      </template>

      <v-spacer />

      <v-btn
        icon="mdi-paperclip"
        size="x-small"
        variant="text"
        @click="triggerFileInput"
      />
      <v-btn
        icon="mdi-emoticon-outline"
        size="x-small"
        variant="text"
        @click="$emit('open-emoji-picker')"
      />
      <v-btn
        icon="mdi-gif"
        size="x-small"
        variant="text"
        @click="$emit('open-giphy-picker')"
      />
      <v-btn
        v-if="mode === 'rich'"
        icon="mdi-send"
        size="x-small"
        variant="text"
        color="primary"
        @click="handleSend"
      />
    </div>

    <editor-content :editor="editor" class="editor-content" />

    <!-- Hidden file input for attachments -->
    <input
      ref="fileInputRef"
      type="file"
      multiple
      style="display: none"
      @change="handleFileSelect"
    />
  </div>
</template>

<script setup lang="ts">
import { onBeforeUnmount, watch, ref as vueRef } from 'vue'
import { useEditor, EditorContent, VueRenderer } from '@tiptap/vue-3'
import StarterKit from '@tiptap/starter-kit'
import Placeholder from '@tiptap/extension-placeholder'
import Link from '@tiptap/extension-link'
import Underline from '@tiptap/extension-underline'
import Mention from '@tiptap/extension-mention'
import { Markdown } from 'tiptap-markdown'
import tippy, { type Instance as TippyInstance } from 'tippy.js'
import MentionList from './MentionList.vue'
import type { MentionItem } from './MentionList.vue'
import { api } from '@/api/client'

type EditorMode = 'minimal' | 'rich'

const props = withDefaults(
  defineProps<{
    placeholder?: string
    initialContent?: string
    members?: MentionItem[]
    tenantId?: string
    roomId?: string
  }>(),
  {
    placeholder: 'Write a message...',
    initialContent: '',
    members: () => [],
    tenantId: '',
    roomId: '',
  },
)

export interface MentionData {
  users: string[]
  everyone: boolean
  here: boolean
}

const emit = defineEmits<{
  send: [content: string, mentions: MentionData, attachmentIds: string[]]
  'open-emoji-picker': []
  'open-giphy-picker': []
}>()

const mode = vueRef<EditorMode>(
  (localStorage.getItem('roomler-editor-mode') as EditorMode) || 'minimal',
)
const pendingAttachmentIds = vueRef<string[]>([])
const fileInputRef = vueRef<HTMLInputElement | null>(null)

function toggleMode() {
  mode.value = mode.value === 'minimal' ? 'rich' : 'minimal'
  localStorage.setItem('roomler-editor-mode', mode.value)
}

function triggerFileInput() {
  fileInputRef.value?.click()
}

async function handleFileSelect(event: Event) {
  const input = event.target as HTMLInputElement
  const files = input.files
  if (!files || !props.tenantId) return

  for (const file of Array.from(files)) {
    const formData = new FormData()
    formData.append('file', file)
    if (props.roomId) formData.append('room_id', props.roomId)

    try {
      const result = await api.upload<{ id: string; filename: string; url: string }>(
        `/tenant/${props.tenantId}/file`,
        formData,
      )
      pendingAttachmentIds.value.push(result.id)

      // Insert image or file reference into editor
      if (file.type.startsWith('image/')) {
        editor.value?.chain().focus().insertContent(`![${file.name}](${result.url})`).run()
      } else {
        editor.value?.chain().focus().insertContent(`[${file.name}](${result.url})`).run()
      }
    } catch (err) {
      console.error('File upload failed:', err)
    }
  }

  // Reset input
  input.value = ''
}

const mentionItems = vueRef<MentionItem[]>([])

const editor = useEditor({
  content: props.initialContent,
  extensions: [
    StarterKit,
    Placeholder.configure({ placeholder: props.placeholder }),
    Link.configure({ openOnClick: false, autolink: true }),
    Underline,
    Markdown.configure({ html: false, transformPastedText: true }),
    Mention.configure({
      HTMLAttributes: {
        class: 'mention',
      },
      suggestion: {
        items: ({ query }: { query: string }) => {
          const q = query.toLowerCase()
          const specialItems: MentionItem[] = [
            { id: '__everyone__', display_name: 'everyone' },
            { id: '__here__', display_name: 'here' },
          ]
          const all = [...specialItems, ...props.members]
          if (!q) return all.slice(0, 10)
          return all
            .filter((item) =>
              (item.display_name || '').toLowerCase().includes(q) ||
              (item.username || '').toLowerCase().includes(q),
            )
            .slice(0, 10)
        },
        render: () => {
          let component: VueRenderer
          let popup: TippyInstance[]

          return {
            onStart: (renderProps: Record<string, unknown>) => {
              component = new VueRenderer(MentionList, {
                props: renderProps,
                editor: renderProps.editor as never,
              })

              if (!renderProps.clientRect) return

              popup = tippy('body', {
                getReferenceClientRect: renderProps.clientRect as () => DOMRect,
                appendTo: () => document.body,
                content: component.element,
                showOnCreate: true,
                interactive: true,
                trigger: 'manual',
                placement: 'bottom-start',
              })
            },
            onUpdate(renderProps: Record<string, unknown>) {
              component?.updateProps(renderProps)
              if (renderProps.clientRect) {
                popup?.[0]?.setProps({
                  getReferenceClientRect: renderProps.clientRect as () => DOMRect,
                })
              }
            },
            onKeyDown(renderProps: { event: KeyboardEvent }) {
              if (renderProps.event.key === 'Escape') {
                popup?.[0]?.hide()
                return true
              }
              return (component?.ref as unknown as { onKeyDown: (props: { event: KeyboardEvent }) => boolean })?.onKeyDown(renderProps)
            },
            onExit() {
              popup?.[0]?.destroy()
              component?.destroy()
            },
          }
        },
      },
    }),
  ],
  editorProps: {
    handleKeyDown(_view, event) {
      // In minimal mode: Enter sends, Shift+Enter inserts newline
      // In rich mode: Enter inserts newline (TipTap default), no override
      if (mode.value === 'minimal' && event.key === 'Enter' && !event.shiftKey) {
        event.preventDefault()
        handleSend()
        return true
      }
      return false
    },
  },
})

watch(
  () => props.initialContent,
  (val) => {
    if (editor.value && val !== getMarkdown()) {
      editor.value.commands.setContent(val)
    }
  },
)

watch(
  () => props.members,
  (val) => { mentionItems.value = val },
  { immediate: true },
)

function getMarkdown(): string {
  if (!editor.value) return ''
  return editor.value.storage.markdown.getMarkdown()
}

function extractMentions(): MentionData {
  const result: MentionData = { users: [], everyone: false, here: false }
  if (!editor.value) return result

  const json = editor.value.getJSON()
  function walk(node: Record<string, unknown>) {
    if (node.type === 'mention' && node.attrs) {
      const attrs = node.attrs as { id?: string }
      if (attrs.id === '__everyone__') {
        result.everyone = true
      } else if (attrs.id === '__here__') {
        result.here = true
      } else if (attrs.id && !result.users.includes(attrs.id)) {
        result.users.push(attrs.id)
      }
    }
    if (Array.isArray(node.content)) {
      for (const child of node.content) {
        walk(child as Record<string, unknown>)
      }
    }
  }
  walk(json as Record<string, unknown>)
  return result
}

function handleSend() {
  const content = getMarkdown().trim()
  if (!content && pendingAttachmentIds.value.length === 0) return
  const mentions = extractMentions()
  emit('send', content || '', mentions, [...pendingAttachmentIds.value])
  pendingAttachmentIds.value = []
  editor.value?.commands.clearContent(true)
}

function toggleLink() {
  if (!editor.value) return
  if (editor.value.isActive('link')) {
    editor.value.chain().focus().unsetLink().run()
  } else {
    const url = window.prompt('URL')
    if (url) {
      editor.value.chain().focus().setLink({ href: url }).run()
    }
  }
}

function insertContent(text: string) {
  editor.value?.chain().focus().insertContent(text).run()
}

function setContent(markdown: string) {
  editor.value?.commands.setContent(markdown)
}

function focus() {
  editor.value?.commands.focus()
}

defineExpose({ insertContent, setContent, focus })

onBeforeUnmount(() => {
  editor.value?.destroy()
})
</script>

<style scoped>
.message-editor {
  border: 1px solid rgba(var(--v-theme-on-surface), 0.2);
  border-radius: 8px;
  overflow: hidden;
}
.editor-toolbar {
  border-bottom: 1px solid rgba(var(--v-theme-on-surface), 0.08);
  background: rgba(var(--v-theme-on-surface), 0.02);
}
.editor-content {
  min-height: 40px;
  max-height: 200px;
  overflow-y: auto;
  padding: 8px 12px;
}
.editor-content :deep(.tiptap) {
  outline: none;
  min-height: 24px;
}
.editor-content :deep(.tiptap p.is-editor-empty:first-child::before) {
  content: attr(data-placeholder);
  float: left;
  color: rgba(var(--v-theme-on-surface), 0.35);
  pointer-events: none;
  height: 0;
}
.editor-content :deep(.tiptap p) {
  margin: 0;
}
.editor-content :deep(.tiptap code) {
  background: rgba(var(--v-theme-on-surface), 0.08);
  border-radius: 3px;
  padding: 1px 4px;
  font-size: 0.85em;
}
.editor-content :deep(.tiptap pre) {
  background: rgba(var(--v-theme-on-surface), 0.08);
  border-radius: 4px;
  padding: 8px 12px;
  margin: 4px 0;
}
.editor-content :deep(.tiptap blockquote) {
  border-left: 3px solid rgba(var(--v-theme-primary), 0.5);
  padding-left: 12px;
  margin: 4px 0;
}
.editor-content :deep(.mention) {
  color: rgb(var(--v-theme-primary));
  background: rgba(var(--v-theme-primary), 0.1);
  border-radius: 4px;
  padding: 1px 4px;
  font-weight: 500;
}
</style>
