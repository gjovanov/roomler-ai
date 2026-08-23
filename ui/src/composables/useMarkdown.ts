import MarkdownIt from 'markdown-it'
import DOMPurify from 'dompurify'

const md = new MarkdownIt({
  html: true,
  breaks: true,
  linkify: true,
})

// Open links in new tab
const defaultRender =
  md.renderer.rules.link_open ||
  function (tokens, idx, options, _env, self) {
    return self.renderToken(tokens, idx, options)
  }

md.renderer.rules.link_open = function (tokens, idx, options, env, self) {
  tokens[idx].attrSet('target', '_blank')
  tokens[idx].attrSet('rel', 'noopener noreferrer')
  return defaultRender(tokens, idx, options, env, self)
}

const ALLOWED_TAGS = [
  'p',
  'br',
  'strong',
  'em',
  'u',
  's',
  'del',
  'code',
  'pre',
  'blockquote',
  'ul',
  'ol',
  'li',
  'a',
  'img',
  'span',
  'h1',
  'h2',
  'h3',
  'h4',
  'h5',
  'h6',
  'hr',
  'table',
  'thead',
  'tbody',
  'tr',
  'th',
  'td',
]

// NOTE: `style` is deliberately NOT allowed. Author-controlled inline CSS on
// message content lets a chat message paint a full-viewport `position:fixed`
// overlay inside the trusted origin (credential-phishing/clickjacking that no
// framing header stops) and fire a render-time `background:url()` beacon on
// every viewer. Message formatting needs none of it.
const ALLOWED_ATTR = ['href', 'target', 'rel', 'src', 'alt', 'title', 'class', 'data-mention-id', 'width', 'height']

// HTML-escape untrusted text before it is interpolated into a raw HTML string.
// Without this a mention's label/id can break out of the attribute/tag (e.g.
// `@[x](y"><img src=x onerror=…>)`). DOMPurify below is the backstop, but the
// safety of this string-concatenation step must not rest on the allowlist
// happening to exclude event handlers.
function escapeHtml(s: string): string {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

// Convert @[Name](id) mention syntax to styled HTML spans
function preprocessMentions(content: string): string {
  return content.replace(/@\[([^\]]+)\]\(([^)]+)\)/g, (_match, label, id) => {
    return `<span class="mention" data-mention-id="${escapeHtml(id)}">@${escapeHtml(label)}</span>`
  })
}

export function renderMarkdown(content: string): string {
  const processed = preprocessMentions(content)
  const raw = md.render(processed)
  return DOMPurify.sanitize(raw, {
    ALLOWED_TAGS,
    ALLOWED_ATTR,
  })
}
