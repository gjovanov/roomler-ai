// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-101 P2c — the viewer's reading of `rc:session.created.features_refused`
 * and the words it shows for each reason.
 */
import { describe, expect, it } from 'vitest'
import {
  FEATURE_REASONS,
  SESSION_FEATURES,
  describeFeatureReason,
  featureRefusal,
  parseFeaturesRefused,
  summarizeRefusals,
} from '@/composables/useSessionFeatures'

describe('parseFeaturesRefused', () => {
  it('keeps the string reasons the server sent', () => {
    expect(parseFeaturesRefused({ files: 'org_policy_off', audio: 'device_policy_off' })).toEqual({
      files: 'org_policy_off',
      audio: 'device_policy_off',
    })
  })

  it('reads an absent or malformed field as nothing refused (an older server)', () => {
    for (const raw of [undefined, null, 'files', 7, ['files'], true]) {
      expect(parseFeaturesRefused(raw)).toEqual({})
    }
  })

  it('drops values that are not reasons, keeps keys and reasons it does not know', () => {
    expect(
      parseFeaturesRefused({ files: 3, clipboard: '', audio: null, screen: 'org_policy_off' }),
    ).toEqual({ screen: 'org_policy_off' })
    expect(parseFeaturesRefused({ files: 'some_future_reason' })).toEqual({
      files: 'some_future_reason',
    })
  })
})

describe('describeFeatureReason', () => {
  it('says something different and specific for every known reason', () => {
    for (const feature of SESSION_FEATURES) {
      const texts = FEATURE_REASONS.map((code) => describeFeatureReason(feature, code))
      expect(new Set(texts).size).toBe(FEATURE_REASONS.length)
      for (const t of texts) expect(t).not.toContain('not available in this session')
    }
  })

  it('names whose decision it was', () => {
    expect(describeFeatureReason('files', 'org_policy_off')).toMatch(/organization/)
    expect(describeFeatureReason('files', 'device_policy_off')).toMatch(/device's owner/)
    expect(describeFeatureReason('clipboard', 'needs_consent')).toMatch(/person at the device/)
    expect(describeFeatureReason('audio', 'device_cannot_ask')).toMatch(/Updating the device/)
    expect(describeFeatureReason('files', 'policy_unavailable')).toMatch(/Reconnect/)
  })

  it('still reads a reason from a newer server as unavailable, with its code', () => {
    expect(describeFeatureReason('files', 'tuesday_only')).toBe(
      'File transfer is not available in this session (tuesday_only).',
    )
  })
})

describe('featureRefusal', () => {
  it('is null for a feature the session kept', () => {
    expect(featureRefusal({}, 'files')).toBeNull()
    expect(featureRefusal({ audio: 'org_policy_off' }, 'files')).toBeNull()
  })

  it('is the reason in words for a refused one', () => {
    expect(featureRefusal({ files: 'org_policy_off' }, 'files')).toBe(
      'Your organization has turned off file transfer for remote sessions.',
    )
  })
})

describe('summarizeRefusals', () => {
  it('is null when nothing was refused', () => {
    expect(summarizeRefusals({})).toBeNull()
    expect(summarizeRefusals({ screen: 'org_policy_off' })).toBeNull()
  })

  it('names what is off, in a fixed order', () => {
    expect(summarizeRefusals({ files: 'org_policy_off' })).toBe(
      'Off for this session: file transfer. Hover a greyed control to see why.',
    )
    expect(
      summarizeRefusals({
        audio: 'device_policy_off',
        clipboard: 'needs_consent',
        files: 'org_policy_off',
      }),
    ).toBe(
      'Off for this session: the clipboard, file transfer and host audio. Hover a greyed control to see why.',
    )
  })
})
