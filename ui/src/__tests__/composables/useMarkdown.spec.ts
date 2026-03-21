import { describe, it, expect } from 'vitest'
import { renderMarkdown } from '@/composables/useMarkdown'

describe('renderMarkdown', () => {
  describe('basic rendering', () => {
    it('should render bold text', () => {
      const result = renderMarkdown('**bold**')
      expect(result).toContain('<strong>bold</strong>')
    })

    it('should render italic text', () => {
      const result = renderMarkdown('*italic*')
      expect(result).toContain('<em>italic</em>')
    })

    it('should render inline code', () => {
      const result = renderMarkdown('`code`')
      expect(result).toContain('<code>code</code>')
    })

    it('should render code blocks', () => {
      const result = renderMarkdown('```\nconst x = 1\n```')
      expect(result).toContain('<pre>')
      expect(result).toContain('<code>')
      expect(result).toContain('const x = 1')
    })

    it('should render unordered lists', () => {
      const result = renderMarkdown('- item 1\n- item 2')
      expect(result).toContain('<ul>')
      expect(result).toContain('<li>item 1</li>')
      expect(result).toContain('<li>item 2</li>')
    })

    it('should render ordered lists', () => {
      const result = renderMarkdown('1. first\n2. second')
      expect(result).toContain('<ol>')
      expect(result).toContain('<li>first</li>')
      expect(result).toContain('<li>second</li>')
    })

    it('should render headings', () => {
      const result = renderMarkdown('# Heading 1')
      expect(result).toContain('<h1>Heading 1</h1>')
    })

    it('should render blockquotes', () => {
      const result = renderMarkdown('> quote')
      expect(result).toContain('<blockquote>')
      expect(result).toContain('quote')
    })

    it('should render line breaks', () => {
      const result = renderMarkdown('line1\nline2')
      expect(result).toContain('<br>')
    })
  })

  describe('link handling', () => {
    it('should render links with target=_blank', () => {
      const result = renderMarkdown('[link](https://example.com)')
      expect(result).toContain('target="_blank"')
      expect(result).toContain('rel="noopener noreferrer"')
      expect(result).toContain('href="https://example.com"')
      expect(result).toContain('>link</a>')
    })

    it('should auto-linkify URLs', () => {
      const result = renderMarkdown('Visit https://example.com today')
      expect(result).toContain('href="https://example.com"')
      expect(result).toContain('target="_blank"')
    })
  })

  describe('sanitization', () => {
    it('should strip script tags', () => {
      const result = renderMarkdown('<script>alert("xss")</script>')
      expect(result).not.toContain('<script>')
      expect(result).not.toContain('alert')
    })

    it('should strip onclick handlers', () => {
      const result = renderMarkdown('<a href="#" onclick="alert(1)">click</a>')
      expect(result).not.toContain('onclick')
    })

    it('should allow safe tags', () => {
      const result = renderMarkdown('<strong>bold</strong> <em>italic</em>')
      expect(result).toContain('<strong>bold</strong>')
      expect(result).toContain('<em>italic</em>')
    })

    it('should strip iframe tags', () => {
      const result = renderMarkdown('<iframe src="https://evil.com"></iframe>')
      expect(result).not.toContain('<iframe')
    })
  })

  describe('mentions', () => {
    it('should convert mention syntax to styled spans', () => {
      const result = renderMarkdown('@[John Doe](user123)')
      expect(result).toContain('class="mention"')
      expect(result).toContain('data-mention-id="user123"')
      expect(result).toContain('@John Doe')
    })

    it('should handle multiple mentions', () => {
      const result = renderMarkdown('Hello @[Alice](u1) and @[Bob](u2)')
      expect(result).toContain('data-mention-id="u1"')
      expect(result).toContain('data-mention-id="u2"')
      expect(result).toContain('@Alice')
      expect(result).toContain('@Bob')
    })

    it('should handle mention with surrounding markdown', () => {
      const result = renderMarkdown('**Hey** @[User](uid) check this')
      expect(result).toContain('<strong>Hey</strong>')
      expect(result).toContain('data-mention-id="uid"')
    })
  })

  describe('empty input', () => {
    it('should handle empty string', () => {
      const result = renderMarkdown('')
      expect(result).toBe('')
    })
  })
})
