import { describe, it, expect } from 'vitest'
import { useValidation } from '@/composables/useValidation'

describe('useValidation', () => {
  const { rules } = useValidation()

  describe('required', () => {
    it('should pass for non-empty string', () => {
      expect(rules.required('hello')).toBe(true)
    })

    it('should pass for number 0', () => {
      expect(rules.required(0)).toBe(true)
    })

    it('should pass for truthy values', () => {
      expect(rules.required(1)).toBe(true)
      expect(rules.required(true)).toBe(true)
      expect(rules.required([1])).toBe(true)
    })

    it('should fail for empty string', () => {
      expect(rules.required('')).toBe('This field is required')
    })

    it('should fail for null', () => {
      expect(rules.required(null)).toBe('This field is required')
    })

    it('should fail for undefined', () => {
      expect(rules.required(undefined)).toBe('This field is required')
    })

    it('should fail for false', () => {
      expect(rules.required(false)).toBe('This field is required')
    })
  })

  describe('email', () => {
    it('should pass for valid email', () => {
      expect(rules.email('user@example.com')).toBe(true)
    })

    it('should pass for empty string (not required)', () => {
      expect(rules.email('')).toBe(true)
    })

    it('should fail for missing @', () => {
      expect(rules.email('userexample.com')).toBe('Invalid email address')
    })

    it('should fail for missing domain', () => {
      expect(rules.email('user@')).toBe('Invalid email address')
    })

    it('should fail for missing local part', () => {
      expect(rules.email('@example.com')).toBe('Invalid email address')
    })

    it('should fail for spaces', () => {
      expect(rules.email('user @example.com')).toBe('Invalid email address')
    })

    it('should pass for email with dots and plus', () => {
      expect(rules.email('user.name+tag@example.co.uk')).toBe(true)
    })
  })

  describe('minLength', () => {
    it('should return a validator function', () => {
      expect(typeof rules.minLength(3)).toBe('function')
    })

    it('should pass when string meets minimum', () => {
      expect(rules.minLength(3)('abc')).toBe(true)
    })

    it('should pass when string exceeds minimum', () => {
      expect(rules.minLength(3)('abcdef')).toBe(true)
    })

    it('should fail when string is too short', () => {
      expect(rules.minLength(3)('ab')).toBe('Must be at least 3 characters')
    })

    it('should pass for empty string (not required)', () => {
      expect(rules.minLength(3)('')).toBe(true)
    })
  })

  describe('maxLength', () => {
    it('should return a validator function', () => {
      expect(typeof rules.maxLength(5)).toBe('function')
    })

    it('should pass when string is within limit', () => {
      expect(rules.maxLength(5)('abc')).toBe(true)
    })

    it('should pass when string equals limit', () => {
      expect(rules.maxLength(5)('abcde')).toBe(true)
    })

    it('should fail when string exceeds limit', () => {
      expect(rules.maxLength(5)('abcdef')).toBe('Must be at most 5 characters')
    })

    it('should pass for empty string', () => {
      expect(rules.maxLength(5)('')).toBe(true)
    })
  })

  describe('positiveNumber', () => {
    it('should pass for positive number', () => {
      expect(rules.positiveNumber(5)).toBe(true)
    })

    it('should pass for positive string number', () => {
      expect(rules.positiveNumber('10')).toBe(true)
    })

    it('should fail for zero', () => {
      expect(rules.positiveNumber(0)).toBe('Must be a positive number')
    })

    it('should fail for negative number', () => {
      expect(rules.positiveNumber(-1)).toBe('Must be a positive number')
    })

    it('should pass for empty string (not required)', () => {
      expect(rules.positiveNumber('')).toBe(true)
    })

    it('should pass for null (not required)', () => {
      expect(rules.positiveNumber(null)).toBe(true)
    })

    it('should pass for undefined (not required)', () => {
      expect(rules.positiveNumber(undefined)).toBe(true)
    })
  })

  describe('slug', () => {
    it('should pass for valid slug', () => {
      expect(rules.slug('my-project')).toBe(true)
    })

    it('should pass for single word', () => {
      expect(rules.slug('project')).toBe(true)
    })

    it('should pass for numbers', () => {
      expect(rules.slug('project123')).toBe(true)
    })

    it('should pass for empty string (not required)', () => {
      expect(rules.slug('')).toBe(true)
    })

    it('should fail for uppercase', () => {
      expect(rules.slug('My-Project')).toBe('Only lowercase letters, numbers and hyphens')
    })

    it('should fail for spaces', () => {
      expect(rules.slug('my project')).toBe('Only lowercase letters, numbers and hyphens')
    })

    it('should fail for leading hyphen', () => {
      expect(rules.slug('-project')).toBe('Only lowercase letters, numbers and hyphens')
    })

    it('should fail for trailing hyphen', () => {
      expect(rules.slug('project-')).toBe('Only lowercase letters, numbers and hyphens')
    })

    it('should fail for consecutive hyphens', () => {
      expect(rules.slug('my--project')).toBe('Only lowercase letters, numbers and hyphens')
    })

    it('should fail for special characters', () => {
      expect(rules.slug('my_project')).toBe('Only lowercase letters, numbers and hyphens')
    })
  })
})
