import { describe, it, expect, beforeEach } from 'vitest'
import { useSnackbar } from '@/composables/useSnackbar'

describe('useSnackbar', () => {
  let snackbar: ReturnType<typeof useSnackbar>

  beforeEach(() => {
    snackbar = useSnackbar()
    snackbar.hideSnackbar()
  })

  describe('initial state', () => {
    it('should start hidden', () => {
      expect(snackbar.state.show).toBe(false)
    })
  })

  describe('showError', () => {
    it('should show snackbar with error color', () => {
      snackbar.showError('Something went wrong')
      expect(snackbar.state.show).toBe(true)
      expect(snackbar.state.text).toBe('Something went wrong')
      expect(snackbar.state.color).toBe('error')
      expect(snackbar.state.timeout).toBe(5000)
    })
  })

  describe('showSuccess', () => {
    it('should show snackbar with success color and shorter timeout', () => {
      snackbar.showSuccess('Done!')
      expect(snackbar.state.show).toBe(true)
      expect(snackbar.state.text).toBe('Done!')
      expect(snackbar.state.color).toBe('success')
      expect(snackbar.state.timeout).toBe(3000)
    })
  })

  describe('showSnackbar', () => {
    it('should use custom color and timeout', () => {
      snackbar.showSnackbar('Info message', 'info', 8000)
      expect(snackbar.state.show).toBe(true)
      expect(snackbar.state.text).toBe('Info message')
      expect(snackbar.state.color).toBe('info')
      expect(snackbar.state.timeout).toBe(8000)
    })

    it('should default to error color and 5000ms timeout', () => {
      snackbar.showSnackbar('Oops')
      expect(snackbar.state.color).toBe('error')
      expect(snackbar.state.timeout).toBe(5000)
    })
  })

  describe('hideSnackbar', () => {
    it('should hide the snackbar', () => {
      snackbar.showError('Error')
      expect(snackbar.state.show).toBe(true)
      snackbar.hideSnackbar()
      expect(snackbar.state.show).toBe(false)
    })
  })

  describe('shared state', () => {
    it('should share state across multiple useSnackbar calls', () => {
      const snackbar1 = useSnackbar()
      const snackbar2 = useSnackbar()
      snackbar1.showSuccess('Shared!')
      expect(snackbar2.state.show).toBe(true)
      expect(snackbar2.state.text).toBe('Shared!')
    })
  })
})
